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
//! - 有効なあいだ、**ローカル**ワークスペースごとに `~/.claude/ide/<port>.lock` を書く。
//!   CLI 2.1.261 はこのファイルからポート (= ファイル名) と `workspaceFolders` /
//!   `authToken` / `transport` を読む
//! - リモート (SSH / WSL / Dev Container / collab) のワークスペースではロックファイルを
//!   **書かない**。`workspaceFolders` が相手側の絶対パスになってしまい、手元で走る CLI は
//!   自分の cwd と照合できないし、リモートのターミナルには `CLAUDE_CODE_SSE_PORT` も
//!   注げないので、置いても誤検出の種にしかならない。サーバー自体は動かしたままなので、
//!   `.mcp.json` 断片を貼れば HTTP からは今までどおり使える
//! - サンドボックス (WSL) の中で動く CLI には、TCP もロックファイルも使わない別の口を
//!   出す ([`start_sandbox_session`]、ADR-F49)

mod diff_view;
mod path_mapping;
mod protocol;
mod sandbox_client;
mod status_item;
mod tools;
mod transport;

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::Context as _;
use collections::HashMap;
use editor::{Editor, EditorEvent};
use futures::channel::{mpsc, oneshot};
use futures::stream::FuturesUnordered;
use futures::{AsyncRead, AsyncWrite, FutureExt as _, StreamExt as _};
use gpui::{
    App, AppContext as _, AsyncApp, BackgroundExecutor, BorrowAppContext as _, Entity, EntityId,
    Global, Subscription, Task, TaskExt as _, WeakEntity,
};
use project::Project;
use settings::{RegisterSetting, Settings, SettingsStore};
use smol::lock::{Semaphore, SemaphoreGuardArc};
use smol::net::{TcpListener, TcpStream};
use util::ResultExt as _;
use workspace::Workspace;

pub use diff_view::{IdeServerDiff, IdeServerDiffOutcome};
pub use path_mapping::{
    MAX_SANDBOX_FILE_BYTES, RealRoots, SandboxFile, has_link_below_root, real_roots,
    resolve_inside_roots,
};
pub use protocol::{LockFileContents, lock_file_directory, lock_file_path};
pub use sandbox_client::{
    SandboxIdeSessionHandle, SandboxSessionInfo, sandbox_lock_file_contents, start_sandbox_session,
};
pub use status_item::IdeServerStatusItem;
pub(crate) use tools::{SelectionSnapshot, ToolHost};
pub use util::path_mapping::{IdentityPathMapper, PathMapper, WslPathMapper};

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

struct IdeServerRegistry {
    servers: HashMap<EntityId, Entity<IdeServer>>,
    /// プロジェクトからワークスペースを引くための逆引き。ターミナルの環境変数と ACP の
    /// `mcp_servers` はどちらもプロジェクトしか持っていないところから呼ばれる。
    workspaces_by_project: HashMap<EntityId, EntityId>,
    /// ロックファイルを置くディレクトリ。テストが本物の `~/.claude/ide` を汚さずに
    /// 済むよう、グローバルに持たせて差し替えられるようにしてある。
    lock_directory: std::path::PathBuf,
    /// 動いているサンドボックスのセッション。右クリックの「選択範囲を送る」と、接続中の
    /// 表示で使う。
    sandbox_sessions: Vec<WeakEntity<sandbox_client::SandboxIdeSession>>,
}

impl Default for IdeServerRegistry {
    fn default() -> Self {
        Self {
            servers: HashMap::default(),
            workspaces_by_project: HashMap::default(),
            lock_directory: protocol::lock_file_directory(),
            sandbox_sessions: Vec::new(),
        }
    }
}

impl Global for IdeServerRegistry {}

fn lock_directory(cx: &App) -> std::path::PathBuf {
    cx.try_global::<IdeServerRegistry>()
        .map(|registry| registry.lock_directory.clone())
        .unwrap_or_else(protocol::lock_file_directory)
}

#[cfg(test)]
fn set_lock_directory(directory: std::path::PathBuf, cx: &mut App) {
    cx.update_global::<IdeServerRegistry, ()>(|registry, _| {
        registry.lock_directory = directory;
    });
}

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
    server_for_project_id(project.entity_id(), cx)
}

fn server_for_project_id(project_id: EntityId, cx: &App) -> Option<Entity<IdeServer>> {
    let registry = cx.try_global::<IdeServerRegistry>()?;
    let workspace_id = registry.workspaces_by_project.get(&project_id)?;
    registry.servers.get(workspace_id).cloned()
}

/// このプロジェクトのホストの IDE サーバーに WebSocket で繋がっている CLI の数。
fn host_connection_count(project_id: EntityId, cx: &App) -> usize {
    server_for_project_id(project_id, cx)
        .map_or(0, |server| server.read(cx).connected_client_count())
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
    /// リモートのワークスペースではロックファイルを書かないので `None`。
    lock_path: Option<std::path::PathBuf>,
    connections: Vec<Arc<transport::Outbox>>,
    _accept_task: Task<()>,
}

impl Drop for RunningServer {
    /// ロックファイルの後始末を [`RunningServer`] 自身に持たせてあるのは、これを
    /// 落とす経路 (`stop`、`IdeServer` の drop、アプリ終了) が増えても取り残しが
    /// 出ないようにするため。
    fn drop(&mut self) {
        let Some(lock_path) = self.lock_path.as_ref() else {
            return;
        };
        if let Err(error) = std::fs::remove_file(lock_path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            log::error!(
                "ide_server: could not remove {}: {error}",
                lock_path.display()
            );
        }
    }
}

pub struct IdeServer {
    workspace: WeakEntity<Workspace>,
    project_id: EntityId,
    running: Option<RunningServer>,
    /// `start` は bind が終わるまで `running` を埋められない。そのあいだに設定の変更が
    /// もう一度届くと 2 本目が立ち上がり、あとから入れ替えられたほうのロックファイルが
    /// 誰にも消されずに残る。この旗が「起動処理が飛んでいる」ことを表す。
    starting: bool,
    latest_selection: Option<SelectionSnapshot>,
    selection_subscription: Option<Subscription>,
    open_diffs: tools::OpenDiffs,
    _settings_subscription: Subscription,
    _quit_subscription: Subscription,
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
        // ウィンドウを閉じずに終了したときは Workspace が解放されないままプロセスが
        // 消えるので、`Drop` に任せているとロックファイルが残る。
        let quit_subscription = cx.on_app_quit(|this: &mut Self, _cx| {
            this.stop();
            async {}
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
            starting: false,
            latest_selection: None,
            selection_subscription: None,
            open_diffs: tools::OpenDiffs::default(),
            _settings_subscription: settings_subscription,
            _quit_subscription: quit_subscription,
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
        self.running.as_ref()?.lock_path.as_deref()
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
        let Some(lock_path) = running.lock_path.clone() else {
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
        let Some(json) = contents.to_json().log_err() else {
            return;
        };
        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            protocol::write_lock_file(&lock_path, json.as_bytes(), &executor)
                .await
                .log_err();
        })
        .detach();
    }

    /// ロックファイルを置いてよいワークスペースか。リモートでは相手側の絶対パスしか
    /// 書けず、手元の CLI からは照合できないので置かない。
    fn writes_lock_file(&self, cx: &App) -> bool {
        self.workspace
            .upgrade()
            .is_some_and(|workspace| workspace.read(cx).project().read(cx).is_local())
    }

    fn apply_settings(&mut self, cx: &mut gpui::Context<Self>) {
        let enabled = IdeServerSettings::get_global(cx).enabled;
        match (enabled, self.running.is_some() || self.starting) {
            (true, false) => self.start(cx),
            (false, true) => {
                self.stop();
                // サンドボックスのセッションは設定と無関係に動くので、単に「未接続」にはしない。
                self.publish_connection_state(cx);
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
        let writes_lock_file = self.writes_lock_file(cx);
        let directory = lock_directory(cx);
        let bind = cx.background_spawn(async move {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .context("binding the IDE server to 127.0.0.1")?;
            let port = listener.local_addr()?.port();
            anyhow::Ok((listener, port))
        });

        self.starting = true;
        cx.spawn(async move |this, cx| {
            let bound = bind.await;
            let (listener, port) = match bound {
                Ok(bound) => bound,
                Err(error) => {
                    this.update(cx, |this, _| this.starting = false).ok();
                    return Err(error);
                }
            };

            // ロックファイルはサーバーを登録したあとに書く。先に書くと、登録前に
            // 止められたときに消し手が居なくなる。
            let lock_path = this.update(cx, |this, cx| {
                this.starting = false;
                // 起動待ちのあいだに無効化されていたら、そのまま畳む。
                if !IdeServerSettings::get_global(cx).enabled {
                    return None;
                }
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
                let lock_path =
                    writes_lock_file.then(|| protocol::lock_file_path(&directory, port));
                this.running = Some(RunningServer {
                    port,
                    auth_token: auth_token.clone(),
                    lock_path: lock_path.clone(),
                    connections: Vec::new(),
                    _accept_task: accept_task,
                });
                log::info!("ide_server: listening on 127.0.0.1:{port}");
                this.watch_active_editor(cx);
                cx.notify();
                lock_path
            })?;

            let Some(lock_path) = lock_path else {
                return Ok(());
            };
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
                    std::fs::create_dir_all(&directory)?;
                    remove_stale_lock_files(&directory, &executor).await;
                    protocol::write_lock_file(&lock_path, json.as_bytes(), &executor).await?;
                    anyhow::Ok(())
                }
            })
            .await
            .context("writing the Claude Code IDE lock file")?;

            // 書き終わるまでのあいだに止められていたら、`RunningServer` の drop は
            // まだ存在しないファイルを消しに行っている。ここで拾い直す。
            let still_ours = this
                .read_with(cx, |this, _| {
                    this.running
                        .as_ref()
                        .and_then(|running| running.lock_path.as_deref())
                        == Some(lock_path.as_path())
                })
                .unwrap_or(false);
            if !still_ours {
                std::fs::remove_file(&lock_path).log_err();
            }
            Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn stop(&mut self) {
        self.starting = false;
        // ロックファイルは `RunningServer` の drop が消す。
        let Some(running) = self.running.take() else {
            return;
        };
        self.selection_subscription = None;
        self.latest_selection = None;
        self.open_diffs = tools::OpenDiffs::default();
        log::info!(
            "ide_server: stopped listening on 127.0.0.1:{}",
            running.port
        );
    }

    fn serve(this: Entity<Self>, stream: TcpStream, auth_token: Arc<str>, cx: &mut AsyncApp) {
        let connection = run_connection(this, stream, auth_token, None, cx.clone());
        cx.spawn(async move |_| connection.await).detach();
    }

    /// 1 本にでも積めたら `true`。
    fn broadcast(&mut self, method: &str, params: serde_json::Value) -> bool {
        let Some(running) = self.running.as_mut() else {
            return false;
        };
        broadcast_notification(&mut running.connections, method, params)
    }

    /// 接続数が変わったことを、ステータスバーと (右クリックメニューが読む) グローバルに
    /// 反映する。
    fn publish_connection_state(&mut self, cx: &mut gpui::Context<Self>) {
        let connected = self.connected_client_count() > 0
            || sandbox_client::has_connected_session(self.project_id, None, cx);
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
        let sent = self.broadcast(
            "at_mentioned",
            tools::at_mention_params(&file_path.to_string_lossy(), start_line, end_line),
        );
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

impl ToolHost for IdeServer {
    fn workspace(&self) -> Option<WeakEntity<Workspace>> {
        Some(self.workspace.clone())
    }

    fn tool_scope(&self) -> tools::ToolScope {
        tools::ToolScope::Full
    }

    fn latest_selection(&self) -> Option<&SelectionSnapshot> {
        self.latest_selection.as_ref()
    }

    fn open_diffs(&mut self) -> &mut tools::OpenDiffs {
        &mut self.open_diffs
    }

    fn add_connection(&mut self, connection: Arc<transport::Outbox>, cx: &mut gpui::Context<Self>) {
        if let Some(running) = self.running.as_mut() {
            running.connections.push(connection);
        }
        self.publish_connection_state(cx);
    }

    fn remove_connection(
        &mut self,
        connection: &Arc<transport::Outbox>,
        cx: &mut gpui::Context<Self>,
    ) {
        if let Some(running) = self.running.as_mut() {
            running
                .connections
                .retain(|existing| !Arc::ptr_eq(existing, connection) && !existing.is_closed());
        }
        self.publish_connection_state(cx);
    }
}

/// 接続中の全員に通知を撃ち、閉じた接続を刈り取る。`selection_changed` は、相手が読むまで
/// 最新の 1 件だけを残す。1 本にでも積めたら `true` (溜まりすぎて捨てた分は数えない)。
pub(crate) fn broadcast_notification(
    connections: &mut Vec<Arc<transport::Outbox>>,
    method: &str,
    params: serde_json::Value,
) -> bool {
    let message = protocol::notification(method, params);
    let mut queued = false;
    connections.retain(|connection| {
        let delivery = if method == "selection_changed" {
            connection.send_selection(message.clone())
        } else {
            connection.send_notification(message.clone())
        };
        queued |= delivery == transport::Delivery::Queued;
        delivery != transport::Delivery::Closed
    });
    queued
}

/// 1 本の接続 (ホストの TCP でも、サンドボックスから転送されてきたストリームでも) を
/// 最後まで処理する。
pub(crate) async fn run_connection<S, H>(
    this: Entity<H>,
    stream: S,
    auth_token: Arc<str>,
    open_transport: Option<OpenTransport>,
    mut cx: AsyncApp,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    H: ToolHost,
{
    // 受け取った要求を溜めておくのは 1 件だけにする。同時実行の枠が空くまで、残りは相手の
    // 送信を (トランスポートの読み込みを止めて) 待たせる。
    let (incoming_tx, mut incoming_rx) = mpsc::channel::<String>(0);
    let (outbox, outgoing) = transport::outbox();
    let (kind_tx, kind_rx) = oneshot::channel();
    let (session_permits, undelivered_limit, max_message_bytes) = this.read_with(&cx, |this, _| {
        (
            this.request_permits(),
            this.max_undelivered_responses(),
            this.max_message_bytes(),
        )
    });
    let handshake_deadline = cx.background_executor().timer(transport::HANDSHAKE_TIMEOUT);
    let io_task = cx.background_spawn(async move {
        // トランスポートが閉じるまで、開いている接続として数える。
        let _open_transport = open_transport;
        if let Err(error) = transport::serve_connection(
            stream,
            auth_token,
            max_message_bytes,
            incoming_tx,
            outgoing,
            kind_tx,
            handshake_deadline,
        )
        .await
        {
            log::debug!("ide_server: connection ended: {error:#}");
        }
    });
    let connection_permits = undelivered_limit.map(|limit| Arc::new(Semaphore::new(limit)));
    let is_websocket = kind_rx.await.ok() == Some(transport::ConnectionKind::WebSocket);
    if is_websocket {
        this.update(&mut cx, |this, cx| this.add_connection(outbox.clone(), cx));
    }

    // 1 件ずつ順に処理すると `openDiff` がユーザーの決定を待つあいだ後続の要求 (CLI が
    // 差分タブを畳む `close_tab` を含む) が止まってしまうので、並行に処理する。
    // JSON-RPC の応答は `id` で対応が付くため順序が入れ替わってよい。
    let connection = tools::ConnectionId::next();
    let mut pending = FuturesUnordered::new();
    loop {
        futures::select_biased! {
            (response, permit) = pending.select_next_some() => {
                if let Some(response) = response {
                    outbox.send_response(response, permit);
                }
            }
            text = incoming_rx.next() => match text {
                Some(text) => {
                    // 枠は要求が届いてから取る。読む前に取ると、何も送ってこない接続が枠を
                    // 抱えたまま、セッションの CLI の接続を待たせる。接続ごとの枠は応答を
                    // 相手に渡すまで、セッションの枠は処理が終わるまで持つ。
                    let connection_permit = match &connection_permits {
                        Some(permits) => {
                            Some(wait_for_permit(permits, &mut pending, &outbox).await)
                        }
                        None => None,
                    };
                    let session_permit = match &session_permits {
                        Some(permits) => {
                            Some(wait_for_permit(permits, &mut pending, &outbox).await)
                        }
                        None => None,
                    };
                    pending.push({
                        let this = this.clone();
                        let mut cx = cx.clone();
                        async move {
                            let response = handle_message(&this, &text, connection, &mut cx).await;
                            drop(session_permit);
                            (response, connection_permit)
                        }
                    });
                }
                None => break,
            },
        }
    }
    if is_websocket {
        this.update(&mut cx, |this, cx| this.remove_connection(&outbox, cx));
    }
    // 接続が切れても、ユーザーの決定を待っている `openDiff` は最後まで待つ (タブは開いた
    // ままなので)。HTTP ではここで送る応答がそのまま本文になる。
    while let Some((response, permit)) = pending.next().await {
        if let Some(response) = response {
            outbox.send_response(response, permit);
        }
    }
    outbox.finish();
    // HTTP の応答はトランスポートがまだ書いている途中かもしれないので、落とさずに待つ。
    io_task.await;
}

/// 相手との接続 (トランスポート) が開いているあいだ数に入る。io タスクが終わると外れる。
pub(crate) struct OpenTransport(Arc<AtomicUsize>);

impl OpenTransport {
    pub(crate) fn new(count: &Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count.clone())
    }
}

impl Drop for OpenTransport {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 枠が空くのを待つ。待つあいだも、処理を終えた要求の応答は届け続ける (接続ごとの枠は、
/// 応答を相手に渡したときに空く)。
async fn wait_for_permit<F>(
    permits: &Arc<Semaphore>,
    pending: &mut FuturesUnordered<F>,
    outbox: &transport::Outbox,
) -> SemaphoreGuardArc
where
    F: Future<Output = (Option<String>, Option<SemaphoreGuardArc>)>,
{
    let acquire = permits.acquire_arc().fuse();
    futures::pin_mut!(acquire);
    loop {
        futures::select_biased! {
            permit = acquire => return permit,
            (response, permit) = pending.select_next_some() => {
                if let Some(response) = response {
                    outbox.send_response(response, permit);
                }
            }
        }
    }
}

/// 1 件の JSON-RPC メッセージを処理する。応答が要らない (通知) なら `None`。
pub(crate) async fn handle_message<H: ToolHost>(
    this: &Entity<H>,
    text: &str,
    connection: tools::ConnectionId,
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
        handle_notification(&method, &params);
        return None;
    };

    Some(match method.as_str() {
        "initialize" => protocol::success_response(id, tools::initialize_result(&params)),
        "ping" => protocol::success_response(id, serde_json::json!({})),
        "tools/list" => {
            let scope = this.read_with(cx, |this, _| this.tool_scope());
            protocol::success_response(id, tools::list_tools_result(&scope))
        }
        "tools/call" => match tools::parse_call_params(params) {
            Ok(params) => {
                let name = params.name.clone();
                match tools::call_tool(this, params, connection, cx).await {
                    Some(result) => protocol::success_response(id, result),
                    // MCP の仕様どおり、出していないツールは JSON-RPC のエラーで答える。
                    None => protocol::error_response(
                        id,
                        protocol::INVALID_PARAMS,
                        format!("the tool {name} is not available in this session"),
                    ),
                }
            }
            Err(error) => {
                protocol::error_response(id, protocol::INVALID_PARAMS, format!("{error:#}"))
            }
        },
        "resources/list" => protocol::success_response(id, serde_json::json!({ "resources": [] })),
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

/// 自分が書いたのに残っている (プロセスが死んでいる) ロックを掃除する。
///
/// 他の IDE のロックには触らない。生存確認はポートに繋いでみるだけ — CLI 側の
/// 掃除処理 (`XQr`) と同じ判定で、追加の依存も要らない。
async fn remove_stale_lock_files(directory: &Path, executor: &BackgroundExecutor) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let current_pid = std::process::id();
    for entry in entries.flatten() {
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
        let Ok(text) = std::fs::read_to_string(&path) else {
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
        std::fs::remove_file(&path).log_err();
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
