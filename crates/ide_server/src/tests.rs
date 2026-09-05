use std::path::{Path, PathBuf};
use std::sync::Arc;

use editor::Editor;
use futures::channel::mpsc::unbounded;
use futures::channel::oneshot;
use futures::{AsyncReadExt as _, AsyncWriteExt as _, StreamExt as _};
use gpui::{AppContext as _, Entity, TestAppContext};
use project::{FakeFs, Project};
use serde_json::{Value, json};
use smol::net::{TcpListener, TcpStream};
use util::path;
use workspace::{MultiWorkspace, Workspace};

use crate::protocol::LockFileContents;
use crate::{IdeServer, McpEndpoint, tools, transport};

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
        let (incoming_tx, mut incoming_rx) = unbounded::<String>();
        let (outgoing_tx, outgoing_rx) = unbounded::<String>();
        let (kind_tx, _kind_rx) = oneshot::channel();
        let pump = smol::spawn(async move {
            while let Some(text) = incoming_rx.next().await {
                if let Some(response) = responder(text)
                    && outgoing_tx.unbounded_send(response).is_err()
                {
                    break;
                }
            }
        });
        let result =
            transport::serve_connection(stream, auth_token, incoming_tx, outgoing_rx, kind_tx)
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
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/test"),
        json!({ "main.rs": "fn main() {\n    println!(\"hi\");\n}\n" }),
    )
    .await;
    let project = Project::test(fs, [path!("/test").as_ref()], cx).await;
    let project_id = project.entity_id();
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
    let server = cx.new(|cx| IdeServer::new(workspace.downgrade(), project_id, cx));
    (workspace, server)
}

async fn request(server: &Entity<IdeServer>, message: Value, cx: &mut TestAppContext) -> Value {
    let mut async_cx = cx.to_async();
    let raw = IdeServer::handle_message(
        server,
        &serde_json::to_string(&message).expect("the request serializes"),
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
