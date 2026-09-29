use std::cell::Cell;
use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};
use std::time::Duration;

use async_tungstenite::WebSocketStream;
use async_tungstenite::tungstenite::Error as WebSocketError;
use editor::Editor;
use futures::channel::mpsc::{self, UnboundedSender, unbounded};
use futures::channel::oneshot;
use futures::{
    AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, FutureExt as _, StreamExt as _,
};
use gpui::{
    AppContext as _, Entity, Focusable as _, TestAppContext, UpdateGlobal as _, WindowHandle,
};
use ide_query::{
    AssistSource, CodeAnnotation, CodeDisplay, CodeHighlight, CodeTour, DisplayOutcome, LineRange,
    QueryLocation, SkippedDisplay, SymbolHit,
};
use language::{Diagnostic, DiagnosticEntry, DiagnosticSeverity, DiagnosticSourceKind, PointUtf16};
use lsp::LanguageServerId;
use project::{FakeFs, Fs as _, Project};
use serde_json::{Value, json};
use smol::net::{TcpListener, TcpStream};
use text::Unclipped;
use util::path_mapping::SandboxPaths;
use util::{path, uri};
use workspace::{MultiWorkspace, Workspace};

use crate::protocol::{self, LockFileContents};
use crate::status_item::Shown;
use crate::{
    IdeServer, IdeServerDiff, IdeServerStatusItem, IdentityPathMapper, McpEndpoint, PathMapper,
    SandboxIdeSessionHandle, ToolHost as _, WslPathMapper, sandbox_client, tools, transport,
};

// ---------------------------------------------------------------------------
// ロックファイル
// ---------------------------------------------------------------------------

#[test]
fn writes_the_lock_file_fields_the_cli_reads() {
    let contents = LockFileContents {
        pid: 4242,
        workspace_folders: vec!["I:\\zed-jp-llm".to_string()],
        ide_name: "Zed".to_string(),
        transport: "ws".to_string(),
        running_in_windows: true,
        auth_token: "token".to_string(),
    };
    let json: Value = serde_json::from_str(&contents.to_json().expect("the lock file serializes"))
        .expect("the lock file is valid JSON");

    // CLI 2.1.261 の `Pen()` が読むキーはこの 6 つ。ポートはファイル名から取るので
    // JSON には入れない。
    let object = json.as_object().expect("the lock file is a JSON object");
    let mut keys = object.keys().cloned().collect::<Vec<_>>();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "authToken".to_string(),
            "ideName".to_string(),
            "pid".to_string(),
            "runningInWindows".to_string(),
            "transport".to_string(),
            "workspaceFolders".to_string(),
        ]
    );
    assert_eq!(json["transport"], json!("ws"));
    assert_eq!(json["ideName"], json!("Zed"));
    assert_eq!(json["runningInWindows"], json!(true));
    assert_eq!(json["workspaceFolders"], json!(["I:\\zed-jp-llm"]));
}

#[test]
fn reads_a_lock_file_written_by_another_ide() {
    let raw = r#"{"pid":25180,"workspaceFolders":["i:\\zed-jp-llm"],"ideName":"Antigravity IDE","transport":"ws","runningInWindows":true,"authToken":"redacted"}"#;
    let contents = LockFileContents::from_json(raw).expect("the lock file parses");
    assert_eq!(contents.pid, 25180);
    assert_eq!(contents.ide_name, "Antigravity IDE");
    assert_eq!(contents.transport, "ws");
    assert!(contents.running_in_windows);
    assert_eq!(
        contents.workspace_folders,
        vec!["i:\\zed-jp-llm".to_string()]
    );
}

#[test]
fn puts_the_port_in_the_lock_file_name() {
    let directory = std::path::Path::new("/home/zed/.claude/ide");
    assert_eq!(
        crate::lock_file_path(directory, 39254),
        directory.join("39254.lock")
    );
}

// ---------------------------------------------------------------------------
// トランスポート (loopback の実ソケット)
// ---------------------------------------------------------------------------

/// テスト用サーバー: 1 本受けて `serve_connection` に渡し、受信した JSON-RPC を
/// `responder` に通した結果を返す。
fn spawn_test_server(
    listener: TcpListener,
    auth_token: &str,
    responder: impl Fn(String) -> Option<String> + Send + 'static,
) -> smol::Task<anyhow::Result<()>> {
    let auth_token: Arc<str> = auth_token.into();
    smol::spawn(async move {
        let (stream, _peer) = listener.accept().await?;
        let (incoming_tx, mut incoming_rx) = mpsc::channel::<String>(0);
        let (outbox, outgoing) = transport::outbox();
        let (route_tx, _route_rx) = oneshot::channel();
        let pump = smol::spawn(async move {
            while let Some(text) = incoming_rx.next().await {
                if let Some(response) = responder(text) {
                    outbox.send_response(response, None);
                }
            }
            outbox.finish();
        });
        let result = transport::serve_connection(
            stream,
            auth_token,
            |_| {
                Ok(transport::Admitted {
                    limits: transport::MessageLimits::same(transport::HOST_MAX_MESSAGE_BYTES),
                    hold: None,
                })
            },
            incoming_tx,
            outgoing,
            route_tx,
            futures::future::pending(),
        )
        .await;
        drop(pump);
        result
    })
}

#[test]
fn rejects_an_http_request_with_the_wrong_token() {
    smol::block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binding the test listener");
        let port = listener
            .local_addr()
            .expect("the listener has an address")
            .port();
        let server = spawn_test_server(listener, "good-token", |_| None);

        let mut client = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connecting to the test server");
        client
            .write_all(
                b"POST /mcp HTTP/1.1\r\n\
                  Host: 127.0.0.1\r\n\
                  Authorization: Bearer wrong-token\r\n\
                  Content-Length: 0\r\n\r\n",
            )
            .await
            .expect("writing the request");
        let mut response = String::new();
        client
            .read_to_string(&mut response)
            .await
            .expect("reading the response");

        assert!(
            response.starts_with("HTTP/1.1 401 Unauthorized"),
            "expected a 401, got {response:?}"
        );
        assert!(
            server.await.is_err(),
            "the server should report the rejected connection"
        );
    });
}

/// ADR-F56: ホストのサーバーの loopback の口は、サーバー自身のトークンと登録されたクライアントの
/// トークンを受け、どのクライアントの接続かをトークンから決める (URL の `client=` は見ない)。
/// 登録を外したクライアントのトークンは 401。
#[test]
fn the_host_tells_a_client_by_the_token_it_presents() {
    smol::block_on(async {
        let client_tokens = transport::ClientTokens::default();
        client_tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert("client-token".to_string(), "client-a".to_string());
        let credentials = transport::Credentials::Host {
            server_token: "server-token".into(),
            client_tokens: client_tokens.clone(),
        };
        let post = |token: &'static str| {
            let credentials = credentials.clone();
            async move {
                let listener = TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("binding the test listener");
                let port = listener
                    .local_addr()
                    .expect("the listener has an address")
                    .port();
                let (route_tx, route_rx) = oneshot::channel();
                let server = smol::spawn(async move {
                    let (stream, _peer) = listener.accept().await?;
                    let (incoming_tx, mut incoming_rx) = mpsc::channel::<String>(0);
                    let (outbox, outgoing) = transport::outbox();
                    let pump = smol::spawn(async move {
                        while incoming_rx.next().await.is_some() {
                            outbox.send_response(
                                r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_string(),
                                None,
                            );
                        }
                        outbox.finish();
                    });
                    let result = transport::serve_connection(
                        stream,
                        credentials,
                        |_| {
                            Ok(transport::Admitted {
                                limits: transport::MessageLimits::same(
                                    transport::HOST_MAX_MESSAGE_BYTES,
                                ),
                                hold: None,
                            })
                        },
                        incoming_tx,
                        outgoing,
                        route_tx,
                        futures::future::pending(),
                    )
                    .await;
                    drop(pump);
                    result
                });
                let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
                let mut client = TcpStream::connect(("127.0.0.1", port))
                    .await
                    .expect("connecting to the test server");
                client
                    .write_all(
                        format!(
                            "POST /mcp/zed?client=client-b HTTP/1.1\r\n\
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
                client
                    .read_to_string(&mut response)
                    .await
                    .expect("reading the response");
                server.await.ok();
                (response, route_rx.await.ok())
            }
        };

        let (response, route) = post("client-token").await;
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response:?}");
        assert_eq!(
            route.and_then(|route| route.client).as_deref(),
            Some("client-a"),
            "the token's client, not the one the URL names"
        );

        let (response, route) = post("server-token").await;
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response:?}");
        assert_eq!(route.map(|route| route.client), Some(None));

        client_tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove("client-token");
        let (response, route) = post("client-token").await;
        assert!(
            response.starts_with("HTTP/1.1 401 Unauthorized"),
            "{response:?}"
        );
        assert!(route.is_none());
    });
}

#[test]
fn answers_a_streamable_http_mcp_request() {
    smol::block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binding the test listener");
        let port = listener
            .local_addr()
            .expect("the listener has an address")
            .port();
        let server = spawn_test_server(listener, "good-token", |request| {
            assert!(request.contains("\"method\":\"ping\""));
            Some(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_string())
        });

        let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let mut client = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connecting to the test server");
        client
            .write_all(
                format!(
                    "POST /mcp HTTP/1.1\r\n\
                     Host: 127.0.0.1\r\n\
                     Authorization: Bearer good-token\r\n\
                     Content-Type: application/json\r\n\
                     Content-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .expect("writing the request");
        let mut response = String::new();
        client
            .read_to_string(&mut response)
            .await
            .expect("reading the response");

        assert!(
            response.starts_with("HTTP/1.1 200 OK"),
            "expected a 200, got {response:?}"
        );
        assert!(
            response.ends_with(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#),
            "the JSON-RPC response should be the body, got {response:?}"
        );
        server.await.expect("the server finished cleanly");
    });
}

#[test]
fn completes_the_claude_code_websocket_handshake() {
    use async_tungstenite::tungstenite::Message;
    use async_tungstenite::tungstenite::http::Request;

    smol::block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binding the test listener");
        let port = listener
            .local_addr()
            .expect("the listener has an address")
            .port();
        let server = spawn_test_server(listener, "good-token", |request| {
            assert!(request.contains("\"method\":\"tools/list\""));
            Some(r#"{"jsonrpc":"2.0","id":7,"result":{"tools":[]}}"#.to_string())
        });

        let client = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connecting to the test server");
        let request = Request::builder()
            .method("GET")
            .uri(format!("ws://127.0.0.1:{port}/"))
            .header("Host", format!("127.0.0.1:{port}"))
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
            .header("Sec-WebSocket-Protocol", "mcp")
            .header(transport::CLAUDE_CODE_AUTH_HEADER, "good-token")
            .body(())
            .expect("building the upgrade request");

        let (mut socket, response) = async_tungstenite::client_async(request, client)
            .await
            .expect("the WebSocket handshake succeeds");
        assert_eq!(
            response
                .headers()
                .get("sec-websocket-protocol")
                .and_then(|value| value.to_str().ok()),
            Some("mcp"),
            "the server must echo the mcp subprotocol"
        );

        socket
            .send(Message::text(
                r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#,
            ))
            .await
            .expect("sending a request");
        let reply = socket
            .next()
            .await
            .expect("a reply arrives")
            .expect("the reply is a valid frame");
        assert_eq!(
            reply.into_text().expect("the reply is text").as_str(),
            r#"{"jsonrpc":"2.0","id":7,"result":{"tools":[]}}"#
        );

        socket.close(None).await.ok();
        drop(socket);
        server.await.ok();
    });
}

// ---------------------------------------------------------------------------
// JSON-RPC のディスパッチ (ワークスペース付き)
// ---------------------------------------------------------------------------

fn init_test(cx: &mut TestAppContext) {
    cx.update(|cx| {
        // `AppState::test` が `SettingsStore` とテーマを用意する。`editor::init` は
        // ファイルを開いたときに `Editor` をプロジェクトアイテムとして解決させるため。
        workspace::AppState::test(cx);
        editor::init(cx);
    });
}

async fn test_workspace(cx: &mut TestAppContext) -> (Entity<Workspace>, Entity<IdeServer>) {
    let (_fs, workspace, server) = test_workspace_with_fs(cx).await;
    (workspace, server)
}

async fn test_workspace_with_fs(
    cx: &mut TestAppContext,
) -> (Arc<FakeFs>, Entity<Workspace>, Entity<IdeServer>) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/test"),
        json!({ "main.rs": "fn main() {\n    println!(\"hi\");\n}\n" }),
    )
    .await;
    let project = Project::test(fs.clone(), [path!("/test").as_ref()], cx).await;
    let weak_project = project.downgrade();
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
    let server = cx.new(|cx| IdeServer::new(workspace.downgrade(), weak_project, cx));
    (fs, workspace, server)
}

fn lock_files(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "lock")
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

/// 実機で `~/.claude/ide` にロックが 2 本残った件 (同じ pid・同じフォルダー) の回帰。
/// 原因は bind が終わるまで `running` が空なので、そのあいだに届いた設定変更が
/// 2 本目のサーバーを立ち上げ、入れ替えられた 1 本目のロックが誰にも消されずに
/// 残っていたこと。
#[gpui::test]
async fn keeps_one_lock_file_and_removes_it_when_the_workspace_goes_away(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let lock_directory = tempfile::tempdir().expect("a temporary lock directory");
    init_test(cx);
    cx.update(|cx| {
        crate::init(cx);
        crate::set_lock_directory(lock_directory.path().to_path_buf(), cx);
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/test"), json!({ "main.rs": "fn main() {}\n" }))
        .await;
    let project = Project::test(fs, [path!("/test").as_ref()], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

    cx.update(|cx| {
        settings::SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.ide_server.get_or_insert_default().enabled = Some(true);
            });
        });
    });
    // bind が終わる前に、無関係な設定変更でもう一度 `SettingsStore` を揺らす。ここで
    // 2 本目が立ち上がると、入れ替えられた 1 本目のロックが残る。
    cx.update(|cx| {
        settings::SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings
                    .terminal
                    .get_or_insert_default()
                    .session_log
                    .get_or_insert_default()
                    .auto = Some(true);
            });
        });
    });
    cx.run_until_parked();

    let locks = lock_files(lock_directory.path());
    assert_eq!(
        locks.len(),
        1,
        "expected exactly one lock file, got {locks:?}"
    );
    let contents = LockFileContents::from_json(
        &std::fs::read_to_string(&locks[0]).expect("the lock file is readable"),
    )
    .expect("the lock file parses");
    assert_eq!(contents.pid, std::process::id());
    assert_eq!(
        contents.workspace_folders,
        vec![path!("/test").to_string()],
        "a local workspace writes its own folders"
    );

    window
        .update(cx, |_, window, _| window.remove_window())
        .expect("the window is still open");
    cx.run_until_parked();

    assert_eq!(
        lock_files(lock_directory.path()),
        Vec::<PathBuf>::new(),
        "releasing the workspace has to take the lock file with it"
    );
}

async fn request(server: &Entity<IdeServer>, message: Value, cx: &mut TestAppContext) -> Value {
    let mut async_cx = cx.to_async();
    let raw = crate::handle_message(
        server,
        &serde_json::to_string(&message).expect("the request serializes"),
        tools::ConnectionId::next(),
        &mut async_cx,
    )
    .await
    .expect("the request produced a response");
    serde_json::from_str(&raw).expect("the response is valid JSON")
}

/// `tools/call` の最初のテキスト内容を JSON として読み直す。
fn first_text_as_json(response: &Value) -> Value {
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .expect("the tool returned a text content block");
    serde_json::from_str(text).expect("the tool's text content is JSON")
}

#[gpui::test]
async fn negotiates_initialize_and_lists_tools(cx: &mut TestAppContext) {
    let (_workspace, server) = test_workspace(cx).await;

    let response = request(
        &server,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "claude-code", "version": "2.1.261" }
            }
        }),
        cx,
    )
    .await;
    assert_eq!(response["result"]["protocolVersion"], json!("2025-06-18"));
    assert_eq!(response["result"]["serverInfo"]["name"], json!("Zed"));

    // 知らないバージョンを言われたら、こちらの最新で答える。
    let response = request(
        &server,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "initialize",
            "params": { "protocolVersion": "1999-01-01" }
        }),
        cx,
    )
    .await;
    assert_eq!(
        response["result"]["protocolVersion"],
        json!(context_server::types::LATEST_PROTOCOL_VERSION)
    );

    let response = request(
        &server,
        json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" }),
        cx,
    )
    .await;
    let names = response["result"]["tools"]
        .as_array()
        .expect("tools is an array")
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_string))
        .collect::<Vec<_>>();
    for expected in [
        "getWorkspaceFolders",
        "getOpenEditors",
        "getCurrentSelection",
        "getLatestSelection",
        "getDiagnostics",
        "openFile",
        "checkDocumentDirty",
        "saveDocument",
        "close_tab",
        "closeAllDiffTabs",
        "openDiff",
    ] {
        assert!(
            names.iter().any(|name| name == expected),
            "missing tool {expected}"
        );
    }

    let response = request(
        &server,
        json!({ "jsonrpc": "2.0", "id": 4, "method": "nope/nope" }),
        cx,
    )
    .await;
    assert_eq!(
        response["error"]["code"],
        json!(crate::protocol::METHOD_NOT_FOUND)
    );
}

#[gpui::test]
async fn reports_workspace_folders(cx: &mut TestAppContext) {
    let (_workspace, server) = test_workspace(cx).await;

    let response = request(
        &server,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "getWorkspaceFolders", "arguments": {} }
        }),
        cx,
    )
    .await;
    assert_eq!(response["result"]["isError"], json!(false));

    let folders = first_text_as_json(&response);
    let listed = folders["folders"]
        .as_array()
        .expect("folders is an array")
        .iter()
        .filter_map(|folder| folder["path"].as_str().map(str::to_string))
        .collect::<Vec<_>>();
    assert_eq!(listed, vec![path!("/test").to_string()]);
}

#[gpui::test]
async fn opens_a_file_and_reports_the_selection(cx: &mut TestAppContext) {
    let (_workspace, server) = test_workspace(cx).await;

    let response = request(
        &server,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "openFile",
                "arguments": {
                    "filePath": path!("/test/main.rs"),
                    "startLine": 1,
                    "startColumn": 4,
                    "endLine": 1,
                    "endColumn": 11
                }
            }
        }),
        cx,
    )
    .await;
    assert_eq!(response["result"]["isError"], json!(false), "{response}");
    assert_eq!(first_text_as_json(&response)["success"], json!(true));
    cx.run_until_parked();

    let response = request(
        &server,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "getCurrentSelection", "arguments": {} }
        }),
        cx,
    )
    .await;
    let selection = first_text_as_json(&response);
    assert_eq!(selection["success"], json!(true));
    assert_eq!(selection["text"], json!("println"));
    assert_eq!(selection["selection"]["start"]["line"], json!(1));
    assert_eq!(selection["selection"]["start"]["character"], json!(4));
    assert_eq!(selection["selection"]["end"]["character"], json!(11));
    assert_eq!(selection["selection"]["isEmpty"], json!(false));

    // 一度観測した選択範囲は `getLatestSelection` にも残る。
    let response = request(
        &server,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "getOpenEditors", "arguments": {} }
        }),
        cx,
    )
    .await;
    let tabs = first_text_as_json(&response);
    let tabs = tabs["tabs"].as_array().expect("tabs is an array");
    assert_eq!(tabs.len(), 1);
    assert_eq!(tabs[0]["label"], json!("main.rs"));
    assert_eq!(tabs[0]["isActive"], json!(true));
    assert_eq!(tabs[0]["isDirty"], json!(false));
}

// ---------------------------------------------------------------------------
// 第 2 段の配線 (ステータスバー / 右クリック / ACP)
// ---------------------------------------------------------------------------

#[test]
fn writes_an_mcp_snippet_other_agents_can_paste() {
    let endpoint = McpEndpoint {
        url: "http://127.0.0.1:39254/mcp".to_string(),
        authorization: "Bearer secret".to_string(),
    };
    let json: Value = serde_json::from_str(
        &endpoint
            .to_configuration_json()
            .expect("the snippet serializes"),
    )
    .expect("the snippet is valid JSON");

    let server = &json["mcpServers"][crate::MCP_SERVER_NAME];
    assert_eq!(server["type"], json!("http"));
    assert_eq!(server["url"], json!("http://127.0.0.1:39254/mcp"));
    assert_eq!(server["headers"]["Authorization"], json!("Bearer secret"));
}

/// 右クリックの「選択範囲を Claude Code に送る」が渡す値。CLI は 0 始まりの行番号を
/// `@ファイル#L1-L2` に直すので、行全体を選んだときに 1 行余分に見えてはいけない。
#[gpui::test]
async fn reports_the_at_mention_range_of_the_selection(cx: &mut TestAppContext) {
    let (workspace, server) = test_workspace(cx).await;

    let response = request(
        &server,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "openFile",
                "arguments": {
                    "filePath": path!("/test/main.rs"),
                    "startLine": 0,
                    "startColumn": 0,
                    "endLine": 1,
                    "endColumn": 0
                }
            }
        }),
        cx,
    )
    .await;
    assert_eq!(response["result"]["isError"], json!(false), "{response}");
    cx.run_until_parked();

    let editor = workspace
        .read_with(cx, |workspace, cx| {
            workspace
                .active_item(cx)
                .and_then(|item| item.act_as::<Editor>(cx))
        })
        .expect("the file opened in an editor");
    let (file_path, start_line, end_line) = cx
        .update(|cx| tools::at_mention_range(&editor, cx))
        .expect("the editor has a file and a selection");

    // CLI は cwd と突き合わせるので、ワークツリー相対ではなく絶対パスを渡す。
    assert_eq!(file_path, PathBuf::from(path!("/test/main.rs")));
    assert_eq!(start_line, 0);
    assert_eq!(
        end_line, 0,
        "a selection that ends at the start of the next line covers one line"
    );
}

#[gpui::test]
async fn does_not_send_an_at_mention_without_a_connected_cli(cx: &mut TestAppContext) {
    let (_workspace, server) = test_workspace(cx).await;

    let sent = server.update(cx, |server, cx| {
        server.send_at_mention(Path::new(path!("/test/main.rs")), 0, 0, cx)
    });
    assert!(
        !sent,
        "with no WebSocket client attached there is nobody to mention the selection to"
    );
}

// ---------------------------------------------------------------------------
// openDiff
// ---------------------------------------------------------------------------

/// `FILE_SAVED` は「保存した」ではなく「この本文で承認した」という意味で、ディスクへ
/// 書くのは CLI 側の Edit / Write ツールである。IDE が先に書くと、CLI が書き込み直前に
/// する `readFileState` の mtime と内容の照合が落ちて
/// 「File content has changed since it was last read」になる (CLI 2.1.261 の `xeo` /
/// `aio`)。実機でも「採用」のあと CLI 側にだけエラーが出ていた。
#[gpui::test]
async fn accepting_a_diff_reports_the_contents_without_writing_the_file(cx: &mut TestAppContext) {
    let (fs, _workspace, server) = test_workspace_with_fs(cx).await;
    let on_disk_before = fs
        .load(path!("/test/main.rs").as_ref())
        .await
        .expect("the file is readable");

    let message = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "openDiff",
            "arguments": {
                "old_file_path": path!("/test/main.rs"),
                "new_file_path": path!("/test/main.rs"),
                "new_file_contents": "fn main() {}\n",
                "tab_name": "main.rs",
            }
        }
    }))
    .expect("the request serializes");

    // `openDiff` はユーザーの決定を待つので、待っているあいだにこちらから採用する。
    let response = cx.spawn({
        let server = server.clone();
        async move |mut cx| {
            crate::handle_message(&server, &message, tools::ConnectionId::next(), &mut cx).await
        }
    });
    cx.run_until_parked();

    let diff = server
        .update(cx, |server, _| server.open_diffs().oldest_named("main.rs"))
        .expect("the diff tab is registered while the CLI waits");
    diff.update(cx, |diff, cx| diff.accept(cx));
    cx.run_until_parked();

    let response: Value =
        serde_json::from_str(&response.await.expect("openDiff produced a response"))
            .expect("the response is valid JSON");
    let content = &response["result"]["content"];
    assert_eq!(content[0]["text"], json!("FILE_SAVED"));
    assert_eq!(content[1]["text"], json!("fn main() {}\n"));

    assert_eq!(
        fs.load(path!("/test/main.rs").as_ref())
            .await
            .expect("the file is still readable"),
        on_disk_before,
        "the IDE must leave the write to the CLI"
    );
}

// ---------------------------------------------------------------------------
// ロックファイルの書き込み
// ---------------------------------------------------------------------------

fn temporary_files(directory: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(directory)
        .expect("the lock directory is readable")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "tmp"))
        .collect()
}

#[gpui::test]
async fn replaces_an_existing_lock_file(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().expect("a temporary lock directory");
    let lock_path = crate::lock_file_path(directory.path(), 41234);
    std::fs::write(&lock_path, "old").expect("writing the old lock file");

    protocol::write_lock_file(&lock_path, b"new", &cx.executor())
        .await
        .expect("the lock file is replaced");

    assert_eq!(
        std::fs::read_to_string(&lock_path).expect("the lock file is readable"),
        "new"
    );
    assert_eq!(temporary_files(directory.path()), Vec::<PathBuf>::new());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&lock_path)
            .expect("the lock file has metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the auth token is only for the owner");
    }
}

/// ロックの置き場所に `<port>.lock` という名前のリンクを仕込まれても、その先を書き換えない。
#[gpui::test]
async fn does_not_follow_a_symlink_planted_at_the_lock_path(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().expect("a temporary lock directory");
    let victim = directory.path().join("victim.txt");
    std::fs::write(&victim, "original").expect("writing the victim file");
    let lock_path = crate::lock_file_path(directory.path(), 41234);

    #[cfg(unix)]
    let planted = std::os::unix::fs::symlink(&victim, &lock_path);
    #[cfg(windows)]
    let planted = std::os::windows::fs::symlink_file(&victim, &lock_path);
    if let Err(error) = planted {
        // Windows では開発者モードか管理者でないとシンボリックリンクを作れない。
        eprintln!("skipping: could not create a symlink to plant: {error}");
        return;
    }

    protocol::write_lock_file(&lock_path, b"{\"pid\":1}", &cx.executor())
        .await
        .expect("the lock file is written");

    assert_eq!(
        std::fs::read_to_string(&victim).expect("the victim is readable"),
        "original",
        "the symlink's target must not be written"
    );
    let metadata = std::fs::symlink_metadata(&lock_path).expect("the lock file exists");
    assert!(
        metadata.file_type().is_file(),
        "the planted link is replaced by a regular file"
    );
    assert_eq!(
        std::fs::read(&lock_path).expect("the lock file is readable"),
        b"{\"pid\":1}"
    );
    assert_eq!(temporary_files(directory.path()), Vec::<PathBuf>::new());
}

/// 書き込みがリンク先に届かないことは、特権の要らないハードリンクでも確かめられる
/// (シンボリックリンクを作れない Windows の環境でも必ず走る)。
#[gpui::test]
async fn does_not_write_through_a_hard_link_planted_at_the_lock_path(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().expect("a temporary lock directory");
    let victim = directory.path().join("victim.txt");
    std::fs::write(&victim, "original").expect("writing the victim file");
    let lock_path = crate::lock_file_path(directory.path(), 41234);
    std::fs::hard_link(&victim, &lock_path).expect("planting a hard link");

    protocol::write_lock_file(&lock_path, b"{\"pid\":1}", &cx.executor())
        .await
        .expect("the lock file is written");

    assert_eq!(
        std::fs::read_to_string(&victim).expect("the victim is readable"),
        "original",
        "the planted file must not be written"
    );
    assert_eq!(
        std::fs::read(&lock_path).expect("the lock file is readable"),
        b"{\"pid\":1}"
    );
    assert_eq!(temporary_files(directory.path()), Vec::<PathBuf>::new());
}

#[test]
fn writes_the_sandbox_lock_file_the_cli_reads() {
    let bytes =
        crate::sandbox_lock_file_contents("sandbox-token", &["/mnt/i/zed-jp-llm".to_string()]);
    let json: Value = serde_json::from_slice(&bytes).expect("the lock file is valid JSON");

    let mut keys = json
        .as_object()
        .expect("the lock file is a JSON object")
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "authToken".to_string(),
            "ideName".to_string(),
            "pid".to_string(),
            "runningInWindows".to_string(),
            "transport".to_string(),
            "workspaceFolders".to_string(),
        ]
    );
    // CLI をサンドボックスの `127.0.0.1` に繋がせ、パスは Linux の形のまま送らせる。
    assert_eq!(json["runningInWindows"], json!(false));
    // pid が偽なら、CLI は古いロックの掃除でポートに繋いで生死を決める。
    assert_eq!(json["pid"], json!(0));
    assert_eq!(json["transport"], json!("ws"));
    assert_eq!(json["ideName"], json!("Zed"));
    assert_eq!(json["authToken"], json!("sandbox-token"));
    assert_eq!(json["workspaceFolders"], json!(["/mnt/i/zed-jp-llm"]));

    let contents =
        LockFileContents::from_json(std::str::from_utf8(&bytes).expect("the lock file is UTF-8"))
            .expect("the CLI-shaped reader accepts it");
    assert!(!contents.running_in_windows);
}

// ---------------------------------------------------------------------------
// サンドボックスのセッション (ADR-F49)
// ---------------------------------------------------------------------------

/// ヘルパーのブリッジが運んでくる 1 本の接続の代わり。片方が書いたバイトをもう片方が読む。
#[derive(Debug)]
struct MemoryStream {
    incoming: Arc<Mutex<Pipe>>,
    outgoing: Arc<Mutex<Pipe>>,
}

#[derive(Debug, Default)]
struct Pipe {
    bytes: VecDeque<u8>,
    closed: bool,
    reader: Option<Waker>,
}

fn memory_stream_pair() -> (MemoryStream, MemoryStream) {
    let first_to_second = Arc::new(Mutex::new(Pipe::default()));
    let second_to_first = Arc::new(Mutex::new(Pipe::default()));
    (
        MemoryStream {
            incoming: second_to_first.clone(),
            outgoing: first_to_second.clone(),
        },
        MemoryStream {
            incoming: first_to_second,
            outgoing: second_to_first,
        },
    )
}

fn close_pipe(pipe: &Mutex<Pipe>) {
    let mut pipe = pipe.lock().expect("the pipe lock is not poisoned");
    pipe.closed = true;
    if let Some(reader) = pipe.reader.take() {
        reader.wake();
    }
}

impl Drop for MemoryStream {
    fn drop(&mut self) {
        close_pipe(&self.incoming);
        close_pipe(&self.outgoing);
    }
}

impl AsyncRead for MemoryStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buffer: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let mut pipe = self.incoming.lock().expect("the pipe lock is not poisoned");
        if pipe.bytes.is_empty() {
            if pipe.closed {
                return Poll::Ready(Ok(0));
            }
            pipe.reader = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let length = buffer.len().min(pipe.bytes.len());
        for (slot, byte) in buffer.iter_mut().zip(pipe.bytes.drain(..length)) {
            *slot = byte;
        }
        Poll::Ready(Ok(length))
    }
}

impl AsyncWrite for MemoryStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut pipe = self.outgoing.lock().expect("the pipe lock is not poisoned");
        if pipe.closed {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        pipe.bytes.extend(buffer);
        if let Some(reader) = pipe.reader.take() {
            reader.wake();
        }
        Poll::Ready(Ok(buffer.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> Poll<io::Result<()>> {
        close_pipe(&self.outgoing);
        Poll::Ready(Ok(()))
    }
}

/// サンドボックスの中から見たワークツリー `/test` と、その外の `/outside`。
const CLIENT_ROOT: &str = if cfg!(windows) {
    "/mnt/c/test"
} else {
    "/test"
};
const CLIENT_OUTSIDE: &str = if cfg!(windows) {
    "/mnt/c/outside"
} else {
    "/outside"
};

fn sandbox_mapper() -> Arc<dyn PathMapper> {
    if cfg!(windows) {
        Arc::new(WslPathMapper {
            distro: "Ubuntu".to_string(),
        })
    } else {
        Arc::new(IdentityPathMapper)
    }
}

struct SandboxFixture {
    fs: Arc<FakeFs>,
    project: Entity<Project>,
    window: WindowHandle<MultiWorkspace>,
    workspace: Entity<Workspace>,
    connections: UnboundedSender<MemoryStream>,
    session: Option<SandboxIdeSessionHandle>,
    auth_token: String,
}

/// フィクスチャーが始めるセッション。
#[derive(Clone, Copy, PartialEq, Eq)]
enum FixtureSession {
    /// サンドボックスの中の CLI 用 ([`crate::start_sandbox_session`])。
    ClaudeCodeCli,
    /// サンドボックスの中の ACP エージェント用の制限セッション
    /// ([`crate::start_restricted_sandbox_session`]、ADR-F49 決定 24)。
    Restricted,
}

/// `ide_server.enabled` は既定の false のまま、サンドボックスのセッションだけを始める。
async fn start_sandbox_fixture(cx: &mut TestAppContext) -> SandboxFixture {
    start_sandbox_fixture_with_roots(vec![PathBuf::from(path!("/test"))], cx).await
}

async fn start_sandbox_fixture_with_roots(
    roots: Vec<PathBuf>,
    cx: &mut TestAppContext,
) -> SandboxFixture {
    start_fixture(roots, FixtureSession::ClaudeCodeCli, cx).await
}

async fn start_restricted_sandbox_fixture(cx: &mut TestAppContext) -> SandboxFixture {
    start_fixture(
        vec![PathBuf::from(path!("/test"))],
        FixtureSession::Restricted,
        cx,
    )
    .await
}

async fn start_fixture(
    roots: Vec<PathBuf>,
    session_kind: FixtureSession,
    cx: &mut TestAppContext,
) -> SandboxFixture {
    init_test(cx);
    cx.update(crate::init);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/test"),
        json!({ "main.rs": "fn main() {\n    println!(\"hi\");\n}\n" }),
    )
    .await;
    fs.insert_tree(
        path!("/outside"),
        json!({ "secret.rs": "const SECRET: &str = \"hunter2\";\n" }),
    )
    .await;
    let project = Project::test(fs.clone(), [path!("/test").as_ref()], cx).await;
    let window = cx.add_window({
        let project = project.clone();
        move |window, cx| MultiWorkspace::test_new(project, window, cx)
    });
    let workspace = window
        .update(cx, |multi_workspace, _, _| {
            multi_workspace.workspace().clone()
        })
        .expect("the window is open");
    cx.run_until_parked();

    let (connections, incoming_connections) = unbounded::<MemoryStream>();
    let weak_workspace = workspace.downgrade();
    let (session, info) = cx.update(|cx| match session_kind {
        FixtureSession::ClaudeCodeCli => crate::start_sandbox_session(
            weak_workspace,
            incoming_connections,
            sandbox_mapper(),
            roots,
            cx,
        ),
        FixtureSession::Restricted => crate::start_restricted_sandbox_session(
            weak_workspace,
            incoming_connections,
            Arc::new(SandboxPaths::new(sandbox_mapper(), roots)),
            cx,
        ),
    });
    cx.run_until_parked();
    SandboxFixture {
        fs,
        project,
        window,
        workspace,
        connections,
        session: Some(session),
        auth_token: info.auth_token,
    }
}

fn upgrade_request(token: &str) -> async_tungstenite::tungstenite::http::Request<()> {
    async_tungstenite::tungstenite::http::Request::builder()
        .method("GET")
        .uri("ws://127.0.0.1:41234/")
        .header("Host", "127.0.0.1:41234")
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
        .header("Sec-WebSocket-Protocol", "mcp")
        .header(transport::CLAUDE_CODE_AUTH_HEADER, token)
        .body(())
        .expect("building the upgrade request")
}

/// ブリッジに新しい接続を 1 本渡し、CLI と同じ WebSocket の握手をする。
async fn open_websocket(
    fixture: &SandboxFixture,
    token: &str,
) -> Result<WebSocketStream<MemoryStream>, WebSocketError> {
    let (server_end, client_end) = memory_stream_pair();
    fixture
        .connections
        .unbounded_send(server_end)
        .expect("the session accepts connections");
    async_tungstenite::client_async(upgrade_request(token), client_end)
        .await
        .map(|(socket, _response)| socket)
}

async fn connect(fixture: &SandboxFixture, cx: &mut TestAppContext) -> SandboxClient {
    let socket = open_websocket(fixture, &fixture.auth_token)
        .await
        .expect("the session's own token is accepted");
    cx.run_until_parked();
    SandboxClient {
        socket,
        unclaimed: Vec::new(),
        next_id: 0,
    }
}

/// サンドボックスの CLI 役。待っていない応答や通知は `unclaimed` に溜めておく。
struct SandboxClient {
    socket: WebSocketStream<MemoryStream>,
    unclaimed: Vec<Value>,
    next_id: u64,
}

impl SandboxClient {
    async fn send(&mut self, method: &str, params: Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.socket
            .send(async_tungstenite::tungstenite::Message::text(
                request.to_string(),
            ))
            .await
            .expect("sending a request");
        id
    }

    async fn next_message(&mut self) -> Value {
        let frame = self
            .socket
            .next()
            .await
            .expect("the session keeps the connection open")
            .expect("the frame is valid");
        serde_json::from_str(frame.to_text().expect("the frame is text"))
            .expect("the frame is JSON")
    }

    async fn message_matching(&mut self, matches: impl Fn(&Value) -> bool) -> Value {
        if let Some(index) = self.unclaimed.iter().position(&matches) {
            return self.unclaimed.remove(index);
        }
        loop {
            let message = self.next_message().await;
            if matches(&message) {
                return message;
            }
            self.unclaimed.push(message);
        }
    }

    async fn response(&mut self, id: u64) -> Value {
        self.message_matching(|message| message.get("id") == Some(&json!(id)))
            .await
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params).await;
        self.response(id).await
    }

    async fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
        .await
    }

    async fn notification(&mut self, method: &str, matches: impl Fn(&Value) -> bool) -> Value {
        self.message_matching(|message| {
            message["method"] == json!(method) && matches(&message["params"])
        })
        .await
    }
}

async fn open_path(fixture: &SandboxFixture, path: &str, cx: &mut TestAppContext) {
    let workspace = fixture.workspace.clone();
    let open = fixture
        .window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_abs_path(
                    PathBuf::from(path),
                    workspace::OpenOptions::default(),
                    window,
                    cx,
                )
            })
        })
        .expect("the window is open");
    open.await.expect("the file opens");
    cx.run_until_parked();
}

fn set_diagnostic(project: &Entity<Project>, path: &str, message: &str, cx: &mut TestAppContext) {
    project
        .update(cx, |project, cx| {
            project.lsp_store().update(cx, |lsp_store, cx| {
                lsp_store.update_diagnostic_entries(
                    LanguageServerId(0),
                    PathBuf::from(path),
                    None,
                    None,
                    vec![DiagnosticEntry::new(
                        Unclipped(PointUtf16::new(0, 0))..Unclipped(PointUtf16::new(0, 2)),
                        Diagnostic {
                            severity: DiagnosticSeverity::ERROR,
                            message: message.to_string().into(),
                            source_kind: DiagnosticSourceKind::Pushed,
                            ..Diagnostic::default()
                        },
                    )],
                    cx,
                )
            })
        })
        .expect("the diagnostics apply");
}

fn open_diff_tabs(
    workspace: &Entity<Workspace>,
    cx: &mut TestAppContext,
) -> Vec<Entity<IdeServerDiff>> {
    workspace.read_with(cx, |workspace, cx| {
        workspace
            .panes()
            .iter()
            .flat_map(|pane| {
                pane.read(cx)
                    .items()
                    .filter_map(|item| item.downcast::<IdeServerDiff>())
                    .collect::<Vec<_>>()
            })
            .collect()
    })
}

fn has_editor_tab(workspace: &Entity<Workspace>, tab_name: &str, cx: &mut TestAppContext) -> bool {
    workspace.read_with(cx, |workspace, cx| {
        workspace.panes().iter().any(|pane| {
            pane.read(cx).items().any(|item| {
                item.act_as::<Editor>(cx).is_some()
                    && item.tab_content_text(0, cx).as_ref() == tab_name
            })
        })
    })
}

fn is_claude_code_connected(project: &Entity<Project>, cx: &mut TestAppContext) -> bool {
    let project_id = project.entity_id();
    cx.update(|cx| zed_actions::ide_server::is_claude_code_connected(project_id, cx))
}

fn open_diff_arguments(path: &str, tab_name: &str) -> Value {
    json!({
        "old_file_path": path,
        "new_file_path": path,
        "new_file_contents": "fn main() {}\n",
        "tab_name": tab_name,
    })
}

#[gpui::test]
async fn sandbox_session_checks_the_token_and_offers_only_the_cli_tools(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;

    match open_websocket(&fixture, "wrong-token").await {
        Ok(_) => panic!("a connection with the wrong token must be refused"),
        Err(WebSocketError::Http(response)) => assert_eq!(response.status(), 401),
        Err(error) => panic!("expected a 401, got {error}"),
    }

    // CLI の古いロックの掃除は、繋いですぐ切るだけの生存確認をする。
    let (server_end, client_end) = memory_stream_pair();
    fixture
        .connections
        .unbounded_send(server_end)
        .expect("the session accepts connections");
    drop(client_end);
    cx.run_until_parked();

    let mut client = connect(&fixture, cx).await;
    assert!(
        is_claude_code_connected(&fixture.project, cx),
        "a sandboxed CLI counts as connected even with ide_server.enabled off"
    );

    let response = client
        .request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "claude-code", "version": "2.1.269" }
            }),
        )
        .await;
    assert_eq!(response["result"]["serverInfo"]["name"], json!("Zed"));

    let response = client.request("tools/list", json!({})).await;
    assert_eq!(
        tool_names(&response),
        vec![
            "closeAllDiffTabs",
            "close_tab",
            "getDiagnostics",
            "openDiff"
        ],
        "{response}"
    );

    for tool in [
        "openFile",
        "saveDocument",
        "checkDocumentDirty",
        "getWorkspaceFolders",
        "getOpenEditors",
        "getCurrentSelection",
        "getLatestSelection",
        "executeCode",
        "noSuchTool",
    ] {
        let response = client
            .call_tool(tool, json!({ "filePath": path!("/test/main.rs") }))
            .await;
        assert_eq!(
            response["error"]["code"],
            json!(protocol::INVALID_PARAMS),
            "{tool}: {response}"
        );
        assert!(response.get("result").is_none(), "{tool}: {response}");
    }
    assert!(
        !has_editor_tab(&fixture.workspace, "main.rs", cx),
        "a refused openFile must not open anything"
    );
}

#[gpui::test]
async fn sandbox_session_maps_diagnostic_paths_both_ways(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    set_diagnostic(
        &fixture.project,
        path!("/test/main.rs"),
        "unknown macro",
        cx,
    );
    // ワークツリーの外のファイルにも診断があるが、サンドボックスには見せない。
    open_path(&fixture, path!("/outside/secret.rs"), cx).await;
    set_diagnostic(
        &fixture.project,
        path!("/outside/secret.rs"),
        "hunter2 leaked",
        cx,
    );
    cx.run_until_parked();

    let mut client = connect(&fixture, cx).await;
    let main_uri = format!("file://{CLIENT_ROOT}/main.rs");

    // 開いていないファイルは、URI を指定されても開かない (サンドボックスが書いたファイルで
    // ホストの言語サーバーを起こさない)。
    let response = client
        .call_tool("getDiagnostics", json!({ "uri": main_uri }))
        .await;
    assert_eq!(response["result"]["isError"], json!(false), "{response}");
    assert_eq!(first_text_as_json(&response), json!([]), "{response}");
    assert!(!is_open(&fixture.project, path!("/test/main.rs"), cx));

    open_path(&fixture, path!("/test/main.rs"), cx).await;
    let response = client
        .call_tool("getDiagnostics", json!({ "uri": main_uri }))
        .await;
    assert_eq!(response["result"]["isError"], json!(false), "{response}");
    let files = first_text_as_json(&response);
    assert_eq!(files[0]["uri"], json!(main_uri), "{files}");
    assert_eq!(
        files[0]["diagnostics"][0]["message"],
        json!("unknown macro")
    );
    assert_eq!(files[0]["diagnostics"][0]["severity"], json!("Error"));

    // URI を指定しない問い合わせも、すでに開いているバッファーの診断だけを返す。
    fixture
        .fs
        .insert_file(path!("/test/other.rs"), b"fn other() {}\n".to_vec())
        .await;
    cx.run_until_parked();
    set_diagnostic(
        &fixture.project,
        path!("/test/other.rs"),
        "never opened",
        cx,
    );
    let response = client.call_tool("getDiagnostics", json!({})).await;
    assert!(!response.to_string().contains("never opened"), "{response}");
    assert!(
        !is_open(&fixture.project, path!("/test/other.rs"), cx),
        "the sandbox cannot make Zed open files"
    );
    let uris = first_text_as_json(&response)
        .as_array()
        .expect("the diagnostics are an array")
        .iter()
        .filter_map(|file| file["uri"].as_str().map(str::to_string))
        .collect::<Vec<_>>();
    assert_eq!(uris, vec![main_uri.clone()]);
    assert!(!response.to_string().contains("hunter2"), "{response}");

    for uri in [
        format!("file://{CLIENT_OUTSIDE}/secret.rs"),
        format!("file://{CLIENT_ROOT}/../outside/secret.rs"),
        "file:///C:/test/main.rs".to_string(),
        "main.rs".to_string(),
    ] {
        let response = client
            .call_tool("getDiagnostics", json!({ "uri": uri }))
            .await;
        assert_eq!(
            response["result"]["isError"],
            json!(true),
            "{uri}: {response}"
        );
        assert!(!response.to_string().contains("hunter2"), "{response}");
    }
}

#[gpui::test]
async fn sandbox_session_opens_diffs_only_inside_the_worktree(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    // サンドボックスはワークツリーに書けるので、外を指すリンクを置ける。
    fixture
        .fs
        .insert_symlink(
            path!("/test/link.rs"),
            PathBuf::from(path!("/outside/secret.rs")),
        )
        .await;
    cx.run_until_parked();
    let mut client = connect(&fixture, cx).await;
    let main_path = format!("{CLIENT_ROOT}/main.rs");

    // 採用しても本文を返すだけで、ディスクには書かない (ホストの IDE サーバーと同じ)。
    let id = client
        .send(
            "tools/call",
            json!({ "name": "openDiff", "arguments": open_diff_arguments(&main_path, "proposal") }),
        )
        .await;
    cx.run_until_parked();
    let diffs = open_diff_tabs(&fixture.workspace, cx);
    let diff = diffs
        .first()
        .expect("the diff tab opens in the sandbox's workspace");
    diff.update(cx, |diff, cx| diff.accept(cx));
    let response = client.response(id).await;
    assert_eq!(
        response["result"]["content"][0]["text"],
        json!("FILE_SAVED")
    );
    assert_eq!(
        response["result"]["content"][1]["text"],
        json!("fn main() {}\n")
    );
    assert_eq!(
        fixture
            .fs
            .load(path!("/test/main.rs").as_ref())
            .await
            .expect("the file is readable"),
        "fn main() {\n    println!(\"hi\");\n}\n"
    );
    cx.run_until_parked();

    for path in [
        format!("{CLIENT_OUTSIDE}/secret.rs"),
        format!("{CLIENT_ROOT}/../outside/secret.rs"),
        format!("{CLIENT_ROOT}/link.rs"),
    ] {
        let response = client
            .call_tool("openDiff", open_diff_arguments(&path, "escape"))
            .await;
        assert_eq!(
            response["result"]["isError"],
            json!(true),
            "{path}: {response}"
        );
    }
    cx.run_until_parked();
    assert!(
        open_diff_tabs(&fixture.workspace, cx).is_empty(),
        "a refused path opens no tab"
    );

    // `close_tab` はセッションが開いた差分タブだけを閉じ、ユーザーのタブには触らない。
    open_path(&fixture, path!("/test/main.rs"), cx).await;
    let id = client
        .send(
            "tools/call",
            json!({ "name": "openDiff", "arguments": open_diff_arguments(&main_path, "proposal") }),
        )
        .await;
    cx.run_until_parked();
    assert_eq!(open_diff_tabs(&fixture.workspace, cx).len(), 1);

    let response = client
        .call_tool("close_tab", json!({ "tab_name": "main.rs" }))
        .await;
    assert_eq!(response["result"]["isError"], json!(false), "{response}");
    cx.run_until_parked();
    assert!(has_editor_tab(&fixture.workspace, "main.rs", cx));
    assert_eq!(open_diff_tabs(&fixture.workspace, cx).len(), 1);

    let response = client
        .call_tool("close_tab", json!({ "tab_name": "proposal" }))
        .await;
    assert_eq!(response["result"]["isError"], json!(false), "{response}");
    let response = client.response(id).await;
    assert_eq!(
        response["result"]["content"][0]["text"],
        json!("DIFF_REJECTED")
    );
    cx.run_until_parked();
    assert!(open_diff_tabs(&fixture.workspace, cx).is_empty());
}

// ---------------------------------------------------------------------------
// ADR-F53: SSH リモートのプロジェクト (パスの確認は接続先で)
// ---------------------------------------------------------------------------

/// ローカルの `FakeFs` のプロジェクトを、接続先で確かめる経路に通す。
///
/// `Project::verify_agent_read` はローカルのワークツリーでは `Fs::verify_read` に降りるので、
/// 「ルート直下から 1 段もリンクを通らずに開ける」の判定そのものは本物のまま (接続先では
/// 同じ判定を proto 506 の先で行う。往復は `remote_server` の
/// `test_remote_verified_read` が押さえている)。
async fn remote_request<'a>(
    paths: &'a SandboxPaths,
    project: &Entity<Project>,
    cx: &mut TestAppContext,
) -> tools::SandboxRequest<'a> {
    let project = project.clone();
    let mut async_cx = cx.to_async();
    tools::SandboxRequest::with_kind(
        paths,
        &project,
        tools::SandboxChecksKind::OnTheRemoteHost,
        &mut async_cx,
    )
    .await
}

fn session_paths() -> SandboxPaths {
    SandboxPaths::new(sandbox_mapper(), vec![PathBuf::from(path!("/test"))])
}

/// ADR-F53 の要: リモートのプロジェクトでは、リンクを通るパスかどうかを接続先に判定させる。
/// ワークツリーの中に置かれたリンクも、リンクを通った先のファイルも断り、正規のパスは通す。
#[gpui::test]
async fn a_remote_sandbox_request_lets_the_host_refuse_links(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    fixture
        .fs
        .insert_tree(path!("/test/src"), json!({ "deep.rs": "fn deep() {}\n" }))
        .await;
    fixture
        .fs
        .insert_symlink(
            path!("/test/link.rs"),
            PathBuf::from(path!("/outside/secret.rs")),
        )
        .await;
    fixture
        .fs
        .insert_symlink(path!("/test/through"), PathBuf::from(path!("/outside")))
        .await;
    cx.run_until_parked();

    let paths = session_paths();
    let request = remote_request(&paths, &fixture.project, cx).await;
    assert_eq!(
        request.checks_kind(),
        tools::SandboxChecksKind::OnTheRemoteHost
    );

    let mut async_cx = cx.to_async();
    let resolved = request
        .resolve(&format!("{CLIENT_ROOT}/src/deep.rs"), &mut async_cx)
        .await
        .expect("a plain file inside the worktree is allowed");
    assert_eq!(resolved, PathBuf::from(path!("/test/src/deep.rs")));

    for path in [
        format!("{CLIENT_ROOT}/link.rs"),
        format!("{CLIENT_ROOT}/through/secret.rs"),
        format!("{CLIENT_OUTSIDE}/secret.rs"),
        format!("{CLIENT_ROOT}/../outside/secret.rs"),
    ] {
        request
            .resolve(&path, &mut async_cx)
            .await
            .expect_err(&format!("{path} leaves the worktree and is refused"));
    }
}

/// ADR-F53: まだ無いパス (新規の `openDiff`) は接続先に判定させられないので、走査が知っている
/// 祖先で判断する。ワークツリーの**中**を指すリンクも、外を指すリンクも祖先にあれば断る。
/// 日本語のパスも同じ経路を通る。
#[gpui::test]
async fn a_remote_sandbox_request_refuses_a_new_path_under_a_link(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    fixture
        .fs
        .insert_tree(
            path!("/test/src"),
            json!({ "deep.rs": "fn deep() {}\n", "日本語.rs": "// あ\n" }),
        )
        .await;
    fixture
        .fs
        .insert_symlink(path!("/test/through"), PathBuf::from(path!("/outside")))
        .await;
    // ワークツリーの中を指すリンク: `is_external` は立たないので、`canonical_path` だけが
    // これをリンクだと言える。
    fixture
        .fs
        .insert_symlink(path!("/test/inner"), PathBuf::from(path!("/test/src")))
        .await;
    fixture
        .fs
        .insert_symlink(
            path!("/test/alias.rs"),
            PathBuf::from(path!("/test/src/deep.rs")),
        )
        .await;
    cx.run_until_parked();

    let paths = session_paths();
    let request = remote_request(&paths, &fixture.project, cx).await;
    let mut async_cx = cx.to_async();

    for path in [
        // ワークツリーの中を指すリンクそのもの。
        format!("{CLIENT_ROOT}/alias.rs"),
        // 祖先がリンク (中を指すもの・外を指すもの) の、まだ無いパス。
        format!("{CLIENT_ROOT}/inner/new.rs"),
        format!("{CLIENT_ROOT}/through/new.rs"),
    ] {
        request
            .resolve(&path, &mut async_cx)
            .await
            .expect_err(&format!("{path} goes through a link and is refused"));
        request
            .read_old_contents(
                &paths.to_host(&path).unwrap_or_default(),
                &path,
                &mut async_cx,
            )
            .await
            .expect_err(&format!("{path} is not read either"));
    }

    // 日本語のパス: 既にあるものは読め、まだ無いものは新規として空。
    let japanese = format!("{CLIENT_ROOT}/src/日本語.rs");
    let host_path = paths
        .to_host(&japanese)
        .expect("a path inside the worktree");
    assert_eq!(
        request
            .read_old_contents(&host_path, &japanese, &mut async_cx)
            .await
            .expect("a plain file with a Japanese name is allowed"),
        "// あ\n"
    );
    let new_japanese = format!("{CLIENT_ROOT}/src/新しい ファイル.rs");
    let host_path = paths
        .to_host(&new_japanese)
        .expect("a path inside the worktree");
    assert_eq!(
        request
            .read_old_contents(&host_path, &new_japanese, &mut async_cx)
            .await
            .expect("a new file with a Japanese name is a new file"),
        ""
    );
}

/// ADR-F53: ワークツリーの走査に出ないパス (`.git` 配下や `file_scan_exclusions` に当たる
/// もの・走査が届いていないもの) は、接続先に実体があるかを確かめる。実体があるなら
/// **断る** —— 旧側を空で出すと既にあるファイルが「新規ファイル」として人の確認を通って
/// しまい、かといって大きさが分からないまま開くと接続先のファイルを丸ごと読み込むため。
#[gpui::test]
async fn a_remote_sandbox_request_refuses_paths_the_worktree_scan_leaves_out(
    cx: &mut TestAppContext,
) {
    let fixture = start_sandbox_fixture(cx).await;
    fixture
        .fs
        .insert_tree(
            path!("/test/.git"),
            json!({ "config": "[core]\n\tbare = false\n" }),
        )
        .await;
    cx.run_until_parked();
    // 走査には出ない。
    fixture.project.read_with(cx, |project, cx| {
        let worktree = project
            .visible_worktrees(cx)
            .next()
            .expect("the fixture opens one worktree");
        assert!(
            worktree
                .read(cx)
                .entry_for_path(util::rel_path::rel_path(".git/config"))
                .is_none(),
            "`.git` is excluded from the scan, which is what this test is about"
        );
    });

    let paths = session_paths();
    let request = remote_request(&paths, &fixture.project, cx).await;
    let mut async_cx = cx.to_async();
    let text = format!("{CLIENT_ROOT}/.git/config");
    let host_path = paths.to_host(&text).expect("a path inside the worktree");
    let error = request
        .read_old_contents(&host_path, &text, &mut async_cx)
        .await
        .expect_err("the host can open it, so it is not a new file and not shown as one");
    assert!(
        format!("{error:#}").contains("previous contents cannot be shown"),
        "{error:#}"
    );
    request
        .resolve(&text, &mut async_cx)
        .await
        .expect_err("the same refusal reaches openDiff before anything is read");

    // 走査も接続先も知らないパスは、これまでどおり新規ファイル (旧側は空)。
    let text = format!("{CLIENT_ROOT}/.git/not-there");
    let host_path = paths.to_host(&text).expect("a path inside the worktree");
    assert_eq!(
        request
            .read_old_contents(&host_path, &text, &mut async_cx)
            .await
            .expect("nothing is there, so it is a new file"),
        ""
    );
}

/// ADR-F53: 大きすぎるファイルは、接続先から**開く前**に断る (開いてしまうと接続先の
/// ファイルを丸ごと読み込むので、サンドボックスの中の CLI にメモリを使い切らせられる)。
#[gpui::test]
async fn a_remote_sandbox_request_refuses_an_oversized_file_before_opening_it(
    cx: &mut TestAppContext,
) {
    let fixture = start_sandbox_fixture(cx).await;
    fixture
        .fs
        .insert_file(path!("/test/huge.txt"), vec![b'a'; 9 * 1024 * 1024])
        .await;
    cx.run_until_parked();

    let paths = session_paths();
    let request = remote_request(&paths, &fixture.project, cx).await;
    let mut async_cx = cx.to_async();
    let text = format!("{CLIENT_ROOT}/huge.txt");
    let host_path = paths.to_host(&text).expect("a path inside the worktree");
    let error = request
        .read_old_contents(&host_path, &text, &mut async_cx)
        .await
        .expect_err("a file over the limit is refused");
    assert!(format!("{error:#}").contains("larger than"), "{error:#}");
    // 同じ大きさの判定が、上限に収まるファイルを巻き込まないこと。
    let text = format!("{CLIENT_ROOT}/main.rs");
    let host_path = paths.to_host(&text).expect("a path inside the worktree");
    assert_eq!(
        request
            .read_old_contents(&host_path, &text, &mut async_cx)
            .await
            .expect("a file inside the limit is read"),
        "fn main() {\n    println!(\"hi\");\n}\n"
    );
}

/// ADR-F53: ワークツリーが 2 つあるときも、それぞれのルートを基点に接続先が判定する
/// (2 つ目のルートの中のリンクも断る)。
#[gpui::test]
async fn a_remote_sandbox_request_checks_every_worktree(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    fixture
        .fs
        .insert_tree(path!("/second"), json!({ "other.rs": "fn other() {}\n" }))
        .await;
    fixture
        .fs
        .insert_symlink(
            path!("/second/away.rs"),
            PathBuf::from(path!("/outside/secret.rs")),
        )
        .await;
    fixture
        .project
        .update(cx, |project, cx| {
            project.create_worktree(path!("/second"), true, cx)
        })
        .await
        .expect("the second folder opens");
    cx.run_until_parked();

    let paths = SandboxPaths::new(
        sandbox_mapper(),
        vec![
            PathBuf::from(path!("/test")),
            PathBuf::from(path!("/second")),
        ],
    );
    let request = remote_request(&paths, &fixture.project, cx).await;
    let mut async_cx = cx.to_async();
    let second_root = if cfg!(windows) {
        "/mnt/c/second"
    } else {
        "/second"
    };
    assert_eq!(
        request
            .resolve(&format!("{second_root}/other.rs"), &mut async_cx)
            .await
            .expect("a plain file in the second worktree is allowed"),
        PathBuf::from(path!("/second/other.rs"))
    );
    request
        .resolve(&format!("{second_root}/away.rs"), &mut async_cx)
        .await
        .expect_err("a link in the second worktree is refused too");
    // 1 つ目のワークツリーはそのまま。
    assert_eq!(
        request
            .resolve(&format!("{CLIENT_ROOT}/main.rs"), &mut async_cx)
            .await
            .expect("the first worktree still works"),
        PathBuf::from(path!("/test/main.rs"))
    );
}

/// ADR-F53: 接続先に辿らせる基点はセッションのルートだけ。セッションのルートの**中**に
/// 別のワークツリーが開かれていると、`find_project_path` がそちらを返しうる —— その場合
/// 「ルートの中に置かれたリンクをたどらない」がそのフォルダーまでの分だけ確かめられなく
/// なるので、基点がセッションのルートでなければ断る (ローカルの経路はセッションのルートから
/// 辿るので、同じ基点に揃える)。
#[gpui::test]
async fn a_remote_sandbox_request_roots_its_checks_at_the_sessions_own_folders(
    cx: &mut TestAppContext,
) {
    let fixture = start_sandbox_fixture(cx).await;
    fixture
        .fs
        .insert_tree(
            path!("/test/nested"),
            json!({ "inner.rs": "fn inner() {}\n" }),
        )
        .await;
    cx.run_until_parked();
    // セッションのルートは `/test` のまま。そのあとでワークスペースの側だけが変わる。
    let paths = session_paths();
    fixture
        .project
        .update(cx, |project, cx| {
            project.create_worktree(path!("/test/nested"), true, cx)
        })
        .await
        .expect("the nested folder opens");
    let outer = fixture.project.read_with(cx, |project, cx| {
        project
            .visible_worktrees(cx)
            .find(|worktree| worktree.read(cx).abs_path().as_ref() == Path::new(path!("/test")))
            .map(|worktree| worktree.read(cx).id())
            .expect("the session's own folder is open")
    });
    fixture.project.update(cx, |project, cx| {
        project.remove_worktree(outer, cx);
    });
    cx.run_until_parked();

    let request = remote_request(&paths, &fixture.project, cx).await;
    let mut async_cx = cx.to_async();
    request
        .resolve(&format!("{CLIENT_ROOT}/nested/inner.rs"), &mut async_cx)
        .await
        .expect_err("the only folder left is not the session's, so nothing is rooted at it");
}

/// `openDiff` の旧本文: 接続先の確認を通ったファイルは (未保存の変更を含めて) バッファーから
/// 読み、まだ無いファイルは空、リンクを通るものは断る。
#[gpui::test]
async fn a_remote_sandbox_request_reads_old_contents_through_the_host(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    fixture
        .fs
        .insert_symlink(
            path!("/test/link.rs"),
            PathBuf::from(path!("/outside/secret.rs")),
        )
        .await;
    cx.run_until_parked();

    let paths = session_paths();
    let request = remote_request(&paths, &fixture.project, cx).await;
    let mut async_cx = cx.to_async();

    let contents = request
        .read_old_contents(
            Path::new(path!("/test/main.rs")),
            &format!("{CLIENT_ROOT}/main.rs"),
            &mut async_cx,
        )
        .await
        .expect("the host allows the plain file");
    assert_eq!(contents, "fn main() {\n    println!(\"hi\");\n}\n");

    let contents = request
        .read_old_contents(
            Path::new(path!("/test/brand-new.rs")),
            &format!("{CLIENT_ROOT}/brand-new.rs"),
            &mut async_cx,
        )
        .await
        .expect("a file that is not there yet is a new file, not a refusal");
    assert_eq!(contents, "", "a new file's old side is empty");

    request
        .read_old_contents(
            Path::new(path!("/test/link.rs")),
            &format!("{CLIENT_ROOT}/link.rs"),
            &mut async_cx,
        )
        .await
        .expect_err("a link is refused instead of read");
}

/// 差分の旧本文は、ホストの経路と同じく開いているバッファーから来るので、未保存の変更も見える
/// (ADR-F53 の親の決定 (a))。
#[gpui::test]
async fn a_remote_sandbox_request_sees_unsaved_changes(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    cx.run_until_parked();
    let buffer = fixture
        .project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/test/main.rs"), cx)
        })
        .await
        .expect("the file opens");
    buffer.update(cx, |buffer, cx| buffer.set_text("fn edited() {}\n", cx));
    cx.run_until_parked();

    let paths = session_paths();
    let request = remote_request(&paths, &fixture.project, cx).await;
    let mut async_cx = cx.to_async();
    let contents = request
        .read_old_contents(
            Path::new(path!("/test/main.rs")),
            &format!("{CLIENT_ROOT}/main.rs"),
            &mut async_cx,
        )
        .await
        .expect("the host allows the plain file");
    assert_eq!(contents, "fn edited() {}\n");
}

/// 診断は開いているバッファーだけを返す。リモートでも、リンク越しに開かれたバッファーは
/// 接続先の判定で落ちる。ワークツリーの**中**を指すリンクは `is_external` が立たないので、
/// これを断れるのは接続先の `verify_agent_read` だけ。
#[gpui::test]
async fn a_remote_sandbox_request_verifies_open_buffers_on_the_host(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    fixture
        .fs
        .insert_symlink(
            path!("/test/link.rs"),
            PathBuf::from(path!("/outside/secret.rs")),
        )
        .await;
    fixture
        .fs
        .insert_symlink(
            path!("/test/alias.rs"),
            PathBuf::from(path!("/test/main.rs")),
        )
        .await;
    cx.run_until_parked();

    let plain = fixture
        .project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/test/main.rs"), cx)
        })
        .await
        .expect("the file opens");
    let linked = fixture
        .project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/test/link.rs"), cx)
        })
        .await
        .expect("the link opens");
    let aliased = fixture
        .project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/test/alias.rs"), cx)
        })
        .await
        .expect("the alias opens");
    cx.run_until_parked();

    let paths = session_paths();
    let request = remote_request(&paths, &fixture.project, cx).await;
    let mut async_cx = cx.to_async();
    let abs_path = request
        .verify_open_buffer(&plain, &mut async_cx)
        .await
        .expect("a plain file in the worktree is shown");
    assert_eq!(abs_path, PathBuf::from(path!("/test/main.rs")));
    request
        .verify_open_buffer(&linked, &mut async_cx)
        .await
        .expect_err("a buffer reached through a link is left out");
    request
        .verify_open_buffer(&aliased, &mut async_cx)
        .await
        .expect_err("a link that stays inside the worktree is a link all the same");
}

/// ローカルのプロジェクトはこれまでどおりクライアントの FS で確かめる。
#[gpui::test]
async fn a_local_sandbox_request_still_checks_this_machine(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    cx.run_until_parked();
    let paths = session_paths();
    let project = fixture.project.clone();
    let mut async_cx = cx.to_async();
    let request = tools::SandboxRequest::new(&paths, &project, &mut async_cx).await;
    assert_eq!(
        request.checks_kind(),
        tools::SandboxChecksKind::OnThisMachine,
        "a project whose files are this machine's is checked here"
    );
}

/// ADR-F53: セッションを受けるプロジェクトの条件。ローカルと SSH リモートは受け、collab は
/// どちらの向きでも断る。
#[test]
fn a_sandbox_session_refuses_collab_projects() {
    let refusal = sandbox_client::session_project_refusal;
    assert_eq!(refusal(true, false, false), None, "a local project");
    assert_eq!(refusal(false, true, false), None, "an SSH-remote project");
    assert!(
        refusal(false, false, false).is_some(),
        "a project opened over collab"
    );
    assert!(
        refusal(true, false, true).is_some(),
        "a local project shared over collab"
    );
    assert!(
        refusal(false, true, true).is_some(),
        "an SSH-remote project shared over collab"
    );
}

#[gpui::test]
async fn sandbox_session_sends_notifications_in_the_client_form(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    let mut client = connect(&fixture, cx).await;
    let main_path = format!("{CLIENT_ROOT}/main.rs");

    open_path(&fixture, path!("/test/main.rs"), cx).await;
    let notification = client
        .notification("selection_changed", |params| {
            params["filePath"] == json!(main_path)
        })
        .await;
    assert_eq!(notification["params"]["text"], json!(""));

    // ワークツリーの外のファイルに移ったら、パスも本文も見せずに選択を消す。
    open_path(&fixture, path!("/outside/secret.rs"), cx).await;
    let notification = client
        .notification("selection_changed", is_cleared_selection)
        .await;
    assert_eq!(notification["params"]["text"], json!(""));

    let session = cx
        .update(|cx| {
            crate::sandbox_client::sessions_for_workspace(fixture.workspace.entity_id(), cx)
        })
        .into_iter()
        .next()
        .expect("the session is attached to the workspace");
    let sent = session.update(cx, |session, cx| {
        session.send_at_mention(Path::new(path!("/test/main.rs")), 0, 1, cx)
    });
    assert!(sent);
    let notification = client.notification("at_mentioned", |_| true).await;
    assert_eq!(
        notification["params"],
        json!({ "filePath": main_path, "lineStart": 0, "lineEnd": 1 })
    );
    let sent = session.update(cx, |session, cx| {
        session.send_at_mention(Path::new(path!("/outside/secret.rs")), 0, 0, cx)
    });
    assert!(!sent, "a file outside the worktree is not mentioned");
}

#[gpui::test]
async fn dropping_the_sandbox_session_closes_its_connections_and_diff_tabs(
    cx: &mut TestAppContext,
) {
    let mut fixture = start_sandbox_fixture(cx).await;
    let mut client = connect(&fixture, cx).await;
    assert!(is_claude_code_connected(&fixture.project, cx));

    let main_path = format!("{CLIENT_ROOT}/main.rs");
    client
        .send(
            "tools/call",
            json!({ "name": "openDiff", "arguments": open_diff_arguments(&main_path, "proposal") }),
        )
        .await;
    cx.run_until_parked();
    assert_eq!(open_diff_tabs(&fixture.workspace, cx).len(), 1);

    drop(fixture.session.take());
    cx.run_until_parked();
    // 解放された Entity の後始末は、次の更新の終わりに走る。
    cx.update(|_| {});
    cx.run_until_parked();

    assert!(
        open_diff_tabs(&fixture.workspace, cx).is_empty(),
        "ending the session closes the diff tabs it opened"
    );
    assert!(!is_claude_code_connected(&fixture.project, cx));
    assert!(
        !matches!(client.socket.next().await, Some(Ok(_))),
        "the forwarded connection is closed"
    );
}

#[gpui::test]
async fn sandbox_session_ends_when_the_bridge_goes_away(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    let mut client = connect(&fixture, cx).await;
    assert!(is_claude_code_connected(&fixture.project, cx));

    drop(fixture.connections);
    cx.run_until_parked();

    assert!(!is_claude_code_connected(&fixture.project, cx));
    assert!(
        !matches!(client.socket.next().await, Some(Ok(_))),
        "the forwarded connection is closed"
    );
}

#[gpui::test]
async fn the_status_item_shows_a_sandbox_connection_with_the_server_off(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    let status_item = fixture.workspace.update(cx, |workspace, cx| {
        cx.new(|cx| IdeServerStatusItem::new(workspace, cx))
    });
    cx.run_until_parked();
    let shown = |cx: &mut TestAppContext| status_item.read_with(cx, |item, cx| item.shown(cx));
    assert_eq!(
        shown(cx),
        None,
        "nothing is connected and ide_server.enabled is off"
    );

    let redraws = Rc::new(Cell::new(0));
    let _observation = cx.update(|cx| {
        let redraws = redraws.clone();
        cx.observe(&status_item, move |_, _| redraws.set(redraws.get() + 1))
    });
    let _client = connect(&fixture, cx).await;
    assert_eq!(shown(cx), Some(Shown::SandboxConnection));
    assert!(
        redraws.get() > 0,
        "the item redraws when the sandboxed CLI connects"
    );

    drop(fixture.connections);
    cx.run_until_parked();
    assert_eq!(shown(cx), None, "the item goes away with the session");
}

/// ブリッジに接続を 1 本渡し、streamable HTTP の MCP 要求を 1 件送って応答をすべて読む。
async fn post_mcp(fixture: &SandboxFixture, token: &str, request: &Value) -> String {
    let (server_end, mut client_end) = memory_stream_pair();
    fixture
        .connections
        .unbounded_send(server_end)
        .expect("the session accepts connections");

    let body = request.to_string();
    client_end
        .write_all(
            format!(
                "POST /mcp HTTP/1.1\r\n\
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
    response
}

fn http_body(response: &str) -> Value {
    let (_, body) = response
        .split_once("\r\n\r\n")
        .expect("the response has a body");
    serde_json::from_str(body).expect("the body is JSON")
}

/// `tools/list` の応答に並んでいるツールの名前 (並べ替え済み)。
fn tool_names(tools_list_result: &Value) -> Vec<String> {
    let mut names = tools_list_result["result"]["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["name"].as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// HTTP の応答はトランスポートが書き終えるまで接続を落とさない。
#[gpui::test]
async fn sandbox_session_answers_streamable_http_with_the_restricted_tools(
    cx: &mut TestAppContext,
) {
    let fixture = start_sandbox_fixture(cx).await;
    let response = post_mcp(
        &fixture,
        &fixture.auth_token,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    )
    .await;

    assert!(
        response.starts_with("HTTP/1.1 200 OK"),
        "expected a 200, got {response:?}"
    );
    let body = http_body(&response);
    assert_eq!(
        body["result"]["tools"].as_array().map(Vec::len),
        Some(4),
        "{body}"
    );
}

// ---------------------------------------------------------------------------
// 制限セッション (ADR-F49 決定 24)
// ---------------------------------------------------------------------------

/// 制限セッションが出すのは `getDiagnostics` 1 本だけ。差分タブを開く 3 本は、CLI 用の
/// セッションでは出しているものも含めてすべて JSON-RPC のエラーで断る。
#[gpui::test]
async fn a_restricted_session_offers_only_get_diagnostics(cx: &mut TestAppContext) {
    let fixture = start_restricted_sandbox_fixture(cx).await;

    match open_websocket(&fixture, "wrong-token").await {
        Ok(_) => panic!("a connection with the wrong token must be refused"),
        Err(WebSocketError::Http(response)) => assert_eq!(response.status(), 401),
        Err(error) => panic!("expected a 401, got {error}"),
    }

    let mut client = connect(&fixture, cx).await;
    let response = client
        .request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "acp-agent", "version": "0.1.0" }
            }),
        )
        .await;
    assert_eq!(response["result"]["serverInfo"]["name"], json!("Zed"));

    let response = client.request("tools/list", json!({})).await;
    assert_eq!(tool_names(&response), vec!["getDiagnostics"], "{response}");

    for tool in [
        "openDiff",
        "close_tab",
        "closeAllDiffTabs",
        "openFile",
        "saveDocument",
        "checkDocumentDirty",
        "getWorkspaceFolders",
        "getOpenEditors",
        "getCurrentSelection",
        "getLatestSelection",
        "executeCode",
        "noSuchTool",
    ] {
        let response = client
            .call_tool(
                tool,
                open_diff_arguments(&format!("{CLIENT_ROOT}/main.rs"), "proposal"),
            )
            .await;
        assert_eq!(
            response["error"]["code"],
            json!(protocol::INVALID_PARAMS),
            "{tool}: {response}"
        );
        assert!(response.get("result").is_none(), "{tool}: {response}");
    }
    cx.run_until_parked();
    assert!(
        open_diff_tabs(&fixture.workspace, cx).is_empty(),
        "a restricted session opens no diff tab"
    );
    assert!(!has_editor_tab(&fixture.workspace, "main.rs", cx));

    // 選択範囲の通知も送らない。撃たれていれば `ping` の応答より先に届いている。
    open_path(&fixture, path!("/test/main.rs"), cx).await;
    let id = client.send("ping", json!({})).await;
    let message = client.next_message().await;
    assert_eq!(
        message["id"],
        json!(id),
        "a restricted session sends no notifications: {message}"
    );
    assert!(client.unclaimed.is_empty(), "{:?}", client.unclaimed);
}

/// 制限セッションはユーザーに見せる口 (ステータスバーの「接続中」・右クリックの送信先) の
/// 相手にしない。ACP のエージェントには Claude Code の起動導線が無い。
#[gpui::test]
async fn a_restricted_session_stays_out_of_the_user_facing_surfaces(cx: &mut TestAppContext) {
    let fixture = start_restricted_sandbox_fixture(cx).await;
    let status_item = fixture.workspace.update(cx, |workspace, cx| {
        cx.new(|cx| IdeServerStatusItem::new(workspace, cx))
    });
    let _client = connect(&fixture, cx).await;
    cx.run_until_parked();

    assert!(
        !is_claude_code_connected(&fixture.project, cx),
        "a restricted session is not a connected Claude Code"
    );
    assert_eq!(
        status_item.read_with(cx, |item, cx| item.shown(cx)),
        None,
        "the status item stays quiet for a restricted session"
    );
    assert!(
        cx.update(|cx| {
            crate::sandbox_client::sessions_for_workspace(fixture.workspace.entity_id(), cx)
        })
        .is_empty(),
        "a restricted session is not a target for the right-click send"
    );
}

/// 診断は CLI 用のセッションと同じ扱い: 開いているバッファーだけ、ワークツリーの中だけ。
#[gpui::test]
async fn a_restricted_session_returns_diagnostics_only_from_the_worktree(cx: &mut TestAppContext) {
    let fixture = start_restricted_sandbox_fixture(cx).await;
    set_diagnostic(
        &fixture.project,
        path!("/test/main.rs"),
        "unknown macro",
        cx,
    );
    open_path(&fixture, path!("/test/main.rs"), cx).await;
    // ワークツリーの外のファイルも開いていて診断があるが、制限セッションには見せない。
    open_path(&fixture, path!("/outside/secret.rs"), cx).await;
    set_diagnostic(
        &fixture.project,
        path!("/outside/secret.rs"),
        "hunter2 leaked",
        cx,
    );
    cx.run_until_parked();

    let mut client = connect(&fixture, cx).await;
    let main_uri = format!("file://{CLIENT_ROOT}/main.rs");
    let response = client
        .call_tool("getDiagnostics", json!({ "uri": main_uri }))
        .await;
    assert_eq!(response["result"]["isError"], json!(false), "{response}");
    let files = first_text_as_json(&response);
    assert_eq!(files[0]["uri"], json!(main_uri), "{files}");
    assert_eq!(
        files[0]["diagnostics"][0]["message"],
        json!("unknown macro"),
        "{files}"
    );

    let response = client.call_tool("getDiagnostics", json!({})).await;
    let uris = first_text_as_json(&response)
        .as_array()
        .expect("the diagnostics are an array")
        .iter()
        .filter_map(|file| file["uri"].as_str().map(str::to_string))
        .collect::<Vec<_>>();
    assert_eq!(uris, vec![main_uri]);
    assert!(!response.to_string().contains("hunter2"), "{response}");

    for uri in [
        format!("file://{CLIENT_OUTSIDE}/secret.rs"),
        format!("file://{CLIENT_ROOT}/../outside/secret.rs"),
    ] {
        let response = client
            .call_tool("getDiagnostics", json!({ "uri": uri }))
            .await;
        assert_eq!(
            response["result"]["isError"],
            json!(true),
            "{uri}: {response}"
        );
        assert!(!response.to_string().contains("hunter2"), "{response}");
    }
}

/// ACP のエージェントが実際に使う口は streamable HTTP。トークンの検査は WebSocket と同じ。
#[gpui::test]
async fn a_restricted_session_answers_streamable_http_with_one_tool(cx: &mut TestAppContext) {
    let fixture = start_restricted_sandbox_fixture(cx).await;
    let list = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });

    let response = post_mcp(&fixture, "wrong-token", &list).await;
    assert!(
        response.starts_with("HTTP/1.1 401"),
        "expected a 401, got {response:?}"
    );

    let response = post_mcp(&fixture, &fixture.auth_token, &list).await;
    assert!(
        response.starts_with("HTTP/1.1 200 OK"),
        "expected a 200, got {response:?}"
    );
    let body = http_body(&response);
    assert_eq!(tool_names(&body), vec!["getDiagnostics"], "{body}");
}

/// 同じワークスペースに制限セッションをもう 1 本足す。`fixture` と共有するのはワークスペース
/// (= 診断の出どころ) だけで、接続の口もトークンも別。
fn another_restricted_session(fixture: &SandboxFixture, cx: &mut TestAppContext) -> SandboxFixture {
    let (connections, incoming_connections) = unbounded::<MemoryStream>();
    let (session, info) = cx.update(|cx| {
        crate::start_restricted_sandbox_session(
            fixture.workspace.downgrade(),
            incoming_connections,
            Arc::new(SandboxPaths::new(
                sandbox_mapper(),
                vec![PathBuf::from(path!("/test"))],
            )),
            cx,
        )
    });
    cx.run_until_parked();
    SandboxFixture {
        fs: fixture.fs.clone(),
        project: fixture.project.clone(),
        window: fixture.window,
        workspace: fixture.workspace.clone(),
        connections,
        session: Some(session),
        auth_token: info.auth_token,
    }
}

/// ADR-F49 決定 24: 1 つのワークスペースに制限セッションが 2 本ぶら下がる。同じプロジェクト・
/// 同じエージェントの 2 本目の接続 (`acp.rs` の `restricted_ide_channel` は接続ごとに 1 本
/// 始める。同じフォルダーを開いた 2 つ目のウィンドウ、古い接続をスレッドが握ったままの再起動)
/// がここに来る。
///
/// 2 本は同じワークスペースの診断を見るが、互いの口には入れない: トークンは接続ごとに別で、
/// 片方のトークンでもう片方は 401、片方を畳んでももう片方は生きている。
#[gpui::test]
async fn two_restricted_sessions_on_one_workspace_stay_apart(cx: &mut TestAppContext) {
    let first = start_restricted_sandbox_fixture(cx).await;
    let second = another_restricted_session(&first, cx);
    assert_ne!(
        first.auth_token, second.auth_token,
        "each session answers for itself alone"
    );

    let list = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
    for (session, name, other) in [(&first, "first", &second), (&second, "second", &first)] {
        let response = post_mcp(session, &session.auth_token, &list).await;
        assert!(
            response.starts_with("HTTP/1.1 200 OK"),
            "{name}: expected a 200, got {response:?}"
        );
        assert_eq!(
            tool_names(&http_body(&response)),
            vec!["getDiagnostics"],
            "{name}: and only the one read-only tool"
        );

        let response = post_mcp(session, &other.auth_token, &list).await;
        assert!(
            response.starts_with("HTTP/1.1 401"),
            "{name}: the other session's token is refused, got {response:?}"
        );
    }

    // 接続が閉じたときに終わるのは自分の分だけ (`AcpConnection` の `ide_link`)。
    drop(first);
    cx.run_until_parked();
    let response = post_mcp(&second, &second.auth_token, &list).await;
    assert!(
        response.starts_with("HTTP/1.1 200 OK"),
        "the session that is left over still answers, got {response:?}"
    );
}

/// `tools/list` の要求に、無視される詰め物を載せて本文を `body_bytes` ちょうどにする
/// (詰め物が入らないほど小さい指定なら、詰め物なしの長さ)。
fn padded_tools_list(body_bytes: usize) -> Value {
    let mut request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list",
        "params": { "pad": "" }
    });
    let overhead = request.to_string().len();
    request["params"]["pad"] = json!("a".repeat(body_bytes.saturating_sub(overhead)));
    request
}

/// 制限セッションは同時接続を 16 まで許すぶん、受け取る 1 本文を 1 MiB に抑える。送る側と
/// CLI 用のセッションの上限は据え置き (`openDiff` の本文が通らなくなる)。
#[gpui::test]
async fn a_restricted_session_refuses_an_oversized_post_body(cx: &mut TestAppContext) {
    let oversized = padded_tools_list(sandbox_client::MAX_RESTRICTED_INCOMING_MESSAGE_BYTES + 1);

    let fixture = start_restricted_sandbox_fixture(cx).await;
    let response = post_mcp(&fixture, &fixture.auth_token, &oversized).await;
    assert!(
        response.is_empty(),
        "the restricted session drops the connection instead of reading the body"
    );
    let response = post_mcp(&fixture, &fixture.auth_token, &padded_tools_list(1024)).await;
    assert!(
        response.starts_with("HTTP/1.1 200 OK"),
        "a body within the limit still goes through"
    );
    assert_eq!(tool_names(&http_body(&response)), vec!["getDiagnostics"]);

    let fixture = start_sandbox_fixture(cx).await;
    let response = post_mcp(&fixture, &fixture.auth_token, &oversized).await;
    assert!(
        response.starts_with("HTTP/1.1 200 OK"),
        "the CLI session keeps its own limit"
    );
    assert_eq!(
        http_body(&response)["result"]["tools"]
            .as_array()
            .map(Vec::len),
        Some(4)
    );
}

/// 制限セッションは 1 要求ごとに接続を使い捨てる streamable HTTP を話すので、WebSocket を
/// 1 本張り続ける CLI (`sandbox_session_limits_its_connections`) より上限が多い。
#[gpui::test]
async fn a_restricted_session_takes_more_connections_than_the_cli_session(cx: &mut TestAppContext) {
    let fixture = start_restricted_sandbox_fixture(cx).await;
    let limit = sandbox_client::MAX_RESTRICTED_CONNECTIONS;
    let mut sockets = Vec::new();
    for index in 0..limit {
        match open_websocket(&fixture, &fixture.auth_token).await {
            Ok(socket) => sockets.push(socket),
            Err(error) => panic!("connection {index} is within the limit of {limit}: {error}"),
        }
        cx.run_until_parked();
    }
    assert!(
        open_websocket(&fixture, &fixture.auth_token).await.is_err(),
        "the restricted session refuses a connection beyond its limit"
    );
    assert_eq!(sockets.len(), limit);
}

async fn send_open_diff(client: &mut SandboxClient, path: &str, tab_name: &str) -> u64 {
    client
        .send(
            "tools/call",
            json!({ "name": "openDiff", "arguments": open_diff_arguments(path, tab_name) }),
        )
        .await
}

fn first_text(response: &Value) -> String {
    response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

async fn end_session(fixture: &mut SandboxFixture, cx: &mut TestAppContext) {
    drop(fixture.session.take());
    cx.run_until_parked();
    // 解放された Entity の後始末は、次の更新の終わりに走る。
    cx.update(|_| {});
    cx.run_until_parked();
}

/// 同じタブ名の提案が 2 度来たら、古いタブを閉じて (待っている要求には拒否として返して)
/// 置き換える。どちらにしても、セッションを落とせばタブは 1 枚も残らない。
#[gpui::test]
async fn sandbox_session_replaces_a_duplicate_diff_tab_and_closes_everything_on_drop(
    cx: &mut TestAppContext,
) {
    let mut fixture = start_sandbox_fixture(cx).await;
    let mut client = connect(&fixture, cx).await;
    let main_path = format!("{CLIENT_ROOT}/main.rs");

    let first = send_open_diff(&mut client, &main_path, "proposal").await;
    cx.run_until_parked();
    send_open_diff(&mut client, &main_path, "proposal").await;
    cx.run_until_parked();

    let response = client.response(first).await;
    assert_eq!(first_text(&response), "DIFF_REJECTED", "{response}");
    assert_eq!(open_diff_tabs(&fixture.workspace, cx).len(), 1);

    // 置き換えられた提案について CLI が送る `close_tab` は、置き換えた提案を閉じない。
    let response = client
        .call_tool("close_tab", json!({ "tab_name": "proposal" }))
        .await;
    assert_eq!(response["result"]["isError"], json!(false), "{response}");
    cx.run_until_parked();
    assert_eq!(
        open_diff_tabs(&fixture.workspace, cx).len(),
        1,
        "the replacement stays open"
    );

    end_session(&mut fixture, cx).await;
    assert!(
        open_diff_tabs(&fixture.workspace, cx).is_empty(),
        "no proposal tab outlives the session"
    );
}

#[gpui::test]
async fn sandbox_session_limits_open_diff_tabs(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    let mut client = connect(&fixture, cx).await;
    let main_path = format!("{CLIENT_ROOT}/main.rs");
    for index in 0..4 {
        send_open_diff(&mut client, &main_path, &format!("proposal {index}")).await;
    }
    cx.run_until_parked();
    assert_eq!(open_diff_tabs(&fixture.workspace, cx).len(), 4);

    let response = client
        .call_tool("openDiff", open_diff_arguments(&main_path, "one too many"))
        .await;
    assert_eq!(response["result"]["isError"], json!(true), "{response}");
    assert!(first_text(&response).contains("close one"), "{response}");
    cx.run_until_parked();
    assert_eq!(open_diff_tabs(&fixture.workspace, cx).len(), 4);
}

fn active_item_id(fixture: &SandboxFixture, cx: &mut TestAppContext) -> Option<gpui::EntityId> {
    fixture.workspace.read_with(cx, |workspace, cx| {
        workspace.active_item(cx).map(|item| item.item_id())
    })
}

/// サンドボックスの提案は、ユーザーがフォーカスしているタブと同じペインでもキーボードの
/// フォーカスを取らない。前面に出すと、ペインがフォーカスを新しい前面のタブへ渡してしまう
/// ので、そのときはタブを足すだけにする。
#[gpui::test]
async fn sandbox_diff_tabs_do_not_take_keyboard_focus(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    let mut client = connect(&fixture, cx).await;
    let main_path = format!("{CLIENT_ROOT}/main.rs");
    open_path(&fixture, path!("/test/main.rs"), cx).await;
    let editor = fixture
        .workspace
        .read_with(cx, |workspace, cx| {
            workspace
                .active_item(cx)
                .and_then(|item| item.act_as::<Editor>(cx))
        })
        .expect("the file opened in an editor");

    send_open_diff(&mut client, &main_path, "beside the focused tab").await;
    cx.run_until_parked();
    let first = open_diff_tabs(&fixture.workspace, cx)
        .into_iter()
        .next()
        .expect("the proposal opens");
    let (editor_focused, proposal_focused) = fixture
        .window
        .update(cx, |_, window, cx| {
            (
                editor
                    .read(cx)
                    .focus_handle(cx)
                    .contains_focused(window, cx),
                first.read(cx).focus_handle(cx).contains_focused(window, cx),
            )
        })
        .expect("the window is open");
    assert!(
        editor_focused,
        "the tab the user is typing in keeps the keyboard focus"
    );
    assert!(
        !proposal_focused,
        "keystrokes meant for the terminal must not land in the proposal"
    );
    assert_eq!(
        active_item_id(&fixture, cx),
        Some(editor.entity_id()),
        "the user's tab stays in front"
    );

    // 2 枚目の提案が来ても同じく、フォーカスもユーザーのタブも動かない。
    send_open_diff(&mut client, &main_path, "a second proposal").await;
    cx.run_until_parked();
    let second = open_diff_tabs(&fixture.workspace, cx)
        .into_iter()
        .find(|diff| diff.entity_id() != first.entity_id())
        .expect("the second proposal opens");
    assert_eq!(
        active_item_id(&fixture, cx),
        Some(editor.entity_id()),
        "the user's tab still stays in front"
    );
    let proposal_focused = fixture
        .window
        .update(cx, |_, window, cx| {
            second
                .read(cx)
                .focus_handle(cx)
                .contains_focused(window, cx)
        })
        .expect("the window is open");
    assert!(!proposal_focused);
}

/// ADR-F56: どの口に来たかは要求の頭を読むまで分からないので、CLI の IDE の口の枠 (4) は頭を
/// 読んでから数え、超えた接続は `503` で断る。
#[gpui::test]
async fn sandbox_session_limits_its_connections(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    let mut clients = Vec::new();
    for _ in 0..sandbox_client::MAX_CONNECTIONS {
        clients.push(connect(&fixture, cx).await);
    }

    match open_websocket(&fixture, &fixture.auth_token).await {
        Ok(_) => panic!("a connection beyond the limit must be refused"),
        Err(WebSocketError::Http(response)) => assert_eq!(response.status(), 503),
        Err(error) => panic!("expected a 503, got {error}"),
    }
    assert_eq!(clients.len(), sandbox_client::MAX_CONNECTIONS);

    // 閉じた分は空く。
    drop(clients.pop());
    cx.run_until_parked();
    let _reconnected = connect(&fixture, cx).await;
}

/// ADR-F56: 頭を読む前に受けておけるのは、両方の口の枠の合計まで。それを超えた接続は、
/// 何も読まずにすぐ閉じる (黙ったまま繋ぎっぱなしにする相手に抱え込まれないため)。
#[gpui::test]
async fn sandbox_session_closes_connections_beyond_both_limits_at_once(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    let total = sandbox_client::MAX_CONNECTIONS + crate::assist_tools::MAX_ASSIST_CONNECTIONS;
    let mut silent = Vec::new();
    for _ in 0..total {
        let (server_end, client_end) = memory_stream_pair();
        fixture
            .connections
            .unbounded_send(server_end)
            .expect("the session accepts connections");
        silent.push(client_end);
    }
    cx.run_until_parked();

    let (server_end, client_end) = memory_stream_pair();
    fixture
        .connections
        .unbounded_send(server_end)
        .expect("the session accepts connections");
    cx.run_until_parked();
    // 読んで確かめると、閉じるのが頭の待ち時間の切れたあとでも通ってしまうので、時計を
    // 進めずに相手の端が閉じたかを見る。
    assert!(
        closed_by_the_session(&client_end),
        "the connection beyond both limits is closed at once"
    );
    assert!(
        silent
            .iter()
            .all(|client_end| !closed_by_the_session(client_end)),
        "the connections within the limits wait for their request"
    );
}

fn closed_by_the_session(client_end: &MemoryStream) -> bool {
    client_end
        .incoming
        .lock()
        .expect("the pipe lock is not poisoned")
        .closed
}

#[gpui::test]
async fn a_client_that_never_finishes_the_handshake_is_disconnected(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    let (server_end, mut client_end) = memory_stream_pair();
    fixture
        .connections
        .unbounded_send(server_end)
        .expect("the session accepts connections");
    client_end
        .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n")
        .await
        .expect("writing half a request");
    cx.run_until_parked();

    cx.executor()
        .advance_clock(transport::HANDSHAKE_TIMEOUT + Duration::from_secs(1));
    cx.run_until_parked();
    let mut byte = [0u8; 1];
    assert_eq!(
        client_end.read(&mut byte).await.expect("reading"),
        0,
        "the silent connection is closed"
    );
}

#[gpui::test]
async fn sandbox_session_closes_a_connection_that_sends_an_oversized_message(
    cx: &mut TestAppContext,
) {
    let fixture = start_sandbox_fixture(cx).await;
    let mut client = connect(&fixture, cx).await;
    client
        .socket
        .send(async_tungstenite::tungstenite::Message::text(
            "x".repeat(17 * 1024 * 1024),
        ))
        .await
        .expect("the client can send");
    cx.run_until_parked();
    assert!(
        !matches!(client.socket.next().await, Some(Ok(message)) if !message.is_close()),
        "the connection is closed"
    );
}

#[gpui::test]
async fn sandbox_session_refuses_directories_and_oversized_files(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    fixture
        .fs
        .insert_file(path!("/test/huge.txt"), vec![b'a'; 9 * 1024 * 1024])
        .await;
    fixture
        .fs
        .insert_tree(path!("/test/folder"), json!({}))
        .await;
    cx.run_until_parked();
    let mut client = connect(&fixture, cx).await;

    for (name, reason) in [
        ("huge.txt", "larger than"),
        ("folder", "not a regular file"),
    ] {
        let path = format!("{CLIENT_ROOT}/{name}");
        let response = client
            .call_tool("openDiff", open_diff_arguments(&path, name))
            .await;
        assert_eq!(
            response["result"]["isError"],
            json!(true),
            "{name}: {response}"
        );
        assert!(first_text(&response).contains(reason), "{name}: {response}");
        // 診断の問い合わせはファイルを開かないので、断るまでもなく空で返る。
        let response = client
            .call_tool("getDiagnostics", json!({ "uri": format!("file://{path}") }))
            .await;
        assert_eq!(
            first_text_as_json(&response),
            json!([]),
            "{name}: {response}"
        );
    }
    cx.run_until_parked();
    assert!(open_diff_tabs(&fixture.workspace, cx).is_empty());
}

/// ワークツリーの中を指すリンクでもたどらない。WSL 側で作られたリンクは Windows の
/// `canonicalize` では解決できないことがあるので、リンクそのものを通さない。
#[gpui::test]
async fn sandbox_session_does_not_follow_links_inside_the_worktree(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    fixture
        .fs
        .insert_tree(path!("/test/sub"), json!({ "inner.rs": "fn inner() {}\n" }))
        .await;
    fixture
        .fs
        .insert_symlink(path!("/test/alias"), PathBuf::from(path!("/test/sub")))
        .await;
    fixture
        .fs
        .insert_symlink(
            path!("/test/dangling.rs"),
            PathBuf::from(path!("/nowhere/missing.rs")),
        )
        .await;
    cx.run_until_parked();
    let mut client = connect(&fixture, cx).await;

    for name in ["alias/inner.rs", "dangling.rs"] {
        let path = format!("{CLIENT_ROOT}/{name}");
        let response = client
            .call_tool("openDiff", open_diff_arguments(&path, name))
            .await;
        assert_eq!(
            response["result"]["isError"],
            json!(true),
            "{name}: {response}"
        );
        assert!(first_text(&response).contains("link"), "{name}: {response}");
    }

    // 同じファイルでも、リンクを通らない道なら開ける。
    send_open_diff(
        &mut client,
        &format!("{CLIENT_ROOT}/sub/inner.rs"),
        "direct",
    )
    .await;
    cx.run_until_parked();
    assert_eq!(open_diff_tabs(&fixture.workspace, cx).len(), 1);
}

/// ワークツリーの中にあっても外を指すリンクのファイルは、選択の本文もパスも送らない。
#[gpui::test]
async fn sandbox_session_hides_the_selection_in_a_file_that_links_outside(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    fixture
        .fs
        .insert_symlink(
            path!("/test/link.rs"),
            PathBuf::from(path!("/outside/secret.rs")),
        )
        .await;
    cx.run_until_parked();
    let mut client = connect(&fixture, cx).await;
    let main_path = format!("{CLIENT_ROOT}/main.rs");

    open_path(&fixture, path!("/test/main.rs"), cx).await;
    client
        .notification("selection_changed", |params| {
            params["filePath"] == json!(main_path)
        })
        .await;
    open_path(&fixture, path!("/test/link.rs"), cx).await;
    let notification = client
        .notification("selection_changed", is_cleared_selection)
        .await;
    assert_eq!(notification["params"]["text"], json!(""));
}

#[gpui::test]
async fn sandbox_session_refuses_roots_the_workspace_does_not_have_open(cx: &mut TestAppContext) {
    let fixture =
        start_sandbox_fixture_with_roots(vec![PathBuf::from(path!("/outside"))], cx).await;
    let (server_end, _client_end) = memory_stream_pair();
    assert!(
        fixture.connections.unbounded_send(server_end).is_err(),
        "a refused session takes no connections"
    );
    assert!(!is_claude_code_connected(&fixture.project, cx));
}

/// CLI は自分の cwd の下の絶対パスと突き合わせるので、ホストの診断も絶対パスの URI で返す。
#[gpui::test]
async fn host_diagnostics_report_absolute_file_uris(cx: &mut TestAppContext) {
    let (_fs, workspace, server) = test_workspace_with_fs(cx).await;
    let project = workspace.read_with(cx, |workspace, _| workspace.project().clone());
    set_diagnostic(&project, path!("/test/main.rs"), "unknown macro", cx);
    cx.run_until_parked();

    let response = request(
        &server,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "getDiagnostics", "arguments": {} }
        }),
        cx,
    )
    .await;
    let files = first_text_as_json(&response);
    assert_eq!(
        files[0]["uri"],
        json!(uri!("file:///test/main.rs")),
        "{files}"
    );
}

fn is_open(project: &Entity<Project>, path: &str, cx: &mut TestAppContext) -> bool {
    project.read_with(cx, |project, cx| {
        project
            .find_project_path(Path::new(path), cx)
            .and_then(|project_path| project.get_open_buffer(&project_path, cx))
            .is_some()
    })
}

fn open_diff_message(id: u64, path: &str, tab_name: &str) -> String {
    serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": "openDiff",
            "arguments": {
                "old_file_path": path,
                "new_file_path": path,
                "new_file_contents": "edited\n",
                "tab_name": tab_name,
            }
        }
    }))
    .expect("the request serializes")
}

/// CLI はタブにファイル名だけを付けるので、ホストでは同じ名前の提案が並びうる。一方をタブで
/// 採用したあとに CLI が送る `close_tab` は、もう一方の (まだ決まっていない) 提案を閉じない。
#[gpui::test]
async fn host_diffs_with_the_same_tab_name_coexist(cx: &mut TestAppContext) {
    let (fs, workspace, server) = test_workspace_with_fs(cx).await;
    fs.insert_tree(path!("/test/a"), json!({ "index.ts": "a\n" }))
        .await;
    fs.insert_tree(path!("/test/b"), json!({ "index.ts": "b\n" }))
        .await;
    cx.run_until_parked();

    let first = cx.spawn({
        let server = server.clone();
        let message = open_diff_message(1, path!("/test/a/index.ts"), "index.ts");
        async move |mut cx| {
            crate::handle_message(&server, &message, tools::ConnectionId::next(), &mut cx).await
        }
    });
    cx.run_until_parked();
    let second = cx.spawn({
        let server = server.clone();
        let message = open_diff_message(2, path!("/test/b/index.ts"), "index.ts");
        async move |mut cx| {
            crate::handle_message(&server, &message, tools::ConnectionId::next(), &mut cx).await
        }
    });
    cx.run_until_parked();
    assert_eq!(
        open_diff_tabs(&workspace, cx).len(),
        2,
        "both proposals are shown"
    );

    let first_diff = server
        .update(cx, |server, _| server.open_diffs().oldest_named("index.ts"))
        .expect("the first proposal is registered");
    first_diff.update(cx, |diff, cx| diff.accept(cx));
    cx.run_until_parked();
    let response: Value =
        serde_json::from_str(&first.await.expect("openDiff answers")).expect("valid JSON");
    assert_eq!(
        response["result"]["content"][0]["text"],
        json!("FILE_SAVED")
    );

    // CLI は答えを受け取ると、そのタブ名で `close_tab` を送ってくる。
    let close = request(
        &server,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "close_tab", "arguments": { "tab_name": "index.ts" } }
        }),
        cx,
    )
    .await;
    assert_eq!(close["result"]["isError"], json!(false), "{close}");
    cx.run_until_parked();
    assert_eq!(
        open_diff_tabs(&workspace, cx).len(),
        1,
        "the other proposal stays open"
    );

    let second_diff = server
        .update(cx, |server, _| server.open_diffs().oldest_named("index.ts"))
        .expect("the second proposal is still registered");
    second_diff.update(cx, |diff, cx| diff.accept(cx));
    cx.run_until_parked();
    let response: Value =
        serde_json::from_str(&second.await.expect("openDiff answers")).expect("valid JSON");
    assert_eq!(
        response["result"]["content"][0]["text"],
        json!("FILE_SAVED"),
        "the second edit is not reported as rejected"
    );
}

fn close_tab_message(id: u64, tab_name: &str) -> String {
    serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": "close_tab", "arguments": { "tab_name": tab_name } }
    }))
    .expect("the request serializes")
}

/// `close_tab` は、同じ接続が開いた差分と覚えた名前を先に見る。別の CLI (接続) が同じ名前の
/// 提案をタブで決めたあとでも、自分の提案を閉じられる。
#[gpui::test]
async fn close_tab_prefers_the_diffs_of_its_own_connection(cx: &mut TestAppContext) {
    let (fs, workspace, server) = test_workspace_with_fs(cx).await;
    fs.insert_tree(path!("/test/a"), json!({ "index.ts": "a\n" }))
        .await;
    fs.insert_tree(path!("/test/b"), json!({ "index.ts": "b\n" }))
        .await;
    cx.run_until_parked();
    let first_connection = tools::ConnectionId::next();
    let second_connection = tools::ConnectionId::next();

    let first = cx.spawn({
        let server = server.clone();
        let message = open_diff_message(1, path!("/test/a/index.ts"), "index.ts");
        async move |mut cx| {
            crate::handle_message(&server, &message, first_connection, &mut cx).await
        }
    });
    cx.run_until_parked();
    let second = cx.spawn({
        let server = server.clone();
        let message = open_diff_message(2, path!("/test/b/index.ts"), "index.ts");
        async move |mut cx| {
            crate::handle_message(&server, &message, second_connection, &mut cx).await
        }
    });
    cx.run_until_parked();
    assert_eq!(open_diff_tabs(&workspace, cx).len(), 2);

    // 1 つめの接続の提案をタブで採用する。その CLI の `close_tab` はまだ届いていない。
    let first_diff = server
        .update(cx, |server, _| server.open_diffs().oldest_named("index.ts"))
        .expect("the first proposal is registered");
    first_diff.update(cx, |diff, cx| diff.accept(cx));
    cx.run_until_parked();
    first.await.expect("openDiff answers");

    // 2 つめの接続の CLI が、ターミナルで決まった自分の提案のタブを閉じる。
    let close = cx.spawn({
        let server = server.clone();
        let message = close_tab_message(3, "index.ts");
        async move |mut cx| {
            crate::handle_message(&server, &message, second_connection, &mut cx).await
        }
    });
    cx.run_until_parked();
    close.await.expect("close_tab answers");
    let response: Value =
        serde_json::from_str(&second.await.expect("openDiff answers")).expect("valid JSON");
    assert_eq!(
        response["result"]["content"][0]["text"],
        json!("DIFF_REJECTED")
    );
    assert!(open_diff_tabs(&workspace, cx).is_empty());

    // 1 つめの接続の `close_tab` は、覚えておいた名前を消し込むだけ。
    let close = cx.spawn({
        let server = server.clone();
        let message = close_tab_message(4, "index.ts");
        async move |mut cx| {
            crate::handle_message(&server, &message, first_connection, &mut cx).await
        }
    });
    cx.run_until_parked();
    let response: Value =
        serde_json::from_str(&close.await.expect("close_tab answers")).expect("valid JSON");
    assert_eq!(response["result"]["isError"], json!(false), "{response}");
}

/// サンドボックスの差分の上限は、旧内容を読む前に数える。まだ登録していない準備中の
/// `openDiff` も (同じ名前で置き換えるものも) 数に入る。
#[test]
fn diffs_being_prepared_count_toward_the_limit() {
    let open_diffs = tools::OpenDiffs::default();
    let reservations = (0..4)
        .map(|_| open_diffs.reserve(4).expect("below the limit"))
        .collect::<Vec<_>>();
    assert!(
        open_diffs.reserve(4).is_err(),
        "four proposals are being prepared"
    );
    drop(reservations);
    assert!(
        open_diffs.reserve(4).is_ok(),
        "finished preparations free their slots"
    );
}

/// 接続を数えるのはトランスポートが開いているものだけ。差分の決定を待っているだけの接続が、
/// CLI の繋ぎ直しを締め出さない。
#[gpui::test]
async fn a_connection_waiting_on_a_diff_does_not_block_a_reconnect(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    let mut clients = Vec::new();
    for _ in 0..4 {
        clients.push(connect(&fixture, cx).await);
    }
    let mut waiting = clients.pop().expect("four clients are connected");
    send_open_diff(&mut waiting, &format!("{CLIENT_ROOT}/main.rs"), "proposal").await;
    cx.run_until_parked();
    assert_eq!(open_diff_tabs(&fixture.workspace, cx).len(), 1);

    // CLI が落ちても、その `openDiff` はユーザーの決定を待ち続ける。
    drop(waiting);
    cx.run_until_parked();
    let mut reconnected = connect(&fixture, cx).await;
    let response = reconnected.request("tools/list", json!({})).await;
    assert!(response["result"]["tools"].is_array(), "{response}");
    assert_eq!(open_diff_tabs(&fixture.workspace, cx).len(), 1);
    assert_eq!(clients.len(), 3);
}

/// 読まない相手に溜めすぎた通知は捨てるが、その接続は切らない。捨てたときは送れたことにしない。
#[test]
fn a_dropped_notification_does_not_count_as_sent() {
    let (outbox, _receiver) = transport::outbox();
    let mut connections = vec![outbox];
    for _ in 0..transport::MAX_QUEUED_NOTIFICATIONS {
        assert!(crate::broadcast_notification(
            &mut connections,
            "at_mentioned",
            json!({})
        ));
    }
    assert!(
        !crate::broadcast_notification(&mut connections, "at_mentioned", json!({})),
        "the queue is full"
    );
    assert_eq!(connections.len(), 1, "a slow client is not treated as gone");
}

async fn make_directory_link(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(target, link).is_ok();
    // ジャンクションは管理者でなくても作れる。
    #[cfg(windows)]
    let linked = smol::process::Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(link)
        .arg(target)
        .output()
        .await
        .is_ok_and(|output| output.status.success());
    linked
}

/// サンドボックスの求めでファイルを読むときは、1 つのハンドルを開いて、その実体の場所と種類を
/// 確かめてから同じハンドルで読む (実ファイルで確かめる)。
#[gpui::test]
async fn reads_a_sandbox_file_from_one_verified_handle(cx: &mut TestAppContext) {
    use crate::path_mapping::{SandboxFile, read_file_through_handle};

    // リンクを作る子プロセスを待つ。
    cx.executor().allow_parking();

    // 実ファイルのハンドルは `fs` を使わないので、何でもよい。
    let fs: Arc<dyn project::Fs> = FakeFs::new(cx.executor());
    let directory = tempfile::tempdir().expect("a temporary worktree");
    let root = directory.path();
    std::fs::create_dir(root.join("real")).expect("creating a directory");
    std::fs::write(root.join("real").join("file.txt"), "contents").expect("writing a file");
    let real_root = std::fs::canonicalize(root).expect("the root resolves");
    let read = |relative: &str, limit: u64| {
        read_file_through_handle(&root.join(relative), &real_root.join(relative), limit, &fs)
    };

    assert_eq!(
        read("real/file.txt", 100).expect("the file is read"),
        SandboxFile::Text("contents".to_string())
    );
    assert_eq!(
        read("real/file.txt", 4).expect("the file is inspected"),
        SandboxFile::TooLarge
    );
    assert_eq!(
        read("real/missing.txt", 100).expect("a missing file is fine"),
        SandboxFile::Missing
    );
    assert!(
        !matches!(read("real", 100), Ok(SandboxFile::Text(_))),
        "a directory is not read"
    );

    // ワークツリーの中のリンク (Windows ではジャンクション) を通って開いたものは、行き着いた
    // 先がワークツリーの中でも、行き着くはずの場所と違うので読まない。
    if make_directory_link(&root.join("real"), &root.join("alias")).await {
        assert_eq!(
            read("alias/file.txt", 100).expect("the file is inspected"),
            SandboxFile::Redirected
        );
    } else {
        eprintln!("skipping the link case: could not create a directory link");
    }

    // 最後の成分がリンクのファイルも読まない。Unix では `O_NOFOLLOW` で開けず、Windows では
    // たどった先が行き着くはずの場所と違う。
    let link_target = root.join("real").join("file.txt");
    let link = root.join("real").join("link.txt");
    #[cfg(unix)]
    let file_linked = std::os::unix::fs::symlink(&link_target, &link).is_ok();
    #[cfg(windows)]
    let file_linked = std::os::windows::fs::symlink_file(&link_target, &link).is_ok();
    if file_linked {
        assert_eq!(
            read("real/link.txt", 100).expect("the link is inspected"),
            SandboxFile::Redirected
        );
    } else {
        eprintln!("skipping the file link case: could not create a symbolic link");
    }

    // FIFO は開いても書き手を待たず、通常のファイルでないので読まない。
    #[cfg(unix)]
    {
        let created = smol::process::Command::new("mkfifo")
            .arg(root.join("real").join("fifo"))
            .status()
            .await
            .is_ok_and(|status| status.success());
        assert!(created, "mkfifo creates a FIFO");
        assert_eq!(
            read("real/fifo", 100).expect("the FIFO is inspected"),
            SandboxFile::NotRegular
        );
    }
}

// ---------------------------------------------------------------------------
// アシストのツール (ADR-F56)
// ---------------------------------------------------------------------------

/// 表示の呼び出しを記録するだけの `CodeDisplay` (本物は `ide_assist`)。
#[derive(Default)]
struct FakeDisplay {
    calls: Mutex<Vec<DisplayCall>>,
}

#[derive(Clone, Debug, PartialEq)]
enum DisplayCall {
    Highlights(AssistSource, Vec<CodeHighlight>),
    Annotations(AssistSource, Vec<CodeAnnotation>),
    Tour(AssistSource, CodeTour),
    Clear(AssistSource),
}

impl FakeDisplay {
    fn record(&self, call: DisplayCall) {
        self.calls
            .lock()
            .expect("the call log is not poisoned")
            .push(call);
    }

    fn take_calls(&self) -> Vec<DisplayCall> {
        std::mem::take(&mut *self.calls.lock().expect("the call log is not poisoned"))
    }
}

impl CodeDisplay for FakeDisplay {
    fn show_highlights(
        &self,
        _project: &Entity<Project>,
        source: AssistSource,
        highlights: Vec<CodeHighlight>,
        _cx: &mut gpui::App,
    ) -> gpui::Task<anyhow::Result<DisplayOutcome>> {
        let shown = highlights.len();
        self.record(DisplayCall::Highlights(source, highlights));
        gpui::Task::ready(Ok(DisplayOutcome {
            shown,
            skipped: Vec::new(),
        }))
    }

    fn show_annotations(
        &self,
        _project: &Entity<Project>,
        source: AssistSource,
        annotations: Vec<CodeAnnotation>,
        _cx: &mut gpui::App,
    ) -> gpui::Task<anyhow::Result<DisplayOutcome>> {
        let shown = annotations.len();
        self.record(DisplayCall::Annotations(source, annotations));
        gpui::Task::ready(Ok(DisplayOutcome {
            shown,
            skipped: Vec::new(),
        }))
    }

    fn start_tour(
        &self,
        _project: &Entity<Project>,
        source: AssistSource,
        tour: CodeTour,
        _cx: &mut gpui::App,
    ) -> gpui::Task<anyhow::Result<DisplayOutcome>> {
        let shown = tour.steps.len();
        let skipped = tour
            .steps
            .first()
            .map(|step| SkippedDisplay {
                path: step.path.clone(),
                reason: "テストで飛ばした".to_string(),
            })
            .into_iter()
            .collect();
        self.record(DisplayCall::Tour(source, tour));
        gpui::Task::ready(Ok(DisplayOutcome { shown, skipped }))
    }

    fn clear(&self, _project: &Entity<Project>, source: &AssistSource, _cx: &mut gpui::App) {
        self.record(DisplayCall::Clear(source.clone()));
    }
}

fn install_fake_display(cx: &mut TestAppContext) -> Arc<FakeDisplay> {
    let display = Arc::new(FakeDisplay::default());
    cx.update(|cx| cx.set_global(ide_query::GlobalCodeDisplay(display.clone())));
    display
}

/// アシストのツールの検査に掛かるファイルを足す。
async fn add_assist_files(fixture: &SandboxFixture, cx: &mut TestAppContext) {
    fixture
        .fs
        .insert_tree(
            path!("/test/src"),
            json!({ "lib.rs": "pub fn lib() {}\n", "nested": { "deep.rs": "fn deep() {}\n" } }),
        )
        .await;
    fixture
        .fs
        .insert_file(path!("/test/.env"), b"SECRET=hunter2\n".to_vec())
        .await;
    fixture
        .fs
        .insert_file(path!("/test/huge.txt"), vec![b'a'; 5 * 1024 * 1024])
        .await;
    fixture
        .fs
        .insert_tree(path!("/test/.git"), json!({ "config": "[core]\n" }))
        .await;
    fixture
        .fs
        .insert_symlink(
            path!("/test/link.rs"),
            PathBuf::from(path!("/outside/secret.rs")),
        )
        .await;
    cx.run_until_parked();
}

/// ブリッジに接続を 1 本渡し、`path` に streamable HTTP の MCP 要求を 1 件送って応答をすべて読む。
async fn post_mcp_to(fixture: &SandboxFixture, token: &str, path: &str, request: &Value) -> String {
    let (server_end, mut client_end) = memory_stream_pair();
    fixture
        .connections
        .unbounded_send(server_end)
        .expect("the session accepts connections");
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
    response
}

async fn call_assist_tool(fixture: &SandboxFixture, name: &str, arguments: Value) -> Value {
    let response = post_mcp_to(
        fixture,
        &fixture.auth_token,
        transport::ASSIST_MCP_PATH,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }),
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 200 OK"),
        "{name}: expected a 200, got {response:?}"
    );
    http_body(&response)
}

fn tool_error_text(response: &Value) -> String {
    assert_eq!(response["result"]["isError"], json!(true), "{response}");
    first_text(response)
}

fn project_path(
    project: &Entity<Project>,
    path: &str,
    cx: &mut TestAppContext,
) -> project::ProjectPath {
    project.read_with(cx, |project, cx| {
        project
            .find_project_path(Path::new(path), cx)
            .expect("the path is in a worktree")
    })
}

fn sorted(names: &[&str]) -> Vec<String> {
    let mut names = names
        .iter()
        .map(|name| name.to_string())
        .collect::<Vec<_>>();
    names.sort();
    names
}

/// `/mcp/zed` はサンドボックスのセッションと同じトークン・同じ転送で 9 本を出し、CLI の
/// IDE の口 (WebSocket・`/mcp`) の一覧は変えない。initialize には `instructions` が入る。
#[gpui::test]
async fn the_assist_endpoint_offers_the_nine_tools_beside_the_cli_endpoint(
    cx: &mut TestAppContext,
) {
    let fixture = start_sandbox_fixture(cx).await;
    let list = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });

    let assist = post_mcp_to(
        &fixture,
        &fixture.auth_token,
        transport::ASSIST_MCP_PATH,
        &list,
    )
    .await;
    assert!(assist.starts_with("HTTP/1.1 200 OK"), "{assist:?}");
    assert_eq!(
        tool_names(&http_body(&assist)),
        sorted(&crate::ASSIST_TOOL_NAMES)
    );
    let ide = post_mcp_to(
        &fixture,
        &fixture.auth_token,
        transport::IDE_MCP_PATH,
        &list,
    )
    .await;
    assert_eq!(
        tool_names(&http_body(&ide)),
        vec![
            "closeAllDiffTabs",
            "close_tab",
            "getDiagnostics",
            "openDiff"
        ],
        "the CLI's own tools are unchanged"
    );

    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "initialize",
        "params": { "protocolVersion": "2025-06-18", "capabilities": {} }
    });
    let assist = http_body(
        &post_mcp_to(
            &fixture,
            &fixture.auth_token,
            transport::ASSIST_MCP_PATH,
            &initialize,
        )
        .await,
    );
    let instructions = assist["result"]["instructions"]
        .as_str()
        .expect("the assist endpoint tells the model where it is");
    assert!(instructions.contains("Zed editor"), "{instructions}");
    assert!(instructions.lines().count() <= 10, "{instructions}");
    assert!(instructions.contains("highlightCode") && instructions.contains("searchSymbols"));

    let mut client = connect(&fixture, cx).await;
    let response = client
        .request("initialize", initialize["params"].clone())
        .await;
    assert!(
        response["result"].get("instructions").is_none(),
        "the CLI's `ide` endpoint is not given instructions: {response}"
    );
    assert_eq!(
        tool_names(&client.request("tools/list", json!({})).await).len(),
        4
    );

    for token in ["wrong-token", ""] {
        let response = post_mcp_to(&fixture, token, transport::ASSIST_MCP_PATH, &list).await;
        assert!(
            response.starts_with("HTTP/1.1 401"),
            "{token:?}: expected a 401, got {response:?}"
        );
    }
}

/// アシストの口は streamable HTTP だけ。WebSocket で来たら断る (CLI の IDE の口と取り違えない)。
#[gpui::test]
async fn the_assist_endpoint_does_not_take_websockets(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    let (server_end, client_end) = memory_stream_pair();
    fixture
        .connections
        .unbounded_send(server_end)
        .expect("the session accepts connections");
    let request = async_tungstenite::tungstenite::http::Request::builder()
        .method("GET")
        .uri("ws://127.0.0.1:41234/mcp/zed")
        .header("Host", "127.0.0.1:41234")
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
        .header(
            transport::CLAUDE_CODE_AUTH_HEADER,
            fixture.auth_token.as_str(),
        )
        .body(())
        .expect("building the upgrade request");
    match async_tungstenite::client_async(request, client_end).await {
        Ok(_) => panic!("a WebSocket on the assist endpoint must be refused"),
        Err(WebSocketError::Http(response)) => assert_eq!(response.status(), 404),
        Err(error) => panic!("expected a 404, got {error}"),
    }
}

/// `ide_server.assist_tools` を切れば、アシストの口は理由を返して何もしない。
#[gpui::test]
async fn the_assist_endpoint_follows_the_setting(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    cx.update(|cx| {
        settings::SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.ide_server.get_or_insert_default().assist_tools = Some(false);
            });
        });
    });
    let response = post_mcp_to(
        &fixture,
        &fixture.auth_token,
        transport::ASSIST_MCP_PATH,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    )
    .await;
    let body = http_body(&response);
    assert_eq!(body["error"]["code"], json!(protocol::REFUSED), "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("disabled")),
        "{body}"
    );
}

/// ACP の制限セッションも同じ転送の `/mcp/zed` で 9 本を出す。`/mcp` は `getDiagnostics` のまま。
#[gpui::test]
async fn a_restricted_session_offers_the_assist_endpoint_too(cx: &mut TestAppContext) {
    let fixture = start_restricted_sandbox_fixture(cx).await;
    let list = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
    let assist = post_mcp_to(
        &fixture,
        &fixture.auth_token,
        transport::ASSIST_MCP_PATH,
        &list,
    )
    .await;
    assert_eq!(
        tool_names(&http_body(&assist)),
        sorted(&crate::ASSIST_TOOL_NAMES)
    );
    let ide = post_mcp_to(
        &fixture,
        &fixture.auth_token,
        transport::IDE_MCP_PATH,
        &list,
    )
    .await;
    assert_eq!(tool_names(&http_body(&ide)), vec!["getDiagnostics"]);
}

/// アシストの口の同時接続は、CLI の IDE の口の枠 (4) とは別に 16 本まで。
#[gpui::test]
async fn the_assist_endpoint_counts_its_connections_apart(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    let mut clients = Vec::new();
    for _ in 0..sandbox_client::MAX_CONNECTIONS {
        clients.push(connect(&fixture, cx).await);
    }
    // CLI の枠が埋まっていても、アシストの口は答える。
    let response = post_mcp_to(
        &fixture,
        &fixture.auth_token,
        transport::ASSIST_MCP_PATH,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response:?}");
    assert_eq!(clients.len(), sandbox_client::MAX_CONNECTIONS);
    // 頭を読む前の総数 (両方の枠の合計) に掛からないよう、CLI の接続は閉じておく。
    drop(clients);
    cx.run_until_parked();

    // 本文を送らずに枠を握る接続を、アシストの口に上限まで並べる。
    let mut held = Vec::new();
    for _ in 0..crate::assist_tools::MAX_ASSIST_CONNECTIONS {
        let (server_end, mut client_end) = memory_stream_pair();
        fixture
            .connections
            .unbounded_send(server_end)
            .expect("the session accepts connections");
        client_end
            .write_all(
                format!(
                    "POST /mcp/zed HTTP/1.1\r\nHost: 127.0.0.1\r\n\
                     Authorization: Bearer {}\r\nContent-Length: 10\r\n\r\n",
                    fixture.auth_token
                )
                .as_bytes(),
            )
            .await
            .expect("writing a head");
        held.push(client_end);
    }
    cx.run_until_parked();
    let response = post_mcp_to(
        &fixture,
        &fixture.auth_token,
        transport::ASSIST_MCP_PATH,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 503"),
        "the assist endpoint refuses beyond its own limit: {response:?}"
    );
    // アシストの口が埋まっていても、CLI の IDE の口は繋がる。
    let _client = connect(&fixture, cx).await;
    assert_eq!(held.len(), crate::assist_tools::MAX_ASSIST_CONNECTIONS);
}

/// 表示のツールは、検査を通したパスを `ProjectPath` にして、このセッションを出どころとして
/// 渡す。絶対パス (CLI から見えるパス)・ルートからの相対パス・ルートの名前から始まる相対パスを
/// 受ける。セッションが終わると、描いたものを消す。
#[gpui::test]
async fn the_display_tools_pass_project_paths_and_the_session_as_the_source(
    cx: &mut TestAppContext,
) {
    let mut fixture = start_sandbox_fixture(cx).await;
    add_assist_files(&fixture, cx).await;
    let display = install_fake_display(cx);
    let main_path = project_path(&fixture.project, path!("/test/main.rs"), cx);
    let lib_path = project_path(&fixture.project, path!("/test/src/lib.rs"), cx);
    let deep_path = project_path(&fixture.project, path!("/test/src/nested/deep.rs"), cx);

    let response = call_assist_tool(
        &fixture,
        "highlightCode",
        json!({ "items": [
            { "path": format!("{CLIENT_ROOT}/main.rs"), "startLine": 1, "endLine": 2, "note": "入口" },
            { "path": "src/lib.rs", "startLine": 1, "endLine": 1 },
            { "path": "test/src/nested/deep.rs", "startLine": 1, "endLine": 1 },
        ] }),
    )
    .await;
    assert_eq!(response["result"]["isError"], json!(false), "{response}");
    assert_eq!(first_text_as_json(&response)["shown"], json!(3));
    let calls = display.take_calls();
    let [DisplayCall::Highlights(source, highlights)] = calls.as_slice() else {
        panic!("one highlight call: {calls:?}");
    };
    assert_eq!(source.label.as_ref(), "Claude Code");
    assert!(source.id.starts_with("zed-ide-session-"), "{source:?}");
    assert_eq!(
        highlights,
        &vec![
            CodeHighlight {
                path: main_path.clone(),
                lines: LineRange {
                    start_line: 1,
                    end_line: 2
                },
                note: Some("入口".to_string()),
            },
            CodeHighlight {
                path: lib_path.clone(),
                lines: LineRange {
                    start_line: 1,
                    end_line: 1
                },
                note: None,
            },
            CodeHighlight {
                path: deep_path,
                lines: LineRange {
                    start_line: 1,
                    end_line: 1
                },
                note: None,
            },
        ]
    );
    let session_source = source.clone();

    let response = call_assist_tool(
        &fixture,
        "annotateCode",
        json!({ "items": [
            { "path": "main.rs", "startLine": 2, "endLine": 3, "markdown": "**説明**" },
        ] }),
    )
    .await;
    assert_eq!(response["result"]["isError"], json!(false), "{response}");
    assert_eq!(
        display.take_calls(),
        vec![DisplayCall::Annotations(
            session_source.clone(),
            vec![CodeAnnotation {
                path: main_path.clone(),
                lines: LineRange {
                    start_line: 2,
                    end_line: 3
                },
                markdown: "**説明**".to_string(),
            }]
        )]
    );

    let response = call_assist_tool(
        &fixture,
        "startCodeTour",
        json!({ "title": "ツアー", "steps": [
            { "path": "main.rs", "startLine": 1, "endLine": 1, "title": "1", "markdown": "a" },
            { "path": "src/lib.rs", "startLine": 1, "endLine": 1, "title": "2", "markdown": "b" },
        ] }),
    )
    .await;
    let outcome = first_text_as_json(&response);
    assert_eq!(outcome["shown"], json!(2));
    assert_eq!(
        outcome["skipped"],
        json!([{ "path": format!("{CLIENT_ROOT}/main.rs"), "reason": "テストで飛ばした" }]),
        "skipped paths are shown the way the CLI sees them"
    );
    let calls = display.take_calls();
    let [DisplayCall::Tour(source, tour)] = calls.as_slice() else {
        panic!("one tour call: {calls:?}");
    };
    assert_eq!(source, &session_source);
    assert_eq!(tour.title, "ツアー");
    assert_eq!(
        tour.steps
            .iter()
            .map(|step| step.path.clone())
            .collect::<Vec<_>>(),
        vec![main_path, lib_path]
    );

    let response = call_assist_tool(&fixture, "clearAnnotations", json!({})).await;
    assert_eq!(response["result"]["isError"], json!(false), "{response}");
    assert_eq!(
        display.take_calls(),
        vec![DisplayCall::Clear(session_source.clone())]
    );

    end_session(&mut fixture, cx).await;
    assert!(
        display
            .take_calls()
            .contains(&DisplayCall::Clear(session_source)),
        "ending the session clears what it drew"
    );
}

/// どの読み取り・表示のツールも、既存の検査と `private_files` の拒否を通してから動く。
/// 断る理由は日本語の短文で、中身 (hunter2) は出さない。表示には何も渡さない。
#[gpui::test]
async fn the_assist_tools_refuse_what_the_checks_refuse(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    add_assist_files(&fixture, cx).await;
    let display = install_fake_display(cx);

    let refused = [
        (format!("{CLIENT_OUTSIDE}/secret.rs"), "outside"),
        (
            format!("{CLIENT_ROOT}/../outside/secret.rs"),
            "contains `..`",
        ),
        (format!("{CLIENT_ROOT}/link.rs"), "link"),
        (format!("{CLIENT_ROOT}/.git/config"), "doesn't scan"),
        (format!("{CLIENT_ROOT}/.env"), "private_files"),
        (".env".to_string(), "private_files"),
        (format!("{CLIENT_ROOT}/huge.txt"), "4 MiB"),
        (format!("{CLIENT_ROOT}/src"), "is a directory"),
        (format!("{CLIENT_ROOT}/missing.rs"), "wasn't found"),
        ("../outside/secret.rs".to_string(), "contains `..`"),
        ("src/../../outside/secret.rs".to_string(), "contains `..`"),
        (String::new(), "is empty"),
    ];
    for (path, reason) in &refused {
        for (tool, arguments) in [
            (
                "highlightCode",
                json!({ "items": [{ "path": path, "startLine": 1, "endLine": 1 }] }),
            ),
            (
                "annotateCode",
                json!({ "items": [{ "path": path, "startLine": 1, "endLine": 1, "markdown": "x" }] }),
            ),
            ("getHover", json!({ "path": path, "line": 1 })),
            ("getDocumentSymbols", json!({ "path": path })),
            (
                "findDefinition",
                json!({ "path": path, "line": 1, "symbol": "main" }),
            ),
            (
                "findReferences",
                json!({ "path": path, "line": 1, "column": 4 }),
            ),
        ] {
            let response = call_assist_tool(&fixture, tool, arguments).await;
            let text = tool_error_text(&response);
            assert!(text.contains(reason), "{tool} {path:?}: {text}");
            assert!(!response.to_string().contains("hunter2"), "{response}");
        }
    }
    assert!(
        display.take_calls().is_empty(),
        "nothing refused reaches the editor"
    );

    // 検査を通ったパスは言語サーバーの問い合わせ (`ide_query`) まで届く。言語サーバーの
    // 居ないこのテストでは失敗するが、それは検査で断ったのではない。
    let response = call_assist_tool(
        &fixture,
        "getHover",
        json!({ "path": "main.rs", "line": 1 }),
    )
    .await;
    let text = tool_error_text(&response);
    assert!(
        !text.contains("can't be used") && !text.contains("wasn't found"),
        "{text}"
    );
}

/// CLI に渡す引数には秘密も空白も無い。`Authorization` の値は丸ごと環境変数から CLI が展開する
/// (Windows PowerShell 5.1 と npm の `claude.ps1` は空白を含む引数を割り、CLI が起動しない)。
#[test]
fn the_cli_arguments_hold_no_secret_and_no_blank() {
    let arguments = crate::cli_assist_arguments("http://127.0.0.1:4000/mcp/zed");
    let [config_option, config, allowed_option, allowed] = arguments.as_slice() else {
        panic!("four arguments: {arguments:?}");
    };
    assert_eq!(config_option, "--mcp-config");
    assert_eq!(allowed_option, "--allowedTools");
    for argument in &arguments {
        assert!(!argument.contains(char::is_whitespace), "{argument:?}");
    }
    let config: Value = serde_json::from_str(config).expect("the config is JSON");
    let server = &config["mcpServers"][crate::MCP_SERVER_NAME];
    assert_eq!(
        server["headers"]["Authorization"],
        json!(format!("${{{}}}", crate::ASSIST_BEARER_ENV_VAR))
    );
    assert_eq!(allowed.split(',').count(), crate::ASSIST_TOOL_NAMES.len());
}

/// `ide_query` の失敗の見せ方: 言語サーバーが居ないことはそれと分かる短文で、ほかはサンドボックス
/// には `ide_query` の一文だけ (原因の連鎖 = ホストのエラー文は見せない)、ホストには全部。
#[test]
fn query_failures_tell_a_missing_language_server_apart() {
    use crate::assist_tools::query_failure;
    let not_running = || {
        anyhow::anyhow!(ide_query::LanguageServerNotRunning {
            path: "test/main.rs".to_string(),
        })
    };
    let sandbox_scope = tools::ToolScope::Assist(Some(Arc::new(session_paths())));
    for (sandboxed, scope) in [
        (true, sandbox_scope.clone()),
        (false, tools::ToolScope::Assist(None)),
    ] {
        let text = tools::error_text(
            "getHover",
            &query_failure(sandboxed, "getHover", not_running()),
            &scope,
        );
        assert!(text.contains("The language server isn't running"), "{text}");
        assert!(
            text.contains("then call again"),
            "the agent is told to wait and ask again: {text}"
        );
        assert!(!text.contains("test/main.rs"), "{text}");
    }

    let with_cause = || {
        anyhow::Error::msg("C:/Users/secret/host-only.rs")
            .context("`test/main.rs` の 3 行目に `main` がありません")
    };
    let sandboxed = tools::error_text(
        "findDefinition",
        &query_failure(true, "findDefinition", with_cause()),
        &sandbox_scope,
    );
    assert_eq!(sandboxed, "`test/main.rs` の 3 行目に `main` がありません");
    let host = tools::error_text(
        "findDefinition",
        &query_failure(false, "findDefinition", with_cause()),
        &tools::ToolScope::Assist(None),
    );
    assert!(host.contains("host-only.rs"), "{host}");
}

/// query 担当の上限の前提: hover は 8,000 文字で切り (文字の境界で)、searchSymbols の limit は
/// 既定 50・上限 100、getDocumentSymbols は 500 件で切る。`ide_query` が空で返した中身は
/// 空のまま (キーごと出さない)。
#[gpui::test]
async fn read_results_are_cut_to_their_limits(cx: &mut TestAppContext) {
    use crate::assist_tools::{hover_json, search_limit};

    let short = hover_json(Some("型: `u32`".to_string()));
    assert_eq!(short["hover"], json!("型: `u32`"));
    assert_eq!(short["truncated"], json!(false));
    let long = hover_json(Some("あ".repeat(8_001)));
    assert_eq!(
        long["hover"].as_str().map(|hover| hover.chars().count()),
        Some(8_000)
    );
    assert_eq!(long["truncated"], json!(true));
    assert_eq!(
        hover_json(None),
        json!({ "hover": null, "truncated": false })
    );

    assert_eq!(search_limit(&json!({})).expect("the default"), 50);
    assert_eq!(search_limit(&json!({ "limit": 7 })).expect("as asked"), 7);
    assert_eq!(search_limit(&json!({ "limit": 500 })).expect("capped"), 100);
    assert!(search_limit(&json!({ "limit": "ten" })).is_err());

    let fixture = start_sandbox_fixture(cx).await;
    let main_path = project_path(&fixture.project, path!("/test/main.rs"), cx);
    let hits = (0..600)
        .map(|index| SymbolHit {
            name: format!("symbol_{index}"),
            kind: "function".to_string(),
            container: None,
            path: main_path.clone(),
            lines: LineRange {
                start_line: 1,
                end_line: 1,
            },
            signature: if index == 0 {
                String::new()
            } else {
                "fn main()".to_string()
            },
            in_project: true,
            score: 1.0,
        })
        .collect::<Vec<_>>();
    let project = fixture.project.clone();
    let mut async_cx = cx.to_async();
    let formatted = crate::assist_tools::format_symbols_for_test(
        project.clone(),
        crate::assist_tools::AssistPaths::Host,
        hits,
        &mut async_cx,
    )
    .await;
    assert_eq!(
        formatted["symbols"].as_array().map(Vec::len),
        Some(500),
        "getDocumentSymbols stops at 500"
    );
    assert_eq!(formatted["truncated"], json!(true));
    assert!(
        formatted["symbols"][0].get("signature").is_none(),
        "an empty signature stays empty"
    );
    assert_eq!(formatted["symbols"][1]["signature"], json!("fn main()"));

    let formatted = crate::assist_tools::format_locations_for_test(
        project,
        crate::assist_tools::AssistPaths::Host,
        vec![QueryLocation {
            path: main_path,
            lines: LineRange {
                start_line: 2,
                end_line: 2,
            },
            in_project: true,
            preview: String::new(),
        }],
        &mut async_cx,
    )
    .await;
    assert!(
        formatted["locations"][0].get("preview").is_none(),
        "a preview `ide_query` left empty is not filled in: {formatted}"
    );
}

/// 件数・文字数の上限と、行の形。上限を超えたら理由を返して何もしない。
#[gpui::test]
async fn the_assist_tools_enforce_their_limits(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    let display = install_fake_display(cx);
    let item = json!({ "path": "main.rs", "startLine": 1, "endLine": 1 });
    let annotation = json!({ "path": "main.rs", "startLine": 1, "endLine": 1, "markdown": "x" });
    let step =
        json!({ "path": "main.rs", "startLine": 1, "endLine": 1, "title": "t", "markdown": "x" });
    let too_long = "あ".repeat(ide_query::MAX_DISPLAY_MARKDOWN_CHARS + 1);

    for (tool, arguments, reason) in [
        (
            "highlightCode",
            json!({ "items": vec![item.clone(); ide_query::MAX_HIGHLIGHTS + 1] }),
            "items (it had",
        ),
        ("highlightCode", json!({ "items": [] }), "is empty"),
        (
            "highlightCode",
            json!({ "items": [{ "path": "main.rs", "startLine": 1, "endLine": 1, "note": too_long }] }),
            "characters (it was",
        ),
        (
            "highlightCode",
            json!({ "items": [{ "path": "main.rs", "startLine": 3, "endLine": 2 }] }),
            "endLine",
        ),
        (
            "highlightCode",
            json!({ "items": [{ "path": "main.rs", "startLine": 0, "endLine": 2 }] }),
            "startLine",
        ),
        (
            "annotateCode",
            json!({ "items": vec![annotation; ide_query::MAX_ANNOTATIONS + 1] }),
            "items (it had",
        ),
        (
            "annotateCode",
            json!({ "items": [{ "path": "main.rs", "startLine": 1, "endLine": 1, "markdown": too_long }] }),
            "characters (it was",
        ),
        (
            "startCodeTour",
            json!({ "title": "t", "steps": vec![step; ide_query::MAX_TOUR_STEPS + 1] }),
            "items (it had",
        ),
        (
            "searchSymbols",
            json!({ "query": "main", "limit": 0 }),
            "limit",
        ),
        (
            "searchSymbols",
            json!({ "query": "x".repeat(1001) }),
            "characters (it was",
        ),
        ("searchSymbols", json!({ "query": " " }), "is empty"),
        ("getHover", json!({ "path": "main.rs", "line": 0 }), "line"),
        (
            "getHover",
            json!({ "path": "main.rs", "line": 1, "column": 0 }),
            "column",
        ),
    ] {
        let response = call_assist_tool(&fixture, tool, arguments).await;
        let text = tool_error_text(&response);
        assert!(text.contains(reason), "{tool}: {text}");
    }
    assert!(display.take_calls().is_empty());
}

/// 読み取り結果は、プロジェクトの中で検査を通るファイルのものだけ中身 (プレビュー・
/// シグネチャ) を返す。プロジェクトの外 (依存ライブラリのソース)・`private_files`・リンクの先は
/// パスと行だけ。パスは CLI から見える形。
#[gpui::test]
async fn read_results_carry_contents_only_from_checked_project_files(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    add_assist_files(&fixture, cx).await;
    let (outside_worktree, _) = fixture
        .project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/outside/secret.rs"), false, cx)
        })
        .await
        .expect("the dependency's file opens as an invisible worktree");
    cx.run_until_parked();
    let outside_path = outside_worktree.read_with(cx, |worktree, _| project::ProjectPath {
        worktree_id: worktree.id(),
        path: util::rel_path::RelPath::empty_arc(),
    });
    let location = |path: project::ProjectPath, in_project: bool, preview: &str| QueryLocation {
        path,
        lines: LineRange {
            start_line: 1,
            end_line: 1,
        },
        in_project,
        preview: preview.to_string(),
    };
    // セッションのルートに入っていない、プロジェクトの別の (見えている) ワークツリー。
    fixture
        .fs
        .insert_tree(
            path!("/other"),
            json!({ "visible.rs": "// hunter2
" }),
        )
        .await;
    fixture
        .project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/other"), true, cx)
        })
        .await
        .expect("a second visible worktree");
    cx.run_until_parked();
    let main_path = project_path(&fixture.project, path!("/test/main.rs"), cx);
    let env_path = project_path(&fixture.project, path!("/test/.env"), cx);
    let link_path = project_path(&fixture.project, path!("/test/link.rs"), cx);
    let other_path = project_path(&fixture.project, path!("/other/visible.rs"), cx);
    let locations = vec![
        location(main_path.clone(), true, "fn main() {"),
        // 言語サーバーはプロジェクトの外と言ったが、中身は出さない。
        location(
            outside_path.clone(),
            false,
            "const SECRET: &str = \"hunter2\";",
        ),
        // プロジェクトの中でも `private_files` とリンクの先は出さない。
        location(env_path, true, "SECRET=hunter2"),
        location(link_path, true, "const SECRET: &str = \"hunter2\";"),
        // セッションのルートの外 (サンドボックスから見えないワークツリー) も出さない。
        location(other_path, true, "// hunter2"),
    ];

    let paths = session_paths();
    let project = fixture.project.clone();
    let mut async_cx = cx.to_async();
    let request = tools::SandboxRequest::new(&paths, &project, &mut async_cx).await;
    let formatted = crate::assist_tools::format_locations_for_test(
        project.clone(),
        crate::assist_tools::AssistPaths::Sandbox(request),
        locations.clone(),
        &mut async_cx,
    )
    .await;
    assert!(!formatted.to_string().contains("hunter2"), "{formatted}");
    let results = formatted["locations"]
        .as_array()
        .expect("the locations are an array");
    assert_eq!(results.len(), 5);
    assert_eq!(results[0]["path"], json!(format!("{CLIENT_ROOT}/main.rs")));
    assert_eq!(results[0]["preview"], json!("fn main() {"));
    assert_eq!(results[0]["inProject"], json!(true));
    for result in &results[1..] {
        assert_eq!(result["inProject"], json!(false), "{result}");
        assert!(result.get("preview").is_none(), "{result}");
        assert_eq!(result["startLine"], json!(1));
    }
    assert_eq!(formatted["truncated"], json!(false));

    // シンボルも同じ: シグネチャは検査を通ったファイルのものだけ。
    let hit = |path: project::ProjectPath, in_project: bool, signature: &str| SymbolHit {
        name: "main".to_string(),
        kind: "function".to_string(),
        container: None,
        path,
        lines: LineRange {
            start_line: 1,
            end_line: 3,
        },
        signature: signature.to_string(),
        in_project,
        score: 1.0,
    };
    let request = tools::SandboxRequest::new(&paths, &project, &mut async_cx).await;
    let formatted = crate::assist_tools::format_symbols_for_test(
        project.clone(),
        crate::assist_tools::AssistPaths::Sandbox(request),
        vec![
            hit(main_path, true, "fn main()"),
            hit(outside_path, false, "hunter2"),
        ],
        &mut async_cx,
    )
    .await;
    assert!(!formatted.to_string().contains("hunter2"), "{formatted}");
    assert_eq!(formatted["symbols"][0]["signature"], json!("fn main()"));
    assert_eq!(formatted["symbols"][0]["name"], json!("main"));
    assert!(formatted["symbols"][1].get("signature").is_none());

    // ホストのクライアントも中身の扱いは同じで、パスはホストの絶対パス。サンドボックスの
    // セッションのルートは関係なく、プロジェクトの別のワークツリーの中身は出す。
    let formatted = crate::assist_tools::format_locations_for_test(
        project,
        crate::assist_tools::AssistPaths::Host,
        locations,
        &mut async_cx,
    )
    .await;
    let results = formatted["locations"]
        .as_array()
        .expect("the locations are an array");
    assert!(
        !serde_json::to_string(&results[..4])
            .expect("serializing")
            .contains("hunter2"),
        "{formatted}"
    );
    assert_eq!(results[0]["path"], json!(path!("/test/main.rs")));
    assert_eq!(results[4]["preview"], json!("// hunter2"));
}

/// ADR-F53 と同じく、リモートのプロジェクトではアシストのツールのパスも接続先
/// (`verify_agent_read`) に確かめさせる。リンクは断り、正規のパスは表示へ渡す。
#[gpui::test]
async fn remote_assist_paths_are_checked_on_the_host(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    add_assist_files(&fixture, cx).await;
    let display = install_fake_display(cx);
    let source = AssistSource {
        id: "remote".into(),
        label: "Claude Code".into(),
    };
    let paths = session_paths();
    let main_path = project_path(&fixture.project, path!("/test/main.rs"), cx);

    for (path, refused) in [
        (format!("{CLIENT_ROOT}/main.rs"), false),
        (format!("{CLIENT_ROOT}/link.rs"), true),
        (format!("{CLIENT_ROOT}/.env"), true),
        (format!("{CLIENT_OUTSIDE}/secret.rs"), true),
    ] {
        let request = remote_request(&paths, &fixture.project, cx).await;
        let project = fixture.project.clone();
        let mut async_cx = cx.to_async();
        let result = crate::assist_tools::run_assist_tool_with(
            "highlightCode",
            json!({ "items": [{ "path": path, "startLine": 1, "endLine": 1 }] }),
            project,
            crate::assist_tools::AssistPaths::Sandbox(request),
            &source,
            &mut async_cx,
        )
        .await;
        assert_eq!(result.is_err(), refused, "{path}: {result:?}");
    }
    assert_eq!(
        display.take_calls(),
        vec![DisplayCall::Highlights(
            source,
            vec![CodeHighlight {
                path: main_path,
                lines: LineRange {
                    start_line: 1,
                    end_line: 1
                },
                note: None,
            }]
        )]
    );
}

fn client_id_of(client: &crate::HostAssistClient, cx: &mut TestAppContext) -> String {
    cx.update(|cx| client.registration.read(cx).client_id.clone())
}

/// ホストのサーバーは `enabled` でなくても、アシストのクライアントが居るあいだだけ動く。
/// IDE の口 (WebSocket・`/mcp`・ロックファイル) は閉じたままで、クライアントを落とすと
/// 描いたものを消してサーバーも止まる。
#[gpui::test]
async fn a_host_assist_client_runs_the_server_without_opening_the_ide_endpoint(
    cx: &mut TestAppContext,
) {
    let lock_directory = tempfile::tempdir().expect("a temporary lock directory");
    let fixture = start_sandbox_fixture(cx).await;
    cx.update(|cx| crate::set_lock_directory(lock_directory.path().to_path_buf(), cx));
    let display = install_fake_display(cx);
    let server = cx
        .update(|cx| crate::server_for_project(&fixture.project, cx))
        .expect("the workspace has a server");
    assert!(server.read_with(cx, |server, _| server.running.is_none()));

    let client = cx
        .update(|cx| crate::host_assist_client(&fixture.project, "Claude Code", cx))
        .expect("a client registers");
    cx.run_until_parked();
    let (endpoint, agent_endpoint, source) = cx.update(|cx| {
        (
            client.assist_endpoint(cx).expect("the assist endpoint"),
            client.agent_endpoint(cx).expect("the agent's endpoint"),
            client.source(cx).expect("the client's source"),
        )
    });
    assert!(endpoint.url.ends_with("/mcp/zed"), "{}", endpoint.url);
    assert_eq!(
        agent_endpoint, endpoint,
        "without `enabled` an agent gets the assist tools alone"
    );
    assert_eq!(source.label.as_ref(), "Claude Code");
    server.read_with(cx, |server, _| {
        assert!(server.running.is_some(), "the server runs for the client");
        assert_eq!(server.port(), None, "the IDE endpoint stays closed");
        assert_eq!(server.lock_file_path(), None);
    });
    assert!(lock_files(lock_directory.path()).is_empty());
    assert!(
        cx.update(|cx| crate::terminal_environment(&fixture.project, cx))
            .is_empty(),
        "terminals are not pointed at an IDE endpoint that is closed"
    );

    let client_id = Some(client_id_of(&client, cx));
    let route = |endpoint, kind| transport::Route {
        kind,
        endpoint,
        client: client_id.clone(),
    };
    let gate = server.read_with(cx, |server, _| server.connection_gate());
    assert!(
        gate.admit(&route(
            transport::Endpoint::Ide,
            transport::ConnectionKind::WebSocket
        ))
        .is_err(),
        "a CLI cannot open the IDE endpoint without `enabled`"
    );
    assert!(
        gate.admit(&route(
            transport::Endpoint::Assist,
            transport::ConnectionKind::Http
        ))
        .is_ok()
    );
    let scope = server
        .read_with(cx, |server, cx| {
            server.connection_scope(
                &route(transport::Endpoint::Assist, transport::ConnectionKind::Http),
                cx,
            )
        })
        .expect("the assist endpoint is open");
    assert_eq!(scope.assist.as_ref(), Some(&source));
    assert_eq!(
        tool_names(&json!({ "result": tools::list_tools_result(&scope) })),
        sorted(&crate::ASSIST_TOOL_NAMES)
    );

    // `enabled` を入れると IDE の口が開く。HTTP の `/mcp` には全ツールとアシストのツール、
    // CLI の WebSocket にはこれまでどおり全ツールだけ。
    cx.update(|cx| {
        settings::SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.ide_server.get_or_insert_default().enabled = Some(true);
            });
        });
    });
    cx.run_until_parked();
    let (http_scope, websocket_scope) = server.read_with(cx, |server, cx| {
        (
            server.connection_scope(
                &route(transport::Endpoint::Ide, transport::ConnectionKind::Http),
                cx,
            ),
            server.connection_scope(
                &route(
                    transport::Endpoint::Ide,
                    transport::ConnectionKind::WebSocket,
                ),
                cx,
            ),
        )
    });
    let full = tools::tools().len();
    let http_scope = http_scope.expect("the IDE endpoint is open");
    assert_eq!(http_scope.assist.as_ref(), Some(&source));
    assert_eq!(
        tool_names(&json!({ "result": tools::list_tools_result(&http_scope) })).len(),
        full + crate::ASSIST_TOOL_NAMES.len()
    );
    assert_eq!(
        tool_names(&json!({ "result": tools::list_tools_result(&websocket_scope.expect("open")) }))
            .len(),
        full
    );
    assert!(server.read_with(cx, |server, _| server.port()).is_some());
    let whole_set = cx
        .update(|cx| client.agent_endpoint(cx))
        .expect("the agent's endpoint");
    assert!(
        whole_set.url.ends_with("/mcp"),
        "with `enabled` an agent gets the whole set: {}",
        whole_set.url
    );
    assert_eq!(
        whole_set.authorization, endpoint.authorization,
        "with the client's own token"
    );
    assert_eq!(lock_files(lock_directory.path()).len(), 1);

    // `enabled` を切っても、クライアントが居れば同じポートとトークンで待ち受け続ける。
    cx.update(|cx| {
        settings::SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.ide_server.get_or_insert_default().enabled = Some(false);
            });
        });
    });
    cx.run_until_parked();
    assert_eq!(
        cx.update(|cx| client.assist_endpoint(cx)),
        Some(endpoint.clone()),
        "the port and token a running CLI holds stay valid"
    );
    assert!(lock_files(lock_directory.path()).is_empty());

    // `assist_tools` を切って戻しても、クライアントが居るあいだは待ち受けを続ける (口だけ閉じる)
    // ので、起動済みの CLI が持っているポートとトークンがそのまま使える。
    set_ide_server_settings(cx, |settings| settings.assist_tools = Some(false));
    assert_eq!(cx.update(|cx| client.assist_endpoint(cx)), None);
    let gate = server.read_with(cx, |server, _| {
        assert!(server.running.is_some(), "still listening for the client");
        server.connection_gate()
    });
    assert!(
        gate.admit(&route(
            transport::Endpoint::Assist,
            transport::ConnectionKind::Http
        ))
        .is_err(),
        "the assist endpoint is closed meanwhile"
    );
    set_ide_server_settings(cx, |settings| settings.assist_tools = Some(true));
    assert_eq!(
        cx.update(|cx| client.assist_endpoint(cx)),
        Some(endpoint),
        "the same port and token after turning the tools back on"
    );

    drop(client);
    cx.run_until_parked();
    cx.update(|_| {});
    cx.run_until_parked();
    assert!(
        display.take_calls().contains(&DisplayCall::Clear(source)),
        "dropping the client clears what it drew"
    );
    assert!(
        server.read_with(cx, |server, _| server.running.is_none()),
        "nothing needs the server any more"
    );
}

/// ADR-F56: ホストのサーバーはアシストのクライアントを URL の申告ではなくトークンで見分ける。
/// クライアントごとに別のトークンを配り、そのトークンからそのクライアントの出どころが決まる。
/// 登録を外したクライアントのトークンは受けず、サーバー自身のトークンは「外部エージェント」。
#[gpui::test]
async fn each_host_assist_client_is_known_by_its_own_token(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    let server = cx
        .update(|cx| crate::server_for_project(&fixture.project, cx))
        .expect("the workspace has a server");
    let first = cx
        .update(|cx| crate::host_assist_client(&fixture.project, "Claude Code", cx))
        .expect("a client registers");
    let second = cx
        .update(|cx| crate::host_assist_client(&fixture.project, "Gemini CLI", cx))
        .expect("another client registers");
    cx.run_until_parked();
    let (first_endpoint, second_endpoint) = cx.update(|cx| {
        (
            first
                .assist_endpoint(cx)
                .expect("the first client's endpoint"),
            second
                .assist_endpoint(cx)
                .expect("the second client's endpoint"),
        )
    });
    assert_eq!(first_endpoint.url, second_endpoint.url, "one server");
    assert_ne!(first_endpoint.authorization, second_endpoint.authorization);

    let (credentials, server_token) = server.read_with(cx, |server, _| {
        let running = server.running.as_ref().expect("running for the clients");
        (
            transport::Credentials::Host {
                server_token: running.auth_token.clone(),
                client_tokens: server.client_tokens.clone(),
            },
            running.auth_token.to_string(),
        )
    });
    let token_of = |endpoint: &crate::McpEndpoint| {
        endpoint
            .authorization
            .strip_prefix("Bearer ")
            .expect("a bearer token")
            .to_string()
    };
    let label_for = |token: &str, cx: &mut TestAppContext| {
        let client = credentials.authenticate(token).expect("a token it takes");
        server.read_with(cx, |server, cx| {
            server
                .connection_scope(
                    &transport::Route {
                        kind: transport::ConnectionKind::Http,
                        endpoint: transport::Endpoint::Assist,
                        client,
                    },
                    cx,
                )
                .expect("the assist endpoint is open")
                .assist
                .expect("with the assist tools")
                .label
        })
    };
    assert_eq!(
        label_for(&token_of(&first_endpoint), cx).as_ref(),
        "Claude Code"
    );
    assert_eq!(
        label_for(&token_of(&second_endpoint), cx).as_ref(),
        "Gemini CLI"
    );
    assert_eq!(label_for(&server_token, cx).as_ref(), "External Agent");
    assert_eq!(credentials.authenticate(&client_id_of(&first, cx)), None);

    drop(first);
    cx.run_until_parked();
    cx.update(|_| {});
    cx.run_until_parked();
    assert_eq!(
        credentials.authenticate(&token_of(&first_endpoint)),
        None,
        "a client that went away takes its token with it"
    );
    assert_eq!(
        label_for(&token_of(&second_endpoint), cx).as_ref(),
        "Gemini CLI"
    );
}

/// エージェントパネルの CLI のタブへの選択範囲: そのタブのセッションに CLI が繋がっていれば
/// `at_mentioned` で送り、繋がっていなければ CLI から見えるパスを返す (`@path#L` を貼る)。
/// ワークツリーの外のファイルは CLI から見えないので、送りも貼りもしない。
#[gpui::test]
async fn a_selection_goes_to_the_cli_of_its_own_tab(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    let (connections, incoming_connections) = unbounded::<MemoryStream>();
    let (session, info) = cx.update(|cx| {
        crate::start_sandbox_session_for_tab(
            fixture.workspace.downgrade(),
            incoming_connections,
            sandbox_mapper(),
            vec![PathBuf::from(path!("/test"))],
            "claude-tab".to_string(),
            cx,
        )
    });
    cx.run_until_parked();
    let tab = SandboxFixture {
        fs: fixture.fs.clone(),
        project: fixture.project.clone(),
        window: fixture.window,
        workspace: fixture.workspace.clone(),
        connections,
        session: Some(session),
        auth_token: info.auth_token,
    };
    let main = Path::new(path!("/test/main.rs"));
    let mention = |tab_key: Option<&str>, path: &Path, cx: &mut TestAppContext| {
        let project = fixture.project.clone();
        cx.update(|cx| crate::mention_selection_in_cli(tab_key, &project, path, 0, 2, cx))
    };

    assert_eq!(
        mention(Some("claude-tab"), main, cx),
        crate::CliMention::Paste {
            cli_path: Some(format!("{CLIENT_ROOT}/main.rs"))
        },
        "not connected: the CLI's own path, to paste"
    );
    assert_eq!(
        mention(
            Some("claude-tab"),
            Path::new(path!("/outside/secret.rs")),
            cx
        ),
        crate::CliMention::Unreachable
    );
    assert_eq!(
        mention(None, main, cx),
        crate::CliMention::Paste { cli_path: None },
        "a tab outside the sandbox, with no host server running"
    );

    let mut client = connect(&tab, cx).await;
    assert_eq!(
        mention(Some("claude-tab"), main, cx),
        crate::CliMention::Sent
    );
    let notification = client.notification("at_mentioned", |_| true).await;
    assert_eq!(
        notification["params"],
        json!({ "filePath": format!("{CLIENT_ROOT}/main.rs"), "lineStart": 0, "lineEnd": 2 })
    );
    // 別のタブ (キー違い) の選択は、このセッションの CLI には送らない。
    assert_eq!(
        mention(Some("claude-other"), main, cx),
        crate::CliMention::Paste { cli_path: None }
    );
}

/// ADR-F56: `private_files` (`.env` など) は、サンドボックスの CLI には選択範囲も診断も見せない。
#[gpui::test]
async fn private_files_stay_out_of_the_sandboxed_cli(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    add_assist_files(&fixture, cx).await;
    let mut client = connect(&fixture, cx).await;

    open_path(&fixture, path!("/test/.env"), cx).await;
    let notification = client
        .notification("selection_changed", is_cleared_selection)
        .await;
    assert_eq!(notification["params"]["text"], json!(""));

    set_diagnostic(&fixture.project, path!("/test/.env"), "hunter2 leaked", cx);
    set_diagnostic(
        &fixture.project,
        path!("/test/main.rs"),
        "unknown macro",
        cx,
    );
    open_path(&fixture, path!("/test/main.rs"), cx).await;
    cx.run_until_parked();
    let response = client.call_tool("getDiagnostics", json!({})).await;
    assert!(!response.to_string().contains("hunter2"), "{response}");
    assert!(response.to_string().contains("unknown macro"), "{response}");
}

/// query 担当の前提 (4): 古いホストから届いたリモートの項目は `is_private` を持たない (false) ので、
/// `private_files` の判定は項目の旗ではなく設定のパターンでも行う。
#[gpui::test]
async fn private_files_are_judged_by_the_settings_not_only_the_entry(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    add_assist_files(&fixture, cx).await;
    let env_path = project_path(&fixture.project, path!("/test/.env"), cx);
    let main_path = project_path(&fixture.project, path!("/test/main.rs"), cx);
    cx.update(|cx| {
        assert!(
            tools::is_private_file(&env_path, None, cx),
            "without the entry's flag, the pattern still matches"
        );
        assert!(!tools::is_private_file(&main_path, None, cx));
    });
}

/// 表示の枠の見出し (`AssistSource.label`) は Zed が決めた名前だけ。MCP の `clientInfo.name`
/// やツールの引数で「Zed」を名乗っても見出しには出ず、登録の無い `client=` は固定の
/// 「外部エージェント」になる。
#[gpui::test]
async fn the_frame_label_is_never_what_the_client_claims(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    let display = install_fake_display(cx);
    let response = post_mcp_to(
        &fixture,
        &fixture.auth_token,
        transport::ASSIST_MCP_PATH,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "Zed", "version": "1.0.0" }
            }
        }),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response:?}");
    let response = call_assist_tool(
        &fixture,
        "highlightCode",
        json!({
            "label": "Zed",
            "source": { "id": "x", "label": "Zed" },
            "items": [{ "path": "main.rs", "startLine": 1, "endLine": 1 }]
        }),
    )
    .await;
    assert_eq!(response["result"]["isError"], json!(false), "{response}");
    let calls = display.take_calls();
    let [DisplayCall::Highlights(source, _)] = calls.as_slice() else {
        panic!("one highlight call: {calls:?}");
    };
    assert_eq!(source.label.as_ref(), "Claude Code");

    let server = cx
        .update(|cx| crate::server_for_project(&fixture.project, cx))
        .expect("the workspace has a server");
    let _client = cx
        .update(|cx| crate::host_assist_client(&fixture.project, "Claude Code", cx))
        .expect("a client registers");
    cx.run_until_parked();
    for forged in [
        Some("Zed".to_string()),
        Some("not-registered".to_string()),
        None,
    ] {
        let scope = server.read_with(cx, |server, cx| {
            server.connection_scope(
                &transport::Route {
                    kind: transport::ConnectionKind::Http,
                    endpoint: transport::Endpoint::Assist,
                    client: forged.clone(),
                },
                cx,
            )
        });
        let label = scope
            .expect("the assist endpoint is open")
            .assist
            .expect("with the assist tools")
            .label;
        assert_eq!(label.as_ref(), "External Agent", "{forged:?}");
    }
}

// ---------------------------------------------------------------------------
// CLI の「⧉ In <ファイル>」を消す (selection_changed の消す通知)
// ---------------------------------------------------------------------------

/// CLI (2.1.281) は `filePath` を `string().optional()` で検査するので、`null` を送ると通知ごと
/// 捨てられる。消す通知はキーを省き、選択範囲 (CLI は無いと何もしない) は付ける。
fn is_cleared_selection(params: &Value) -> bool {
    params.get("filePath").is_none()
        && params["selection"]["start"].is_object()
        && params["selection"]["end"].is_object()
        && params["text"] == json!("")
}

#[test]
fn the_selection_notification_leaves_out_a_missing_path() {
    assert!(is_cleared_selection(
        &crate::SelectionSnapshot::cleared_notification_params()
    ));
    let snapshot = crate::SelectionSnapshot {
        file_path: None,
        text: "fn".to_string(),
        start_line: 0,
        start_character: 0,
        end_line: 0,
        end_character: 2,
    };
    let untitled = snapshot.notification_params(None);
    assert!(untitled.get("filePath").is_none(), "{untitled}");
    assert_eq!(untitled["text"], json!("fn"));
    let with_path = snapshot.notification_params(Some("/p/main.rs"));
    assert_eq!(with_path["filePath"], json!("/p/main.rs"));
}

#[test]
fn a_bare_cursor_is_cleared_when_the_open_file_is_not_shared() {
    let cursor = crate::SelectionSnapshot {
        file_path: Some(PathBuf::from("/p/main.rs")),
        text: String::new(),
        start_line: 3,
        start_character: 1,
        end_line: 3,
        end_character: 1,
    };
    assert_eq!(
        cursor.notification_params_for_cli(Some("/p/main.rs"), true)["filePath"],
        json!("/p/main.rs")
    );
    assert!(is_cleared_selection(
        &cursor.notification_params_for_cli(Some("/p/main.rs"), false)
    ));
    let lines = crate::SelectionSnapshot {
        text: "a\nb\n".to_string(),
        end_line: 5,
        end_character: 0,
        ..cursor
    };
    let params = lines.notification_params_for_cli(Some("/p/main.rs"), false);
    assert_eq!(params["filePath"], json!("/p/main.rs"));
    assert_eq!(params["text"], json!("a\nb\n"));
}

fn set_ide_server_settings(
    cx: &mut TestAppContext,
    update: impl FnOnce(&mut settings::IdeServerSettingsContent),
) {
    cx.update(|cx| {
        settings::SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                update(settings.ide_server.get_or_insert_default());
            });
        });
    });
    cx.run_until_parked();
}

/// ホストのサーバーを `enabled` にしたワークスペース。サンドボックスのセッションは落とす。
async fn start_host_fixture(
    cx: &mut TestAppContext,
) -> (SandboxFixture, Entity<IdeServer>, tempfile::TempDir) {
    let lock_directory = tempfile::tempdir().expect("a temporary lock directory");
    let mut fixture = start_sandbox_fixture(cx).await;
    drop(fixture.session.take());
    cx.update(|cx| crate::set_lock_directory(lock_directory.path().to_path_buf(), cx));
    let server = cx
        .update(|cx| crate::server_for_project(&fixture.project, cx))
        .expect("the workspace has a server");
    set_ide_server_settings(cx, |settings| settings.enabled = Some(true));
    assert!(server.read_with(cx, |server, _| server.port().is_some()));
    (fixture, server, lock_directory)
}

/// ホストのサーバーに WebSocket の CLI が繋がったことにする。
fn attach_host_cli(
    server: &Entity<IdeServer>,
    cx: &mut TestAppContext,
) -> transport::OutboxReceiver {
    let (outbox, receiver) = transport::outbox();
    server.update(cx, |server, cx| server.add_connection(outbox, cx));
    receiver
}

/// 届いている `selection_changed` の中身 (未読のうち最新の 1 件だけが残る)。
fn received_selections(receiver: &mut transport::OutboxReceiver) -> Vec<Value> {
    let mut selections = Vec::new();
    while let Some(Some(text)) = receiver.next().now_or_never() {
        let message: Value = serde_json::from_str(&text).expect("the notification is JSON");
        if message["method"] == json!("selection_changed") {
            selections.push(message["params"].clone());
        }
    }
    selections
}

fn close_all_items(fixture: &SandboxFixture, cx: &mut TestAppContext) {
    let workspace = fixture.workspace.clone();
    let close = fixture
        .window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.active_pane().update(cx, |pane, cx| {
                    pane.close_all_items(&workspace::CloseAllItems::default(), window, cx)
                })
            })
        })
        .expect("the window is open");
    cx.run_until_parked();
    drop(close);
    cx.run_until_parked();
}

fn open_non_editor_item(fixture: &SandboxFixture, cx: &mut TestAppContext) {
    let workspace = fixture.workspace.clone();
    fixture
        .window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                let item = cx.new(workspace::item::test::TestItem::new);
                workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
            })
        })
        .expect("the window is open");
    cx.run_until_parked();
}

fn active_editor(fixture: &SandboxFixture, cx: &mut TestAppContext) -> Entity<Editor> {
    fixture
        .workspace
        .read_with(cx, |workspace, cx| {
            workspace
                .active_item(cx)
                .and_then(|item| item.act_as::<Editor>(cx))
        })
        .expect("an editor is active")
}

fn select(fixture: &SandboxFixture, range: std::ops::Range<PointUtf16>, cx: &mut TestAppContext) {
    let editor = active_editor(fixture, cx);
    fixture
        .window
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                editor.change_selections(Default::default(), window, cx, |selections| {
                    selections.select_ranges([range])
                });
            })
        })
        .expect("the window is open");
    cx.run_until_parked();
}

fn last_file_path(selections: &[Value]) -> Option<Value> {
    selections.last().map(|params| params["filePath"].clone())
}

#[gpui::test]
async fn closing_every_tab_clears_the_open_file_in_the_host_cli(cx: &mut TestAppContext) {
    let (fixture, server, _lock_directory) = start_host_fixture(cx).await;
    let mut cli = attach_host_cli(&server, cx);

    open_path(&fixture, path!("/test/main.rs"), cx).await;
    let selections = received_selections(&mut cli);
    assert_eq!(
        last_file_path(&selections),
        Some(json!(path!("/test/main.rs"))),
        "{selections:?}"
    );

    close_all_items(&fixture, cx);
    let selections = received_selections(&mut cli);
    assert!(
        selections.last().is_some_and(is_cleared_selection),
        "closing every tab clears the CLI's `In main.rs`: {selections:?}"
    );
    assert!(
        server.read_with(cx, |server, _| server.latest_selection().is_some()),
        "getLatestSelection still answers with the last selection it saw"
    );

    // 同じ状態では撃ち直さない。
    server.update(cx, |server, cx| server.watch_active_editor(cx));
    cx.run_until_parked();
    assert_eq!(received_selections(&mut cli), Vec::<Value>::new());

    open_path(&fixture, path!("/test/main.rs"), cx).await;
    let selections = received_selections(&mut cli);
    assert_eq!(
        last_file_path(&selections),
        Some(json!(path!("/test/main.rs"))),
        "reopening the file shows it again: {selections:?}"
    );

    // エディターでない項目 (ターミナル・プレビューなど) に切り替えても消す。
    open_non_editor_item(&fixture, cx);
    let selections = received_selections(&mut cli);
    assert!(
        selections.last().is_some_and(is_cleared_selection),
        "{selections:?}"
    );
}

#[gpui::test]
async fn a_newly_connected_host_cli_gets_the_latest_selection(cx: &mut TestAppContext) {
    let (fixture, server, _lock_directory) = start_host_fixture(cx).await;
    open_path(&fixture, path!("/test/main.rs"), cx).await;

    let mut cli = attach_host_cli(&server, cx);
    cx.run_until_parked();
    assert_eq!(
        received_selections(&mut cli),
        Vec::<Value>::new(),
        "nothing is sent before the CLI registers its handler"
    );
    cx.executor()
        .advance_clock(crate::RESEND_SELECTION_AFTER_CONNECT);
    cx.run_until_parked();
    let selections = received_selections(&mut cli);
    assert_eq!(
        last_file_path(&selections),
        Some(json!(path!("/test/main.rs"))),
        "{selections:?}"
    );
}

#[gpui::test]
async fn the_host_shares_only_selected_lines_when_the_open_file_is_not_shared(
    cx: &mut TestAppContext,
) {
    let (fixture, server, _lock_directory) = start_host_fixture(cx).await;
    set_ide_server_settings(cx, |settings| settings.share_active_file = Some(false));
    let mut cli = attach_host_cli(&server, cx);

    open_path(&fixture, path!("/test/main.rs"), cx).await;
    let selections = received_selections(&mut cli);
    assert!(
        selections.iter().all(is_cleared_selection),
        "a bare cursor does not name the file: {selections:?}"
    );
    assert!(server.read_with(cx, |server, _| {
        server.last_sent.as_ref().is_some_and(is_cleared_selection)
    }));

    select(&fixture, PointUtf16::new(0, 0)..PointUtf16::new(1, 0), cx);
    let selections = received_selections(&mut cli);
    let last = selections.last().expect("the selected lines are sent");
    assert_eq!(last["filePath"], json!(path!("/test/main.rs")));
    assert_eq!(last["text"], json!("fn main() {\n"));

    select(&fixture, PointUtf16::new(0, 0)..PointUtf16::new(0, 0), cx);
    let selections = received_selections(&mut cli);
    assert!(
        selections.last().is_some_and(is_cleared_selection),
        "{selections:?}"
    );
}

#[gpui::test]
async fn turning_off_sharing_clears_the_host_cli_at_once(cx: &mut TestAppContext) {
    let (fixture, server, _lock_directory) = start_host_fixture(cx).await;
    let mut cli = attach_host_cli(&server, cx);
    open_path(&fixture, path!("/test/main.rs"), cx).await;
    let selections = received_selections(&mut cli);
    assert_eq!(
        last_file_path(&selections),
        Some(json!(path!("/test/main.rs")))
    );

    set_ide_server_settings(cx, |settings| settings.share_active_file = Some(false));
    let selections = received_selections(&mut cli);
    assert!(
        selections.last().is_some_and(is_cleared_selection),
        "{selections:?}"
    );

    set_ide_server_settings(cx, |settings| settings.share_active_file = Some(true));
    let selections = received_selections(&mut cli);
    assert_eq!(
        last_file_path(&selections),
        Some(json!(path!("/test/main.rs"))),
        "{selections:?}"
    );
}

/// もう届いている `selection_changed` のうち `matches` に合うもの。待たない (届かないときに
/// テストが止まらず、失敗として返るように)。
fn arrived_selection(
    client: &mut SandboxClient,
    matches: impl Fn(&Value) -> bool,
) -> Option<Value> {
    client
        .notification("selection_changed", matches)
        .now_or_never()
}

#[gpui::test]
async fn sandbox_session_clears_the_open_file_when_every_tab_closes(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    let mut client = connect(&fixture, cx).await;
    let main_path = format!("{CLIENT_ROOT}/main.rs");

    open_path(&fixture, path!("/test/main.rs"), cx).await;
    arrived_selection(&mut client, |params| params["filePath"] == json!(main_path))
        .expect("opening the file names it");
    client.unclaimed.clear();

    close_all_items(&fixture, cx);
    arrived_selection(&mut client, is_cleared_selection)
        .expect("closing every tab clears the CLI's `In main.rs`");
}

#[gpui::test]
async fn a_newly_connected_sandboxed_cli_gets_the_latest_selection(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    let main_path = format!("{CLIENT_ROOT}/main.rs");
    open_path(&fixture, path!("/test/main.rs"), cx).await;

    let mut client = connect(&fixture, cx).await;
    assert!(
        arrived_selection(&mut client, |_| true).is_none(),
        "nothing is sent before the CLI registers its handler"
    );
    cx.executor()
        .advance_clock(crate::RESEND_SELECTION_AFTER_CONNECT);
    cx.run_until_parked();
    arrived_selection(&mut client, |params| params["filePath"] == json!(main_path))
        .expect("the latest selection is sent again after the handshake");
}

#[gpui::test]
async fn the_sandboxed_cli_gets_only_selected_lines_when_the_open_file_is_not_shared(
    cx: &mut TestAppContext,
) {
    let fixture = start_sandbox_fixture(cx).await;
    let mut client = connect(&fixture, cx).await;
    let main_path = format!("{CLIENT_ROOT}/main.rs");
    open_path(&fixture, path!("/test/main.rs"), cx).await;
    arrived_selection(&mut client, |params| params["filePath"] == json!(main_path))
        .expect("opening the file names it");
    client.unclaimed.clear();

    set_ide_server_settings(cx, |settings| settings.share_active_file = Some(false));
    arrived_selection(&mut client, is_cleared_selection)
        .expect("turning sharing off clears the CLI at once");

    select(&fixture, PointUtf16::new(0, 0)..PointUtf16::new(1, 0), cx);
    let notification =
        arrived_selection(&mut client, |params| params["filePath"] == json!(main_path))
            .expect("selected lines are still sent");
    assert_eq!(notification["params"]["text"], json!("fn main() {\n"));
}

/// 届いている `selection_changed` をすべて読む (待たない)。
fn all_arrived_selections(client: &mut SandboxClient) -> Vec<Value> {
    let mut selections = Vec::new();
    while let Some(notification) = arrived_selection(client, |_| true) {
        selections.push(notification["params"].clone());
    }
    selections
}

/// 接続直後の送り直しも、見せてはいけないファイルの検査を通った通知だけを送る。
#[gpui::test]
async fn a_newly_connected_sandboxed_cli_is_not_told_about_a_private_file(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    add_assist_files(&fixture, cx).await;
    open_path(&fixture, path!("/test/.env"), cx).await;
    select(&fixture, PointUtf16::new(0, 0)..PointUtf16::new(1, 0), cx);

    let mut client = connect(&fixture, cx).await;
    cx.executor()
        .advance_clock(crate::RESEND_SELECTION_AFTER_CONNECT);
    cx.run_until_parked();
    let selections = all_arrived_selections(&mut client);
    assert!(
        !selections.is_empty() && selections.iter().all(is_cleared_selection),
        "the resend clears the CLI instead of naming `.env`: {selections:?}"
    );
    assert!(
        !format!("{selections:?}").contains("hunter2"),
        "{selections:?}"
    );
}

/// 共有を切った状態で始まったセッションでも、入れ直したらすぐファイルが CLI に出る。
#[gpui::test]
async fn a_sandboxed_session_started_with_sharing_off_follows_turning_it_on(
    cx: &mut TestAppContext,
) {
    let mut fixture = start_sandbox_fixture(cx).await;
    drop(fixture.session.take());
    cx.run_until_parked();
    set_ide_server_settings(cx, |settings| settings.share_active_file = Some(false));
    let (connections, incoming_connections) = unbounded::<MemoryStream>();
    let (session, info) = cx.update(|cx| {
        crate::start_sandbox_session(
            fixture.workspace.downgrade(),
            incoming_connections,
            sandbox_mapper(),
            vec![PathBuf::from(path!("/test"))],
            cx,
        )
    });
    cx.run_until_parked();
    let fixture = SandboxFixture {
        connections,
        session: Some(session),
        auth_token: info.auth_token,
        ..fixture
    };
    let mut client = connect(&fixture, cx).await;
    let main_path = format!("{CLIENT_ROOT}/main.rs");

    open_path(&fixture, path!("/test/main.rs"), cx).await;
    assert!(
        arrived_selection(&mut client, |params| params.get("filePath").is_some()).is_none(),
        "a bare cursor does not name the file while sharing is off"
    );
    client.unclaimed.clear();

    set_ide_server_settings(cx, |settings| settings.share_active_file = Some(true));
    arrived_selection(&mut client, |params| params["filePath"] == json!(main_path))
        .expect("turning sharing on names the open file at once");
}

/// サンドボックスの CLI だけが繋がっているときの項目 (「Claude Code (サンドボックス): 接続中」)
/// からも、開いているファイルを伝えるかどうかを切り替えられる。
#[gpui::test]
async fn the_sandbox_status_item_menu_turns_off_sharing_the_open_file(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    let status_item = fixture.workspace.update(cx, |workspace, cx| {
        cx.new(|cx| IdeServerStatusItem::new(workspace, cx))
    });
    let _client = connect(&fixture, cx).await;
    let shown = status_item.read_with(cx, |item, cx| item.shown(cx));
    assert_eq!(shown, Some(Shown::SandboxConnection));
    let menu_builder = status_item.read_with(cx, |item, cx| {
        item.menu_builder(Shown::SandboxConnection, cx)
    });

    let menu_window = cx
        .update(|cx| {
            cx.open_window(Default::default(), |window, cx| {
                menu_builder(window, cx).expect("the menu builds")
            })
        })
        .expect("the menu window opens");
    cx.run_until_parked();
    menu_window
        .update(cx, |menu, window, cx| {
            window.focus(&menu.focus_handle(cx), cx);
            assert!(
                menu.select_last(window, cx).is_some(),
                "the menu has the sharing toggle"
            );
        })
        .expect("the menu window is open");
    cx.run_until_parked();
    menu_window
        .update(cx, |_, window, cx| {
            let confirm = cx
                .build_action("menu::Confirm", None)
                .expect("the menu actions are registered");
            window.dispatch_action(confirm, cx);
        })
        .expect("the menu window is open");
    cx.run_until_parked();

    let fs: Arc<dyn project::Fs> = fixture.fs.clone();
    let settings_text = settings::SettingsStore::load_settings(&fs)
        .await
        .expect("the settings file is readable");
    let settings =
        <settings::SettingsContent as settings::RootUserSettings>::parse_json_with_comments(
            &settings_text,
        )
        .expect("the settings file parses");
    assert_eq!(
        settings
            .ide_server
            .and_then(|ide_server| ide_server.share_active_file),
        Some(false),
        "{settings_text}"
    );
}

// ---------------------------------------------------------------------------
// 拒否の理由 / 失敗したタスクの出力 / 全体診断の続き
// ---------------------------------------------------------------------------

#[test]
fn a_rejection_carries_the_reason_only_when_one_was_written() {
    assert_eq!(
        tools::rejected_diff_content(None),
        vec![json!({ "type": "text", "text": "DIFF_REJECTED" })]
    );
    assert_eq!(
        tools::rejected_diff_content(Some("   ")),
        vec![json!({ "type": "text", "text": "DIFF_REJECTED" })],
        "a blank reason adds nothing"
    );
    let content = tools::rejected_diff_content(Some("  keep the old name  "));
    assert_eq!(content.len(), 2, "{content:?}");
    assert_eq!(
        content[0]["text"],
        json!("DIFF_REJECTED"),
        "the CLI only looks at the first text"
    );
    assert_eq!(
        content[1]["text"],
        json!("The user rejected this change. Their reason: keep the old name")
    );
}

#[gpui::test]
async fn rejecting_a_diff_hands_the_written_reason_back(cx: &mut TestAppContext) {
    let (_fs, _workspace, server) = test_workspace_with_fs(cx).await;
    let message = open_diff_message(1, path!("/test/main.rs"), "main.rs");
    let response = cx.spawn({
        let server = server.clone();
        async move |mut cx| {
            crate::handle_message(&server, &message, tools::ConnectionId::next(), &mut cx).await
        }
    });
    cx.run_until_parked();

    let diff = server
        .update(cx, |server, _| server.open_diffs().oldest_named("main.rs"))
        .expect("the diff tab is registered while the CLI waits");
    let window = *cx.windows().first().expect("the workspace window is open");
    cx.update_window(window, |_, window, cx| {
        diff.update(cx, |diff, cx| {
            diff.set_rejection_reason(" use the existing helper instead ", window, cx)
        })
    })
    .expect("the workspace window is open");
    diff.update(cx, |diff, cx| diff.reject(cx));
    cx.run_until_parked();

    let response: Value =
        serde_json::from_str(&response.await.expect("openDiff produced a response"))
            .expect("the response is valid JSON");
    let content = &response["result"]["content"];
    assert_eq!(content[0]["text"], json!("DIFF_REJECTED"), "{response}");
    assert_eq!(
        content[1]["text"],
        json!("The user rejected this change. Their reason: use the existing helper instead"),
        "{response}"
    );
}

#[test]
fn keeps_the_end_of_the_task_output_without_splitting_a_character() {
    let (output, truncated) = tools::task_output_tail("\n\nbuild   \nfailed  \n\n\n", 4096);
    assert_eq!(output, "build\nfailed");
    assert!(!truncated);

    // 全体 6016 バイトなので、末尾 4096 バイトの境目 (1920) は 3 バイトの「あ」の 2 バイト目に来る。
    let long = format!("head\n{}\nlast line!", "あ".repeat(2000));
    let (output, truncated) = tools::task_output_tail(&long, 4096);
    assert!(truncated);
    assert!(
        output.len() <= 4096 && output.len() > 4000,
        "{}",
        output.len()
    );
    assert!(
        output.ends_with("last line!"),
        "the end of the output is kept"
    );
    assert!(!output.contains("head"), "the start is dropped");
}

fn task_terminal(task_id: &str, cx: &mut TestAppContext) -> Entity<terminal::Terminal> {
    let spawned_task = task::SpawnInTerminal {
        id: task::TaskId(task_id.to_string()),
        full_label: "cargo test".to_string(),
        label: "cargo test".to_string(),
        command_label: "cargo test -p demo".to_string(),
        ..Default::default()
    };
    cx.new(|cx| {
        let mut terminal = terminal::TerminalBuilder::new_display_only(
            terminal::terminal_settings::CursorShape::default(),
            terminal::terminal_settings::AlternateScroll::On,
            None,
            0,
            cx.background_executor(),
            util::paths::PathStyle::local(),
        )
        .subscribe(cx);
        terminal.start_task_for_test(spawned_task);
        terminal
    })
}

fn finish_task(
    workspace: &Entity<Workspace>,
    task_id: &str,
    exit_code: i32,
    cx: &mut TestAppContext,
) {
    workspace.update(cx, |_, cx| {
        cx.emit(workspace::Event::TaskFinished {
            task_id: task::TaskId(task_id.to_string()),
            label: "cargo test".to_string(),
            command_label: "cargo test -p demo".to_string(),
            success: exit_code == 0,
            exit_code: Some(exit_code),
        })
    });
    cx.run_until_parked();
}

fn get_last_task_output_request() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "getLastTaskOutput", "arguments": {} }
    })
}

#[gpui::test]
async fn a_failed_task_leaves_its_output_for_get_last_task_output(cx: &mut TestAppContext) {
    let (_fs, workspace, server) = test_workspace_with_fs(cx).await;
    let project = workspace.read_with(cx, |workspace, _| workspace.project().clone());

    let response = request(&server, get_last_task_output_request(), cx).await;
    assert_eq!(
        first_text_as_json(&response)["success"],
        json!(false),
        "{response}"
    );

    let terminal = task_terminal("demo-task", cx);
    project.update(cx, |project, _| {
        project.add_terminal_handle_for_test(&terminal)
    });
    terminal.update(cx, |terminal, cx| {
        terminal.write_output(
            b"running 3 tests\r\nerror[E0425]: cannot find value `x`\r\n",
            cx,
        );
        terminal.finish_task_for_test(101, cx);
    });
    finish_task(&workspace, "demo-task", 101, cx);

    let response = request(&server, get_last_task_output_request(), cx).await;
    let result = first_text_as_json(&response);
    assert_eq!(result["success"], json!(true), "{result}");
    assert_eq!(result["label"], json!("cargo test"));
    assert_eq!(result["command"], json!("cargo test -p demo"));
    assert_eq!(result["exitCode"], json!(101));
    assert_eq!(result["truncated"], json!(false));
    let output = result["output"].as_str().expect("the output is text");
    assert!(
        output.contains("error[E0425]: cannot find value `x`"),
        "{output}"
    );

    // 同じタスクが通れば、前の失敗は古いので返さない。
    finish_task(&workspace, "demo-task", 0, cx);
    let response = request(&server, get_last_task_output_request(), cx).await;
    assert_eq!(
        first_text_as_json(&response)["success"],
        json!(false),
        "{response}"
    );
}

#[gpui::test]
async fn a_failed_task_reads_only_the_tail_of_a_long_terminal(cx: &mut TestAppContext) {
    let (_fs, workspace, server) = test_workspace_with_fs(cx).await;
    let project = workspace.read_with(cx, |workspace, _| workspace.project().clone());

    let terminal = task_terminal("long-task", cx);
    project.update(cx, |project, _| {
        project.add_terminal_handle_for_test(&terminal)
    });
    // 400 行 × 約 30 バイト = 約 12 KB。上限 (4 KB) の 3 倍ほど。
    let mut output = String::new();
    for index in 0..400 {
        output.push_str(&format!(
            "output line number {index:04}
"
        ));
    }
    output.push_str(
        "

",
    );
    terminal.update(cx, |terminal, cx| {
        terminal.write_output(output.as_bytes(), cx);
        terminal.finish_task_for_test(1, cx);
    });

    let tail = terminal.read_with(cx, |terminal, _| {
        terminal.last_lines_within_bytes(tools::MAX_TASK_OUTPUT_BYTES)
    });
    assert!(
        tail.len() > tools::MAX_TASK_OUTPUT_BYTES && tail.len() < 6 * 1024,
        "reads just past the limit, not the whole scrollback: {}",
        tail.len()
    );
    assert!(
        !tail.contains("output line number 0000"),
        "the start is not read"
    );

    finish_task(&workspace, "long-task", 1, cx);
    let response = request(&server, get_last_task_output_request(), cx).await;
    let result = first_text_as_json(&response);
    assert_eq!(result["truncated"], json!(true), "{result}");
    let output = result["output"].as_str().expect("the output is text");
    assert!(
        output.len() <= tools::MAX_TASK_OUTPUT_BYTES,
        "{}",
        output.len()
    );
    assert!(
        output.contains("output line number 0399"),
        "the last line is kept: {output}"
    );
    assert!(
        !output.contains("output line number 0000"),
        "the first line is dropped"
    );
}

#[gpui::test]
async fn a_failed_task_whose_terminal_is_gone_still_reports_its_exit_code(cx: &mut TestAppContext) {
    let (_fs, workspace, server) = test_workspace_with_fs(cx).await;
    finish_task(&workspace, "closed-task", 2, cx);

    let response = request(&server, get_last_task_output_request(), cx).await;
    let result = first_text_as_json(&response);
    assert_eq!(result["success"], json!(true), "{result}");
    assert_eq!(result["exitCode"], json!(2));
    assert!(result.get("output").is_none(), "{result}");
}

#[gpui::test]
async fn task_output_is_not_shared_when_the_setting_is_off(cx: &mut TestAppContext) {
    let (_fs, workspace, server) = test_workspace_with_fs(cx).await;
    finish_task(&workspace, "demo-task", 1, cx);
    set_ide_server_settings(cx, |settings| settings.share_task_output = Some(false));

    let response = request(&server, get_last_task_output_request(), cx).await;
    assert_eq!(response["result"]["isError"], json!(true), "{response}");
    assert!(
        server.read_with(cx, |server, _| server.last_task_failure().is_none()),
        "turning the setting off forgets what was recorded"
    );

    finish_task(&workspace, "demo-task", 1, cx);
    assert!(
        server.read_with(cx, |server, _| server.last_task_failure().is_none()),
        "nothing is recorded while the setting is off"
    );
}

#[test]
fn pages_continue_where_the_previous_one_stopped() {
    assert_eq!(
        tools::page_of(vec![1, 2, 3, 4, 5], 0, 2),
        (vec![1, 2], Some(2))
    );
    assert_eq!(
        tools::page_of(vec![1, 2, 3, 4, 5], 2, 2),
        (vec![3, 4], Some(4))
    );
    assert_eq!(tools::page_of(vec![1, 2, 3, 4, 5], 4, 2), (vec![5], None));
    assert_eq!(tools::page_of(vec![1, 2, 3, 4], 2, 2), (vec![3, 4], None));
    assert_eq!(tools::page_of(vec![1, 2], 9, 2), (Vec::<i32>::new(), None));
}

fn get_diagnostics_request(arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "getDiagnostics", "arguments": arguments }
    })
}

fn diagnostic_uris(files: &Value) -> Vec<String> {
    files
        .as_array()
        .expect("the diagnostics are an array")
        .iter()
        .map(|file| file["uri"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[gpui::test]
async fn workspace_diagnostics_beyond_one_page_continue_with_a_cursor(cx: &mut TestAppContext) {
    let (fs, workspace, server) = test_workspace_with_fs(cx).await;
    let project = workspace.read_with(cx, |workspace, _| workspace.project().clone());
    let file_count = 501;
    let files = (0..file_count)
        .map(|index| (format!("file{index:03}.rs"), json!("x\n")))
        .collect::<serde_json::Map<_, _>>();
    fs.insert_tree(path!("/test/many"), Value::Object(files))
        .await;
    cx.run_until_parked();
    for index in 0..file_count {
        set_diagnostic(
            &project,
            &format!("{}/file{index:03}.rs", path!("/test/many")),
            "broken",
            cx,
        );
    }
    cx.run_until_parked();

    // 引数なし (CLI の呼び方) は今までどおり最初のテキストが配列で、500 件で止まる。
    let response = request(&server, get_diagnostics_request(json!({})), cx).await;
    let first_page = diagnostic_uris(&first_text_as_json(&response));
    assert_eq!(first_page.len(), 500);
    let continuation: Value = serde_json::from_str(
        response["result"]["content"][1]["text"]
            .as_str()
            .expect("a truncated result carries a continuation"),
    )
    .expect("the continuation is JSON");
    let cursor = continuation["nextCursor"]
        .as_str()
        .expect("the continuation has a cursor")
        .to_string();

    let response = request(
        &server,
        get_diagnostics_request(json!({ "cursor": cursor })),
        cx,
    )
    .await;
    let second_page = diagnostic_uris(&first_text_as_json(&response));
    assert_eq!(second_page.len(), 1, "{second_page:?}");
    assert!(
        response["result"]["content"].get(1).is_none(),
        "the last page has no continuation: {response}"
    );
    let mut all = first_page;
    all.extend(second_page);
    all.sort();
    all.dedup();
    assert_eq!(all.len(), file_count, "every file shows up exactly once");

    let response = request(
        &server,
        get_diagnostics_request(json!({ "cursor": "not-a-cursor" })),
        cx,
    )
    .await;
    assert_eq!(response["result"]["isError"], json!(true), "{response}");
}

#[gpui::test]
async fn workspace_diagnostics_list_a_file_once_for_several_language_servers(
    cx: &mut TestAppContext,
) {
    let (_fs, workspace, server) = test_workspace_with_fs(cx).await;
    let project = workspace.read_with(cx, |workspace, _| workspace.project().clone());
    for server_id in [0, 1] {
        project
            .update(cx, |project, cx| {
                project.lsp_store().update(cx, |lsp_store, cx| {
                    lsp_store.update_diagnostic_entries(
                        LanguageServerId(server_id),
                        PathBuf::from(path!("/test/main.rs")),
                        None,
                        None,
                        vec![DiagnosticEntry::new(
                            Unclipped(PointUtf16::new(0, 0))..Unclipped(PointUtf16::new(0, 2)),
                            Diagnostic {
                                severity: DiagnosticSeverity::ERROR,
                                message: format!("from server {server_id}").into(),
                                source_kind: DiagnosticSourceKind::Pushed,
                                ..Diagnostic::default()
                            },
                        )],
                        cx,
                    )
                })
            })
            .expect("the diagnostics apply");
    }
    cx.run_until_parked();

    let response = request(&server, get_diagnostics_request(json!({})), cx).await;
    let files = first_text_as_json(&response);
    assert_eq!(diagnostic_uris(&files).len(), 1, "{files}");
    assert_eq!(
        files[0]["diagnostics"]
            .as_array()
            .map(|diagnostics| diagnostics.len()),
        Some(2),
        "both servers' diagnostics are in the one entry: {files}"
    );
}

/// ターミナルの出力には秘密が混じりうるので、`getLastTaskOutput` はホストの全ツールの口だけ。
/// サンドボックスの CLI・制限セッション・アシストの口 (`/mcp/zed`) には一覧にも出さず、呼ばれても断る。
#[gpui::test]
async fn task_output_never_reaches_a_sandboxed_or_assist_scope(cx: &mut TestAppContext) {
    let paths = Arc::new(session_paths());
    for scope in [
        tools::ToolScope::Sandboxed(paths.clone()),
        tools::ToolScope::SandboxedReadOnly(paths.clone()),
        tools::ToolScope::Assist(Some(paths.clone())),
        tools::ToolScope::Assist(None),
    ] {
        assert!(!scope.allows_tool("getLastTaskOutput"));
        let listed = tool_names(&json!({
            "result": tools::list_tools_result(&tools::ConnectionScope::without_assist(scope)),
        }));
        assert!(
            !listed.iter().any(|name| name == "getLastTaskOutput"),
            "{listed:?}"
        );
    }
    assert!(tools::ToolScope::Full.allows_tool("getLastTaskOutput"));

    let fixture = start_sandbox_fixture(cx).await;
    let mut client = connect(&fixture, cx).await;
    let response = client.call_tool("getLastTaskOutput", json!({})).await;
    assert_eq!(
        response["error"]["code"],
        json!(protocol::INVALID_PARAMS),
        "{response}"
    );
    assert!(response.get("result").is_none(), "{response}");
}
