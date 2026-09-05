//! Zed 自身を **IDE MCP サーバー**として loopback に出す (ADR-F26)。
//!
//! Zed は MCP のクライアント (`context_server`) は持っていたが、自分をサーバーとして
//! 外に出す口が無かった。そのため統合ターミナルで起動した Claude Code CLI は、
//! 選択範囲・開いているファイル・診断・差分レビューをまったく使えない。VSCode /
//! JetBrains の Claude Code 拡張はこれを `ide` という名前の MCP サーバーで渡している。
//! ここではその口を Zed に足す。
//!
//! - 127.0.0.1 の空きポートに bind し、**外向き通信はゼロ**。したがって ADR-F03 の
//!   egress 許可リストとは無関係 (CLI 自身の外向き通信は ADR-F20 のとおり
//!   「人間のターミナル」扱い)
//! - 起動ごとのランダムトークンで認証する。WebSocket は CLI が送る
//!   `X-Claude-Code-Ide-Authorization`、HTTP は `Authorization: Bearer`
//! - 設定 `ide_server.enabled` は**既定 false** (ADR-F09「AI 機能は既定オフ」)
//! - 有効なあいだ、ウィンドウごとに `~/.claude/ide/<port>.lock` を書く。CLI 2.1.261 は
//!   このファイルからポート (= ファイル名) と `workspaceFolders` / `authToken` /
//!   `transport` を読む

mod diff_view;
mod protocol;
mod status_item;
mod tools;
mod transport;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use collections::HashMap;
use editor::{Editor, EditorEvent};
use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use futures::channel::oneshot;
use gpui::{
    App, AppContext as _, AsyncApp, BackgroundExecutor, BorrowAppContext as _, Entity, EntityId,
    Global, Subscription, Task, TaskExt as _, WeakEntity,
};
use project::Project;
use settings::{RegisterSetting, Settings, SettingsStore};
use smol::net::{TcpListener, TcpStream};
use util::ResultExt as _;
use workspace::Workspace;

pub use diff_view::{IdeServerDiff, IdeServerDiffOutcome};
pub use protocol::{LockFileContents, lock_file_directory, lock_file_path};
pub use status_item::IdeServerStatusItem;
pub(crate) use tools::SelectionSnapshot;

/// Claude Code CLI が「どのインスタンスに繋げばよいか」を決めるための環境変数。
///
/// CLI 2.1.261 はロックファイルのポートがこれと一致すればワークツリー照合と親プロセス
/// 照合を飛ばし、これが設定されていること自体を IDE 自動接続の合図にする。
pub const CLAUDE_CODE_PORT_ENV_VAR: &str = "CLAUDE_CODE_SSE_PORT";

/// ロックファイルに書く IDE 名。CLI 側の表示にそのまま使われる。
const IDE_NAME: &str = "Zed";

/// ACP セッションと `.mcp.json` に載せるときのサーバー名。
///
/// エージェントから見えるツール名は `mcp__<name>__<tool>` になるので、CLI が内蔵する
/// `ide` とも、ユーザーが設定した MCP サーバーとも被らない名前にしてある。
pub const MCP_SERVER_NAME: &str = "zed";

/// 古いロックファイルの生存確認に使う接続待ち時間。loopback なので短くてよい。
const STALE_LOCK_PROBE_TIMEOUT: Duration = Duration::from_millis(300);

#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct IdeServerSettings {
    pub enabled: bool,
}

impl Settings for IdeServerSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let ide_server = content.ide_server.clone().unwrap_or_default();
        Self {
            enabled: ide_server.enabled.unwrap_or(false),
        }
    }
}

pub fn init(cx: &mut App) {
    cx.set_global(IdeServerRegistry::default());
    project::terminals::register_terminal_environment_provider(terminal_environment, cx);
    cx.observe_new(
        |workspace: &mut Workspace, _window, cx: &mut gpui::Context<Workspace>| {
            if !cx.has_global::<IdeServerRegistry>() {
                return;
            }
            let workspace_id = cx.entity_id();
            let project_id = workspace.project().entity_id();
            let workspace_handle = cx.weak_entity();
            let server = cx.new(|cx| IdeServer::new(workspace_handle, project_id, cx));
            cx.update_global::<IdeServerRegistry, ()>(|registry, _| {
                registry.servers.insert(workspace_id, server);
                registry
                    .workspaces_by_project
                    .insert(project_id, workspace_id);
            });
            cx.on_release(move |_, cx| {
                zed_actions::ide_server::set_claude_code_connected(project_id, false, cx);
                if cx.has_global::<IdeServerRegistry>() {
                    cx.update_global::<IdeServerRegistry, ()>(|registry, _| {
                        registry.servers.remove(&workspace_id);
                        registry.workspaces_by_project.remove(&project_id);
                    });
                }
            })
            .detach();
            status_item::register_workspace_actions(workspace);
        },
    )
    .detach();
}

#[derive(Default)]
struct IdeServerRegistry {
    servers: HashMap<EntityId, Entity<IdeServer>>,
    /// プロジェクトからワークスペースを引くための逆引き。ターミナルの環境変数と ACP の
    /// `mcp_servers` はどちらもプロジェクトしか持っていないところから呼ばれる。
    workspaces_by_project: HashMap<EntityId, EntityId>,
}

impl Global for IdeServerRegistry {}

fn server_for(workspace: &Entity<Workspace>, cx: &App) -> Option<Entity<IdeServer>> {
    server_for_workspace_id(workspace.entity_id(), cx)
}

fn server_for_workspace_id(workspace_id: EntityId, cx: &App) -> Option<Entity<IdeServer>> {
    cx.try_global::<IdeServerRegistry>()?
        .servers
        .get(&workspace_id)
        .cloned()
}

fn server_for_project(project: &Entity<Project>, cx: &App) -> Option<Entity<IdeServer>> {
    let registry = cx.try_global::<IdeServerRegistry>()?;
    let workspace_id = registry.workspaces_by_project.get(&project.entity_id())?;
    registry.servers.get(workspace_id).cloned()
}

/// このプロジェクトのローカルターミナルに注入する環境変数。
///
/// サーバーが動いていなければ空。`init` で `project` のプロバイダーとして登録している。
pub fn terminal_environment(project: &Entity<Project>, cx: &App) -> Vec<(String, String)> {
    let Some(port) = server_for_project(project, cx).and_then(|server| server.read(cx).port())
    else {
        return Vec::new();
    };
    vec![(CLAUDE_CODE_PORT_ENV_VAR.to_string(), port.to_string())]
}

/// このワークスペースの IDE サーバーが待ち受けているポート。
pub fn active_port(workspace: &Entity<Workspace>, cx: &App) -> Option<u16> {
    server_for(workspace, cx)?.read(cx).port()
}

/// 外部のエージェントに渡す streamable HTTP の MCP エンドポイント。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpEndpoint {
    pub url: String,
    /// `Authorization` ヘッダーの値 (`Bearer <token>`)。
    pub authorization: String,
}

impl McpEndpoint {
    /// `.mcp.json` にそのまま貼れる断片。
    pub fn to_configuration_json(&self) -> anyhow::Result<String> {
        serde_json::to_string_pretty(&serde_json::json!({
            "mcpServers": {
                MCP_SERVER_NAME: {
                    "type": "http",
                    "url": self.url,
                    "headers": { "Authorization": self.authorization },
                }
            }
        }))
        .context("serializing the MCP configuration snippet")
    }
}

/// このプロジェクトの IDE サーバーに繋ぐための MCP エンドポイント。
pub fn mcp_endpoint(project: &Entity<Project>, cx: &App) -> Option<McpEndpoint> {
    server_for_project(project, cx)?.read(cx).mcp_endpoint()
}

struct RunningServer {
    port: u16,
    auth_token: Arc<str>,
    lock_path: std::path::PathBuf,
    connections: Vec<UnboundedSender<String>>,
    _accept_task: Task<()>,
}

pub struct IdeServer {
    workspace: WeakEntity<Workspace>,
    project_id: EntityId,
    running: Option<RunningServer>,
    latest_selection: Option<SelectionSnapshot>,
    selection_subscription: Option<Subscription>,
    open_diffs: HashMap<String, Entity<IdeServerDiff>>,
    _settings_subscription: Subscription,
    _workspace_subscription: Option<Subscription>,
}

impl IdeServer {
    fn new(
        workspace: WeakEntity<Workspace>,
        project_id: EntityId,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let settings_subscription = cx.observe_global::<SettingsStore>(|this, cx| {
            this.apply_settings(cx);
        });
        let workspace_subscription = workspace
            .upgrade()
            .map(|workspace| cx.subscribe(&workspace, Self::on_workspace_event));
        // `init` の `observe_new` は Workspace をリースしたまま呼ばれる。ここで直接
        // `apply_settings` に入ると、設定が既に有効なときに `start` が
        // `workspace.read` して二重リースで panic するので、1 tick 遅らせる。
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            if let Some(this) = this.upgrade() {
                this.update(cx, |this, cx| this.apply_settings(cx));
            }
        });
        Self {
            workspace,
            project_id,
            running: None,
            latest_selection: None,
            selection_subscription: None,
            open_diffs: HashMap::default(),
            _settings_subscription: settings_subscription,
            _workspace_subscription: workspace_subscription,
        }
    }

    pub fn port(&self) -> Option<u16> {
        self.running.as_ref().map(|running| running.port)
    }

    /// WebSocket で繋がっている Claude Code CLI の数。
    pub fn connected_client_count(&self) -> usize {
        self.running
            .as_ref()
            .map_or(0, |running| running.connections.len())
    }

    pub fn mcp_endpoint(&self) -> Option<McpEndpoint> {
        let running = self.running.as_ref()?;
        Some(McpEndpoint {
            url: format!("http://127.0.0.1:{}/mcp", running.port),
            authorization: format!("Bearer {}", running.auth_token),
        })
    }

    pub fn lock_file_path(&self) -> Option<&Path> {
        self.running
            .as_ref()
            .map(|running| running.lock_path.as_path())
    }

    pub(crate) fn workspace(&self) -> &WeakEntity<Workspace> {
        &self.workspace
    }

    pub(crate) fn latest_selection(&self) -> Option<&SelectionSnapshot> {
        self.latest_selection.as_ref()
    }

    pub(crate) fn register_diff(&mut self, tab_name: String, diff: Entity<IdeServerDiff>) {
        self.open_diffs.insert(tab_name, diff);
    }

    pub(crate) fn take_diff(&mut self, tab_name: &str) -> Option<Entity<IdeServerDiff>> {
        self.open_diffs.remove(tab_name)
    }

    pub(crate) fn take_all_diffs(&mut self) -> Vec<Entity<IdeServerDiff>> {
        std::mem::take(&mut self.open_diffs).into_values().collect()
    }

    fn on_workspace_event(
        &mut self,
        _workspace: Entity<Workspace>,
        event: &workspace::Event,
        cx: &mut gpui::Context<Self>,
    ) {
        match event {
            workspace::Event::ActiveItemChanged
            | workspace::Event::ItemAdded { .. }
            | workspace::Event::ItemRemoved { .. } => self.watch_active_editor(cx),
            // ワークツリーが増減したらロックファイルの `workspaceFolders` を書き直す。
            // CLI はこれで「この cwd はどの IDE のものか」を決めている。
            workspace::Event::WorktreeCreationChanged => self.refresh_lock_file(cx),
            _ => {}
        }
    }

    fn refresh_lock_file(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(running) = self.running.as_ref() else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let contents = protocol::LockFileContents {
            pid: std::process::id(),
            workspace_folders: tools::workspace_folder_paths(&workspace, cx),
            ide_name: IDE_NAME.to_string(),
            transport: "ws".to_string(),
            running_in_windows: cfg!(target_os = "windows"),
            auth_token: running.auth_token.to_string(),
        };
        let lock_path = running.lock_path.clone();
        let Some(json) = contents.to_json().log_err() else {
            return;
        };
        cx.background_spawn(async move {
            smol::fs::write(&lock_path, json).await.log_err();
        })
        .detach();
    }

    fn apply_settings(&mut self, cx: &mut gpui::Context<Self>) {
        let enabled = IdeServerSettings::get_global(cx).enabled;
        match (enabled, self.running.is_some()) {
            (true, false) => self.start(cx),
            (false, true) => {
                self.stop();
                zed_actions::ide_server::set_claude_code_connected(self.project_id, false, cx);
                cx.notify();
            }
            _ => {}
        }
    }

    fn start(&mut self, cx: &mut gpui::Context<Self>) {
        let auth_token: Arc<str> = uuid::Uuid::new_v4().to_string().into();
        let workspace_folders = self
            .workspace
            .upgrade()
            .map(|workspace| tools::workspace_folder_paths(&workspace, cx))
            .unwrap_or_default();
        let bind = cx.background_spawn(async move {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .context("binding the IDE server to 127.0.0.1")?;
            let port = listener.local_addr()?.port();
            anyhow::Ok((listener, port))
        });

        cx.spawn(async move |this, cx| {
            let (listener, port) = bind.await?;
            let directory = protocol::lock_file_directory();
            let lock_path = protocol::lock_file_path(&directory, port);
            let contents = protocol::LockFileContents {
                pid: std::process::id(),
                workspace_folders,
                ide_name: IDE_NAME.to_string(),
                transport: "ws".to_string(),
                running_in_windows: cfg!(target_os = "windows"),
                auth_token: auth_token.to_string(),
            };
            let json = contents.to_json()?;
            let executor = cx.background_executor().clone();
            cx.background_spawn({
                let lock_path = lock_path.clone();
                async move {
                    smol::fs::create_dir_all(&directory).await?;
                    remove_stale_lock_files(&directory, &executor).await;
                    smol::fs::write(&lock_path, json).await?;
                    anyhow::Ok(())
                }
            })
            .await
            .context("writing the Claude Code IDE lock file")?;

            this.update(cx, |this, cx| {
                let accept_task = cx.spawn({
                    let auth_token = auth_token.clone();
                    async move |this, cx| {
                        loop {
                            let Ok((stream, peer)) = listener.accept().await else {
                                break;
                            };
                            if !peer.ip().is_loopback() {
                                log::warn!(
                                    "ide_server: refused a non-loopback connection from {peer}"
                                );
                                continue;
                            }
                            let Some(this) = this.upgrade() else {
                                break;
                            };
                            IdeServer::serve(this, stream, auth_token.clone(), cx);
                        }
                    }
                });
                this.running = Some(RunningServer {
                    port,
                    auth_token,
                    lock_path,
                    connections: Vec::new(),
                    _accept_task: accept_task,
                });
                log::info!("ide_server: listening on 127.0.0.1:{port}");
                this.watch_active_editor(cx);
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    fn stop(&mut self) {
        let Some(running) = self.running.take() else {
            return;
        };
        self.selection_subscription = None;
        self.latest_selection = None;
        self.open_diffs.clear();
        std::fs::remove_file(&running.lock_path).log_err();
        log::info!(
            "ide_server: stopped listening on 127.0.0.1:{}",
            running.port
        );
    }

    fn serve(this: Entity<Self>, stream: TcpStream, auth_token: Arc<str>, cx: &mut AsyncApp) {
        let (incoming_tx, mut incoming_rx) = unbounded::<String>();
        let (outgoing_tx, outgoing_rx) = unbounded::<String>();
        let (kind_tx, kind_rx) = oneshot::channel();
        let io_task = cx.background_spawn(async move {
            if let Err(error) =
                transport::serve_connection(stream, auth_token, incoming_tx, outgoing_rx, kind_tx)
                    .await
            {
                log::debug!("ide_server: connection ended: {error:#}");
            }
        });

        cx.spawn(async move |cx| {
            let kind = kind_rx.await.ok();
            let is_websocket = kind == Some(transport::ConnectionKind::WebSocket);
            if is_websocket {
                this.update(cx, |this, cx| {
                    if let Some(running) = this.running.as_mut() {
                        running.connections.push(outgoing_tx.clone());
                    }
                    this.publish_connection_state(cx);
                });
            }
            // 1 件ずつ順に処理すると `openDiff` がユーザーの決定を待つあいだ後続の
            // 要求 (CLI が差分タブを畳む `close_tab` を含む) が止まってしまうので、
            // 要求ごとにタスクを起こす。JSON-RPC の応答は `id` で対応が付くため
            // 順序が入れ替わってよい。
            while let Some(text) = incoming_rx.next().await {
                cx.spawn({
                    let this = this.clone();
                    let outgoing_tx = outgoing_tx.clone();
                    async move |cx| {
                        if let Some(response) = IdeServer::handle_message(&this, &text, cx).await {
                            outgoing_tx.unbounded_send(response).ok();
                        }
                    }
                })
                .detach();
            }
            drop(outgoing_tx);
            if is_websocket {
                this.update(cx, |this, cx| {
                    if let Some(running) = this.running.as_mut() {
                        running
                            .connections
                            .retain(|connection| !connection.is_closed());
                    }
                    this.publish_connection_state(cx);
                });
            }
            drop(io_task);
        })
        .detach();
    }

    /// 1 件の JSON-RPC メッセージを処理する。応答が要らない (通知) なら `None`。
    pub(crate) async fn handle_message(
        this: &Entity<Self>,
        text: &str,
        cx: &mut AsyncApp,
    ) -> Option<String> {
        let message: protocol::IncomingMessage = match serde_json::from_str(text) {
            Ok(message) => message,
            Err(error) => {
                log::warn!("ide_server: could not parse an incoming message: {error}");
                return Some(protocol::error_response(
                    protocol::RequestId::Int(0),
                    protocol::PARSE_ERROR,
                    format!("{error}"),
                ));
            }
        };
        let method = message.method.unwrap_or_default();
        let params = message.params.unwrap_or(serde_json::Value::Null);
        let Some(id) = message.id else {
            Self::handle_notification(&method, &params);
            return None;
        };

        Some(match method.as_str() {
            "initialize" => protocol::success_response(id, tools::initialize_result(&params)),
            "ping" => protocol::success_response(id, serde_json::json!({})),
            "tools/list" => protocol::success_response(id, tools::list_tools_result()),
            "tools/call" => match tools::call_tool(this, params, cx).await {
                Ok(result) => protocol::success_response(id, result),
                Err(error) => {
                    protocol::error_response(id, protocol::INTERNAL_ERROR, format!("{error:#}"))
                }
            },
            "resources/list" => {
                protocol::success_response(id, serde_json::json!({ "resources": [] }))
            }
            "prompts/list" => protocol::success_response(id, serde_json::json!({ "prompts": [] })),
            other => protocol::error_response(
                id,
                protocol::METHOD_NOT_FOUND,
                format!("unhandled method {other}"),
            ),
        })
    }

    fn handle_notification(method: &str, params: &serde_json::Value) {
        match method {
            "notifications/initialized" | "experiment_gates" | "notifications/cancelled" => {}
            "ide_connected" => {
                let pid = params.get("pid").and_then(serde_json::Value::as_i64);
                log::info!("ide_server: a Claude Code CLI connected (pid {pid:?})");
            }
            other => log::debug!("ide_server: ignoring the notification {other}"),
        }
    }

    fn broadcast(&mut self, method: &str, params: serde_json::Value) {
        let Some(running) = self.running.as_mut() else {
            return;
        };
        let message = protocol::notification(method, params);
        running
            .connections
            .retain(|connection| connection.unbounded_send(message.clone()).is_ok());
    }

    /// 接続数が変わったことを、ステータスバーと (右クリックメニューが読む) グローバルに
    /// 反映する。
    fn publish_connection_state(&mut self, cx: &mut gpui::Context<Self>) {
        let connected = self.connected_client_count() > 0;
        zed_actions::ide_server::set_claude_code_connected(self.project_id, connected, cx);
        cx.notify();
    }

    /// 選択範囲を、接続中の CLI に `at_mentioned` として送る。
    ///
    /// CLI 側は行番号を 0 始まりで受け取り、`@ファイル#L1-L3` の形に直して会話に貼る。
    /// 送り先が 1 本も無ければ `false`。
    pub(crate) fn send_at_mention(
        &mut self,
        file_path: &Path,
        start_line: u32,
        end_line: u32,
        cx: &mut gpui::Context<Self>,
    ) -> bool {
        if self.connected_client_count() == 0 {
            return false;
        }
        self.broadcast(
            "at_mentioned",
            serde_json::json!({
                "filePath": file_path.to_string_lossy(),
                "lineStart": start_line,
                "lineEnd": end_line,
            }),
        );
        // 送信先が閉じていれば `broadcast` が刈り取っているので、その結果で判定する。
        let sent = self.connected_client_count() > 0;
        self.publish_connection_state(cx);
        sent
    }

    /// アクティブなエディターの選択範囲を追いかけ、変わるたびに CLI へ通知する。
    ///
    /// 購読は `Subscription` を差し替えることで自動的に張り替わる。
    fn watch_active_editor(&mut self, cx: &mut gpui::Context<Self>) {
        if self.running.is_none() {
            self.selection_subscription = None;
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            self.selection_subscription = None;
            return;
        };
        let active_item = workspace.read(cx).active_item(cx);
        let editor = active_item.and_then(|item| item.act_as::<Editor>(cx));
        let Some(editor) = editor else {
            self.selection_subscription = None;
            return;
        };
        self.selection_subscription = Some(cx.subscribe(
            &editor,
            |this, editor, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::SelectionsChanged { .. }) {
                    this.publish_selection(&editor, cx);
                }
            },
        ));
        self.publish_selection(&editor, cx);
    }

    fn publish_selection(&mut self, editor: &Entity<Editor>, cx: &mut gpui::Context<Self>) {
        let Some(snapshot) = tools::selection_snapshot(editor, cx) else {
            return;
        };
        if self.latest_selection.as_ref() == Some(&snapshot) {
            return;
        }
        let params = snapshot.to_notification_params();
        self.latest_selection = Some(snapshot);
        self.broadcast("selection_changed", params);
    }
}

impl Drop for IdeServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 自分が書いたのに残っている (プロセスが死んでいる) ロックを掃除する。
///
/// 他の IDE のロックには触らない。生存確認はポートに繋いでみるだけ — CLI 側の
/// 掃除処理 (`XQr`) と同じ判定で、追加の依存も要らない。
async fn remove_stale_lock_files(directory: &Path, executor: &BackgroundExecutor) {
    let Ok(mut entries) = smol::fs::read_dir(directory).await else {
        return;
    };
    let current_pid = std::process::id();
    while let Some(Ok(entry)) = entries.next().await {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("lock") {
            continue;
        }
        let Some(port) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.parse::<u16>().ok())
        else {
            continue;
        };
        let Ok(text) = smol::fs::read_to_string(&path).await else {
            continue;
        };
        let Ok(contents) = protocol::LockFileContents::from_json(&text) else {
            continue;
        };
        if contents.ide_name != IDE_NAME || contents.pid == current_pid {
            continue;
        }
        if port_is_listening(port, executor).await {
            continue;
        }
        smol::fs::remove_file(&path).await.log_err();
    }
}

/// 迷ったら「生きている」に倒す — 生きているサーバーのロックを消すほうが害が大きい。
async fn port_is_listening(port: u16, executor: &BackgroundExecutor) -> bool {
    let connect = async { TcpStream::connect(("127.0.0.1", port)).await.is_ok() };
    let timeout = async {
        executor.timer(STALE_LOCK_PROBE_TIMEOUT).await;
        true
    };
    smol::future::or(connect, timeout).await
}

#[cfg(test)]
mod tests;
