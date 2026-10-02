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
//! - サンドボックスの中で動く外部の ACP エージェントには、同じ口を `getDiagnostics` だけに
//!   絞って出す ([`start_restricted_sandbox_session`]、ADR-F49 決定 24)
//! - どの経路のエージェントにも、`POST /mcp/zed` で**アシストのツール** (ハイライト・注釈・
//!   ツアー・定義・参照・シンボル検索、ADR-F56) を出す。`ide_server.enabled` とは独立で、
//!   `ide_server.assist_tools` (既定 true) で切れる。ホストのサーバーは、アシストの
//!   クライアント ([`host_assist_client`]) が居るあいだだけでも動く (ロックファイルは
//!   `enabled` のときだけ書く)

mod assist_tools;
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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::Context as _;
use collections::HashMap;
use editor::{Editor, EditorEvent};
use futures::channel::{mpsc, oneshot};
use futures::stream::FuturesUnordered;
use futures::{AsyncRead, AsyncWrite, FutureExt as _, StreamExt as _};
use gpui::{
    App, AppContext as _, AsyncApp, BackgroundExecutor, BorrowAppContext as _, Entity, EntityId,
    Global, SharedString, Subscription, Task, TaskExt as _, WeakEntity,
};
use ide_query::AssistSource;
use project::Project;
use settings::{RegisterSetting, Settings, SettingsStore};
use smol::lock::{Semaphore, SemaphoreGuardArc};
use smol::net::{TcpListener, TcpStream};
use util::ResultExt as _;
use workspace::Workspace;

pub use assist_tools::{
    ASSIST_BEARER_ENV_VAR, ASSIST_TOOL_NAMES, cli_assist_arguments, cli_assist_config_json,
};
pub use diff_view::{IdeServerDiff, IdeServerDiffOutcome};
pub use path_mapping::{
    MAX_SANDBOX_FILE_BYTES, RealRoots, SandboxFile, has_link_below_root, real_roots,
};
pub use protocol::{LockFileContents, lock_file_directory, lock_file_path};
pub use sandbox_client::{
    CliMention, SandboxIdeSessionHandle, SandboxSessionInfo, mention_selection_in_cli,
    sandbox_lock_file_contents, start_restricted_sandbox_session, start_sandbox_session,
    start_sandbox_session_for_tab,
};
pub use status_item::IdeServerStatusItem;
pub use tools::RESTRICTED_TOOL_NAMES;
pub(crate) use tools::{SelectionSnapshot, ToolHost};
pub use transport::{ASSIST_MCP_PATH, IDE_MCP_PATH};
pub use util::path_mapping::{IdentityPathMapper, PathMapper, WslPathMapper};

/// Claude Code CLI が「どのインスタンスに繋げばよいか」を決めるための環境変数。
///
/// CLI 2.1.261 はロックファイルのポートがこれと一致すればワークツリー照合と親プロセス
/// 照合を飛ばし、これが設定されていること自体を IDE 自動接続の合図にする。
pub const CLAUDE_CODE_PORT_ENV_VAR: &str = "CLAUDE_CODE_SSE_PORT";

/// ロックファイルに書く IDE 名。CLI 側の表示にそのまま使われる。
const IDE_NAME: &str = PRODUCT_NAME;

/// The name the agents connected to this server see the editor under.
pub(crate) const PRODUCT_NAME: &str = branding::CURRENT.product_name;

/// ACP セッションと `.mcp.json` に載せるときのサーバー名。
///
/// エージェントから見えるツール名は `mcp__<name>__<tool>` になるので、CLI が内蔵する
/// `ide` とも、ユーザーが設定した MCP サーバーとも被らない名前にしてある。
pub const MCP_SERVER_NAME: &str = "zed";

/// サンドボックスの中の ACP エージェントに制限セッション (ADR-F49 決定 24) を渡すときの
/// サーバー名。ホストの [`MCP_SERVER_NAME`] とは違って `ide` にしてある。
///
/// Claude Code CLI は `mcp__ide__` で始まるツールのうち `getDiagnostics` と `executeCode`
/// だけをモデルに見せ、残りの `mcp__ide__*` は CLI が内部から呼ぶものとして隠す
/// (2.1.271 で確認)。名前を `ide` にしておけば、制限セッションの唯一のツールは
/// `mcp__ide__getDiagnostics` として、CLI 本来の IDE 連携とまったく同じ名前でモデルに届く。
pub const RESTRICTED_MCP_SERVER_NAME: &str = "ide";

/// 古いロックファイルの生存確認に使う接続待ち時間。loopback なので短くてよい。
const STALE_LOCK_PROBE_TIMEOUT: Duration = Duration::from_millis(300);

#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct IdeServerSettings {
    pub enabled: bool,
    /// ADR-F56: エージェントにアシストのツール (MCP サーバー `zed`) を渡すか。
    pub assist_tools: bool,
    /// 何も選んでいないときも、開いているファイルを CLI に伝えるか (CLI の「⧉ In <ファイル>」)。
    pub share_active_file: bool,
    /// 最後に失敗したタスクの出力を `getLastTaskOutput` で渡すか。
    pub share_task_output: bool,
}

impl Settings for IdeServerSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let ide_server = content.ide_server.clone().unwrap_or_default();
        Self {
            enabled: ide_server.enabled.unwrap_or(false),
            assist_tools: ide_server.assist_tools.unwrap_or(true),
            share_active_file: ide_server.share_active_file.unwrap_or(true),
            share_task_output: ide_server.share_task_output.unwrap_or(true),
        }
    }
}

/// 接続してから、最新の選択範囲を送り直すまでの待ち。CLI は IDE の MCP クライアントが繋がった
/// あとで `selection_changed` の受け手を登録し、そのとき表示を空に戻す。握手の直後に送ると
/// 受け手の登録前に届いて捨てられる (VSCode の拡張も接続の 500ms 後に送り直している)。
const RESEND_SELECTION_AFTER_CONNECT: Duration = Duration::from_millis(500);

/// 新しく繋がった CLI に、`RESEND_SELECTION_AFTER_CONNECT` のあとで最新の選択範囲を送る。
/// `resend` は今の状態を計算し直し、その接続に送るべき通知を返す。
fn resend_selection_after_connect<H: 'static>(
    connection: Arc<transport::Outbox>,
    resend: fn(&mut H, &mut gpui::Context<H>) -> Option<serde_json::Value>,
    cx: &mut gpui::Context<H>,
) {
    cx.spawn(async move |this, cx| {
        cx.background_executor()
            .timer(RESEND_SELECTION_AFTER_CONNECT)
            .await;
        if connection.is_closed() {
            return;
        }
        let Ok(Some(params)) = this.update(cx, |this, cx| resend(this, cx)) else {
            return;
        };
        connection.send_selection(protocol::notification("selection_changed", params));
    })
    .detach();
}

/// ADR-F56: エージェントにアシストのツールを渡す設定か。IDE サーバーの設定が登録されていない
/// プロセス (一部のテスト) では渡さない。
pub fn assist_tools_enabled(cx: &App) -> bool {
    cx.has_global::<IdeServerRegistry>() && IdeServerSettings::get_global(cx).assist_tools
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
            let project = workspace.project().downgrade();
            let workspace_handle = cx.weak_entity();
            let server = cx.new(|cx| IdeServer::new(workspace_handle, project, cx));
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

/// このプロジェクトを開いているワークスペース。
///
/// サンドボックスのセッション ([`start_sandbox_session`] /
/// [`start_restricted_sandbox_session`]) を始める側は、プロジェクトしか持っていないところ
/// (ACP の接続) から呼ばれる。[`IdeServer`] は `ide_server.enabled` と無関係にワークスペース
/// ごとに作られるので、その逆引きをそのまま使える。
pub fn workspace_for_project(project: &Entity<Project>, cx: &App) -> Option<WeakEntity<Workspace>> {
    Some(server_for_project(project, cx)?.read(cx).workspace.clone())
}

/// ADR-F56: ホストのサーバーを使うアシストのクライアント (ホストで起動した CLI のタブ・
/// サンドボックスの外の ACP の接続)。持っているあいだはサーバーが動き (`enabled` でなくても)、
/// 落とすと登録が外れ、このクライアントが描いたハイライト・注釈・ツアーが消える。
#[must_use = "dropping the client ends its registration and clears what it drew"]
pub struct HostAssistClient {
    registration: Entity<HostAssistRegistration>,
}

struct HostAssistRegistration {
    server: WeakEntity<IdeServer>,
    client_id: String,
}

/// ADR-F56: このプロジェクトのホストのサーバーに、アシストのクライアントを 1 つ登録する。
/// `label` は表示の枠に出る名前 (「Claude Code」やエージェント名)。サーバーが必要なら
/// ここで起動する。プロジェクトを開いているワークスペースが無ければ `None`。
pub fn host_assist_client(
    project: &Entity<Project>,
    label: impl Into<SharedString>,
    cx: &mut App,
) -> Option<HostAssistClient> {
    let server = server_for_project(project, cx)?;
    let client_id = uuid::Uuid::new_v4().simple().to_string();
    let label = label.into();
    server.update(cx, |server, cx| {
        server.register_assist_client(client_id.clone(), label, cx)
    });
    let weak_server = server.downgrade();
    let registration = cx.new(|cx| {
        cx.on_release(|registration: &mut HostAssistRegistration, cx| {
            let server = registration.server.clone();
            let client_id = registration.client_id.clone();
            // 持ち主 (ACP の接続・CLI のタブ) が落ちるのは、どの更新の最中でもありうる。
            cx.defer(move |cx| {
                if let Some(server) = server.upgrade() {
                    server.update(cx, |server, cx| {
                        server.unregister_assist_client(&client_id, cx)
                    });
                }
            });
        })
        .detach();
        HostAssistRegistration {
            server: weak_server,
            client_id,
        }
    });
    Some(HostAssistClient { registration })
}

impl HostAssistClient {
    fn server_and_id<'a>(&self, cx: &'a App) -> Option<(&'a IdeServer, String)> {
        let registration = self.registration.read(cx);
        let server = registration.server.upgrade()?;
        let client_id = registration.client_id.clone();
        Some((server.read(cx), client_id))
    }

    /// アシストのツールだけの口 (`/mcp/zed`、このクライアントのトークン)。ホストで起動した
    /// CLI に渡す。
    /// `ide_server.assist_tools` が無効か、サーバーが動いていなければ `None`。
    pub fn assist_endpoint(&self, cx: &App) -> Option<McpEndpoint> {
        let (server, client_id) = self.server_and_id(cx)?;
        if !IdeServerSettings::get_global(cx).assist_tools {
            return None;
        }
        server.endpoint_for(transport::ASSIST_MCP_PATH, Some(&client_id))
    }

    /// サンドボックスの外の ACP のエージェントに渡す口。`ide_server.enabled` なら全ツールの
    /// `/mcp` (アシストのツールも含む)、そうでなければアシストのツールだけの `/mcp/zed`。
    /// どちらも無効なら `None`。
    pub fn agent_endpoint(&self, cx: &App) -> Option<McpEndpoint> {
        let (server, client_id) = self.server_and_id(cx)?;
        let settings = IdeServerSettings::get_global(cx);
        if settings.enabled {
            server.endpoint_for(transport::IDE_MCP_PATH, Some(&client_id))
        } else if settings.assist_tools {
            server.endpoint_for(transport::ASSIST_MCP_PATH, Some(&client_id))
        } else {
            None
        }
    }

    /// このクライアントが描く表示の出どころ。
    pub fn source(&self, cx: &App) -> Option<AssistSource> {
        let (server, client_id) = self.server_and_id(cx)?;
        Some(server.assist_source(Some(&client_id)))
    }
}

/// Zed が名前を決めていないクライアント (`.mcp.json` を貼った外部のクライアントなど、サーバー
/// 自身のトークンで繋いできたもの)。表示の枠の見出しは、クライアントの申告 (MCP の
/// `clientInfo.name`・ツールの引数) からは決して取らない: 「Zed」などを名乗らせると、Zed 自身の
/// 表示に見せかけられるため。
const UNKNOWN_ASSIST_CLIENT_LABEL: &str = i18n::msg!("External Agent");

/// 口ごとの受け入れの判断 (ADR-F56)。要求の頭を読んだところで、トランスポートが
/// バックグラウンドで使う。
#[derive(Clone)]
pub(crate) struct ConnectionGate {
    pub(crate) ide: EndpointGate,
    pub(crate) assist: EndpointGate,
}

#[derive(Clone)]
pub(crate) struct EndpointGate {
    /// `None` なら常に開いている。
    pub(crate) open: Option<Arc<AtomicBool>>,
    pub(crate) limits: transport::MessageLimits,
    /// 同時に開いておけるトランスポートの数と、その上限。`None` なら上限なし。
    pub(crate) quota: Option<(Arc<AtomicUsize>, usize)>,
}

impl EndpointGate {
    pub(crate) fn unlimited(limits: transport::MessageLimits) -> Self {
        Self {
            open: None,
            limits,
            quota: None,
        }
    }
}

impl ConnectionGate {
    /// どちらの口も開いていて、同時接続の枠が無い。
    pub(crate) fn unlimited(ide_limits: transport::MessageLimits) -> Self {
        Self {
            ide: EndpointGate::unlimited(ide_limits),
            assist: EndpointGate::unlimited(assist_tools::ASSIST_MESSAGE_LIMITS),
        }
    }

    pub(crate) fn admit(
        &self,
        route: &transport::Route,
    ) -> Result<transport::Admitted, transport::Refusal> {
        let gate = match route.endpoint {
            transport::Endpoint::Ide => &self.ide,
            transport::Endpoint::Assist => &self.assist,
        };
        if gate
            .open
            .as_ref()
            .is_some_and(|open| !open.load(Ordering::SeqCst))
        {
            return Err(transport::Refusal::NotFound);
        }
        let hold = match &gate.quota {
            Some((count, limit)) => {
                Some(OpenTransport::try_new(count, *limit).ok_or(transport::Refusal::Busy)?)
            }
            None => None,
        };
        Ok(transport::Admitted {
            limits: gate.limits,
            hold,
        })
    }
}

struct RunningServer {
    port: u16,
    auth_token: Arc<str>,
    /// ロックファイルを書いていないあいだ (`enabled` でない・リモートのワークスペース) は `None`。
    lock_path: Option<std::path::PathBuf>,
    connections: Vec<Arc<transport::Outbox>>,
    /// ADR-F56: IDE の口 (WebSocket と `/mcp`) を開いているか。`ide_server.enabled` のときだけ。
    /// アシストのクライアントのためだけに動いているあいだは閉じている。
    ide_open: Arc<AtomicBool>,
    /// ADR-F56: アシストの口 (`/mcp/zed`) を開いているか (`ide_server.assist_tools`)。
    assist_open: Arc<AtomicBool>,
    _accept_task: Task<()>,
}

impl RunningServer {
    fn serves_ide(&self) -> bool {
        self.ide_open.load(Ordering::SeqCst)
    }
}

impl Drop for RunningServer {
    /// ロックファイルの後始末を [`RunningServer`] 自身に持たせてあるのは、これを
    /// 落とす経路 (`stop`、`IdeServer` の drop、アプリ終了) が増えても取り残しが
    /// 出ないようにするため。
    fn drop(&mut self) {
        if let Some(lock_path) = self.lock_path.take() {
            remove_lock_file_at(&lock_path);
        }
    }
}

fn remove_lock_file_at(lock_path: &Path) {
    if let Err(error) = std::fs::remove_file(lock_path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        log::error!(
            "ide_server: could not remove {}: {error}",
            lock_path.display()
        );
    }
}

struct AssistClient {
    /// 表示の枠に出す名前。
    label: SharedString,
    token: Arc<str>,
}

pub struct IdeServer {
    workspace: WeakEntity<Workspace>,
    /// 表示を消すのに使う (ワークスペースは更新や解放の途中で読めないことがある)。
    project: WeakEntity<Project>,
    project_id: EntityId,
    running: Option<RunningServer>,
    latest_selection: Option<SelectionSnapshot>,
    /// 最後に CLI へ送った `selection_changed`。同じものを撃ち直さない。`latest_selection`
    /// (`getLatestSelection` が返す、最後に見たエディターの選択) とは別に持つ: エディターが
    /// 無くなったときに送る消す通知は、選択を見たことにはしない。
    last_sent: Option<serde_json::Value>,
    /// 最後に反映した `ide_server.share_active_file`。変わったときだけ送り直す。
    share_active_file: bool,
    selection_subscription: Option<Subscription>,
    open_diffs: tools::OpenDiffs,
    /// このワークスペースで最後に失敗したタスク (`getLastTaskOutput`)。失敗した時点で
    /// ターミナルから読んでおく: あとで同じタスクを流し直すと、ターミナルが使い回される。
    last_task_failure: Option<tools::TaskFailure>,
    /// ADR-F56: このサーバーを使うアシストのクライアント。1 つでもあれば、`enabled` でなくても
    /// サーバーを動かしておく。
    assist_clients: HashMap<String, AssistClient>,
    /// クライアントごとのトークン (トランスポートが照合する)。サーバーではなく登録の寿命なので、
    /// サーバーを止めて起動し直しても、登録が続くかぎり同じトークンが使える。
    client_tokens: transport::ClientTokens,
    _settings_subscription: Subscription,
    _quit_subscription: Subscription,
    _workspace_subscription: Option<Subscription>,
}

impl IdeServer {
    fn new(
        workspace: WeakEntity<Workspace>,
        project: WeakEntity<Project>,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let project_id = project.entity_id();
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
            project,
            project_id,
            running: None,
            latest_selection: None,
            last_sent: None,
            share_active_file: true,
            selection_subscription: None,
            open_diffs: tools::OpenDiffs::default(),
            last_task_failure: None,
            assist_clients: HashMap::default(),
            client_tokens: transport::ClientTokens::default(),
            _settings_subscription: settings_subscription,
            _quit_subscription: quit_subscription,
            _workspace_subscription: workspace_subscription,
        }
    }

    /// IDE の口 (ロックファイル・`.mcp.json`) で待ち受けているポート。アシストの
    /// クライアントのためだけに動いているあいだ (`enabled` でない) は `None`。
    pub fn port(&self) -> Option<u16> {
        self.running
            .as_ref()
            .filter(|running| running.serves_ide())
            .map(|running| running.port)
    }

    /// WebSocket で繋がっている Claude Code CLI の数。
    pub fn connected_client_count(&self) -> usize {
        self.running
            .as_ref()
            .map_or(0, |running| running.connections.len())
    }

    /// ADR-F56: 登録されているアシストのクライアント (ホストで起動した CLI・ACP の接続) の数。
    pub fn assist_client_count(&self) -> usize {
        self.assist_clients.len()
    }

    pub fn mcp_endpoint(&self) -> Option<McpEndpoint> {
        self.running
            .as_ref()
            .filter(|running| running.serves_ide())?;
        self.endpoint_for(transport::IDE_MCP_PATH, None)
    }

    /// 動いているサーバーの `path` を指すエンドポイント。クライアントの ID があればその
    /// クライアントのトークン、なければサーバー自身のトークンで。
    fn endpoint_for(&self, path: &str, client_id: Option<&str>) -> Option<McpEndpoint> {
        let running = self.running.as_ref()?;
        let token = match client_id {
            Some(client_id) => self.assist_clients.get(client_id)?.token.clone(),
            None => running.auth_token.clone(),
        };
        Some(McpEndpoint {
            url: format!("http://127.0.0.1:{}{path}", running.port),
            authorization: format!("Bearer {token}"),
        })
    }

    pub fn lock_file_path(&self) -> Option<&Path> {
        self.running.as_ref()?.lock_path.as_deref()
    }

    /// ADR-F56: 要求を送ってきたクライアントの、表示の出どころ。登録の無いクライアント
    /// (`.mcp.json` を貼った外部のクライアントなど) は、このサーバーの 1 つの出どころにまとめる。
    fn assist_source(&self, client_id: Option<&str>) -> AssistSource {
        match client_id.and_then(|client_id| {
            self.assist_clients
                .get(client_id)
                .map(|client| (client_id, client))
        }) {
            Some((client_id, client)) => AssistSource {
                id: format!("zed-ide-client-{client_id}").into(),
                label: client.label.clone(),
            },
            None => AssistSource {
                id: format!("zed-ide-server-{}", self.project_id.as_u64()).into(),
                label: i18n::tr(UNKNOWN_ASSIST_CLIENT_LABEL).into(),
            },
        }
    }

    fn register_assist_client(
        &mut self,
        client_id: String,
        label: SharedString,
        cx: &mut gpui::Context<Self>,
    ) {
        let token: Arc<str> = uuid::Uuid::new_v4().to_string().into();
        self.client_tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(token.to_string(), client_id.clone());
        self.assist_clients
            .insert(client_id, AssistClient { label, token });
        self.apply_settings(cx);
        cx.notify();
    }

    fn unregister_assist_client(&mut self, client_id: &str, cx: &mut gpui::Context<Self>) {
        let source = self.assist_source(Some(client_id));
        let Some(client) = self.assist_clients.remove(client_id) else {
            return;
        };
        self.client_tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&*client.token);
        self.clear_assist_display(source, cx);
        self.apply_settings(cx);
        cx.notify();
    }

    /// `source` が描いたハイライト・注釈・ツアーを消す。
    fn clear_assist_display(&self, source: AssistSource, cx: &mut App) {
        let Some(display) = ide_query::code_display(cx) else {
            return;
        };
        let Some(project) = self.project.upgrade() else {
            return;
        };
        // 表示の側はエディターとワークスペースを更新するので、今の更新の外で消す。
        cx.defer(move |cx| display.clear(&project, &source, cx));
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
            workspace::Event::TaskFinished {
                task_id,
                label,
                command_label,
                success,
                exit_code,
            } => {
                if *success {
                    // 同じタスクが通ったら、前の失敗はもう古い。
                    if self
                        .last_task_failure
                        .as_ref()
                        .is_some_and(|failure| &failure.task_id == task_id)
                    {
                        self.last_task_failure = None;
                    }
                } else if IdeServerSettings::get_global(cx).share_task_output {
                    let terminal_content = self.finished_task_terminal_content(task_id, cx);
                    self.last_task_failure = Some(tools::TaskFailure::new(
                        task_id.clone(),
                        label.clone(),
                        command_label.clone(),
                        *exit_code,
                        terminal_content.as_deref(),
                    ));
                }
            }
            _ => {}
        }
    }

    /// 終わったタスクのターミナルの内容。同じタスクのターミナルが複数あれば、いちばん新しいもの。
    /// `hide: always` などでターミナルが先に閉じられていたら `None`。
    fn finished_task_terminal_content(&self, task_id: &task::TaskId, cx: &App) -> Option<String> {
        let project = self.project.upgrade()?;
        let project = project.read(cx);
        project
            .local_terminal_handles()
            .iter()
            .rev()
            .filter_map(WeakEntity::upgrade)
            .find(|terminal| {
                terminal.read(cx).task().is_some_and(|task| {
                    &task.spawned_task.id == task_id && task.status != terminal::TaskStatus::Running
                })
            })
            .map(|terminal| {
                // 余裕を足すのは、末尾の切り詰めが `truncated` を正しく立てられるように
                // 上限を少し超える量を読むため。スクロールバック全体は文字列にしない。
                terminal
                    .read(cx)
                    .last_lines_within_bytes(tools::MAX_TASK_OUTPUT_BYTES + 1024)
            })
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

    /// 設定とアシストのクライアントの有無から、サーバーを動かすか・どの口を開けるかを決める。
    ///
    /// - `enabled`: IDE の口 (WebSocket・`/mcp`) を開け、ロックファイルを書き、選択範囲を追う
    /// - `assist_tools` かつクライアントあり: 少なくともアシストの口 (`/mcp/zed`) のために動かす
    /// - 動いているあいだにクライアントが居れば、`assist_tools` を切っても待ち受けは続ける
    ///   (口だけ閉じる)
    ///
    /// 口の開け閉めでは待ち受けもトークンも変えない。起動済みの CLI のタブや ACP の接続が
    /// 持っているポートとトークンが、設定を切り替えて戻したあとも使えるように。
    fn apply_settings(&mut self, cx: &mut gpui::Context<Self>) {
        let settings = IdeServerSettings::get_global(cx).clone();
        let share_active_file_changed =
            std::mem::replace(&mut self.share_active_file, settings.share_active_file)
                != settings.share_active_file;
        if !settings.share_task_output {
            self.last_task_failure = None;
        }
        let wanted = settings.enabled
            || (!self.assist_clients.is_empty()
                && (settings.assist_tools || self.running.is_some()));
        if !wanted {
            if self.running.is_some() {
                self.stop();
                // 登録の無いクライアントの表示は、サーバーと一緒に消す。
                self.clear_assist_display(self.assist_source(None), cx);
                // サンドボックスのセッションは設定と無関係に動くので、単に「未接続」にはしない。
                self.publish_connection_state(cx);
            }
            return;
        }
        if self.running.is_none()
            && let Err(error) = self.start(cx)
        {
            log::error!("ide_server: could not start the IDE server: {error:#}");
            return;
        }
        let Some(running) = self.running.as_ref() else {
            return;
        };
        running
            .assist_open
            .store(settings.assist_tools, Ordering::SeqCst);
        let was_serving_ide = running.ide_open.swap(settings.enabled, Ordering::SeqCst);
        match (was_serving_ide, settings.enabled) {
            (false, true) => {
                // ロックファイルの中身と選択範囲はワークスペースを読む。ここはワークスペースを
                // 更新中の経路 (CLI の起動アクションからのアシストのクライアントの登録) からも
                // 呼ばれるので、読むのは 1 tick あとにする。
                let this = cx.weak_entity();
                cx.defer(move |cx| {
                    if let Some(this) = this.upgrade() {
                        this.update(cx, |this, cx| {
                            this.write_lock_file(cx);
                            this.watch_active_editor(cx);
                            cx.notify();
                        });
                    }
                });
            }
            (true, false) => {
                self.close_ide_endpoint();
                self.publish_connection_state(cx);
            }
            (true, true) if share_active_file_changed => {
                // `share_active_file` を切ったら、開いているだけのファイルの表示をすぐ消す
                // (入れたらすぐ出す)。同じ通知は `send_selection_params` が撃ち直さない。
                let this = cx.weak_entity();
                cx.defer(move |cx| {
                    if let Some(this) = this.upgrade() {
                        this.update(cx, |this, cx| this.watch_active_editor(cx));
                    }
                });
            }
            _ => {}
        }
    }

    /// 127.0.0.1 の空きポートで待ち受ける。口はまだどちらも閉じていて、開けるのは
    /// [`Self::apply_settings`]。bind は loopback なのでその場で済ませる (非同期にすると、
    /// そのあいだに届いた設定の変更が 2 本目を立ち上げうる)。
    fn start(&mut self, cx: &mut gpui::Context<Self>) -> anyhow::Result<()> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .context("binding the IDE server to 127.0.0.1")?;
        let port = listener.local_addr()?.port();
        let listener = TcpListener::try_from(listener)
            .context("handing the IDE server's socket to the async runtime")?;
        let auth_token: Arc<str> = uuid::Uuid::new_v4().to_string().into();
        let accept_task = cx.spawn({
            let credentials = transport::Credentials::Host {
                server_token: auth_token.clone(),
                client_tokens: self.client_tokens.clone(),
            };
            async move |this, cx| {
                loop {
                    let Ok((stream, peer)) = listener.accept().await else {
                        break;
                    };
                    if !peer.ip().is_loopback() {
                        log::warn!("ide_server: refused a non-loopback connection from {peer}");
                        continue;
                    }
                    let Some(this) = this.upgrade() else {
                        break;
                    };
                    IdeServer::serve(this, stream, credentials.clone(), cx);
                }
            }
        });
        self.running = Some(RunningServer {
            port,
            auth_token,
            lock_path: None,
            connections: Vec::new(),
            ide_open: Arc::new(AtomicBool::new(false)),
            assist_open: Arc::new(AtomicBool::new(false)),
            _accept_task: accept_task,
        });
        log::info!("ide_server: listening on 127.0.0.1:{port}");
        cx.notify();
        Ok(())
    }

    /// ロックファイルを書く (ローカルのワークスペースだけ)。サーバーを登録したあとに書く:
    /// 先に書くと、登録前に止められたときに消し手が居なくなる。
    fn write_lock_file(&mut self, cx: &mut gpui::Context<Self>) {
        if !self.writes_lock_file(cx) {
            return;
        }
        let directory = lock_directory(cx);
        let workspace_folders = self
            .workspace
            .upgrade()
            .map(|workspace| tools::workspace_folder_paths(&workspace, cx))
            .unwrap_or_default();
        let Some(running) = self.running.as_mut() else {
            return;
        };
        if running.lock_path.is_some() || !running.serves_ide() {
            return;
        }
        let lock_path = protocol::lock_file_path(&directory, running.port);
        running.lock_path = Some(lock_path.clone());
        let contents = protocol::LockFileContents {
            pid: std::process::id(),
            workspace_folders,
            ide_name: IDE_NAME.to_string(),
            transport: "ws".to_string(),
            running_in_windows: cfg!(target_os = "windows"),
            auth_token: running.auth_token.to_string(),
        };
        let Some(json) = contents.to_json().log_err() else {
            return;
        };
        cx.spawn(async move |this, cx| {
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

            // 書き終わるまでのあいだに止められて (口を閉じられて) いたら、消す側は
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
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// `enabled` が切れたが、アシストのクライアントのために待ち受けは続けるとき: ロックファイルを
    /// 消し、繋がっている CLI を切り、選択範囲を追うのをやめる。
    fn close_ide_endpoint(&mut self) {
        if let Some(running) = self.running.as_mut() {
            if let Some(lock_path) = running.lock_path.take() {
                remove_lock_file_at(&lock_path);
            }
            for connection in running.connections.drain(..) {
                connection.finish();
            }
        }
        self.selection_subscription = None;
        self.latest_selection = None;
        self.last_sent = None;
        self.open_diffs = tools::OpenDiffs::default();
    }

    fn stop(&mut self) {
        // ロックファイルは `RunningServer` の drop が消す。
        let Some(running) = self.running.take() else {
            return;
        };
        self.selection_subscription = None;
        self.latest_selection = None;
        self.last_sent = None;
        self.open_diffs = tools::OpenDiffs::default();
        log::info!(
            "ide_server: stopped listening on 127.0.0.1:{}",
            running.port
        );
    }

    fn serve(
        this: Entity<Self>,
        stream: TcpStream,
        credentials: transport::Credentials,
        cx: &mut AsyncApp,
    ) {
        let connection = run_connection(this, stream, credentials, None, cx.clone());
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
        if !self
            .running
            .as_ref()
            .is_some_and(|running| running.serves_ide())
        {
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
            // 全部閉じた・ターミナルやプレビューに移ったときも、CLI の「In <ファイル>」を消す。
            self.selection_subscription = None;
            self.send_selection_params(tools::SelectionSnapshot::cleared_notification_params());
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
        let params =
            snapshot.to_notification_params(IdeServerSettings::get_global(cx).share_active_file);
        self.latest_selection = Some(snapshot);
        self.send_selection_params(params);
    }

    fn send_selection_params(&mut self, params: serde_json::Value) {
        if self.last_sent.as_ref() == Some(&params) {
            return;
        }
        self.last_sent = Some(params.clone());
        self.broadcast("selection_changed", params);
    }

    /// 今の状態を計算し直し (変わっていれば全員に送り)、新しく繋がった CLI に送る通知を返す。
    fn current_selection_params(
        &mut self,
        cx: &mut gpui::Context<Self>,
    ) -> Option<serde_json::Value> {
        self.watch_active_editor(cx);
        self.last_sent.clone()
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

    fn last_task_failure(&self) -> Option<&tools::TaskFailure> {
        self.last_task_failure.as_ref()
    }

    fn add_connection(&mut self, connection: Arc<transport::Outbox>, cx: &mut gpui::Context<Self>) {
        if let Some(running) = self.running.as_mut() {
            running.connections.push(connection.clone());
        }
        self.publish_connection_state(cx);
        resend_selection_after_connect(connection, Self::current_selection_params, cx);
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

    /// 口の開け閉めは設定で決まる ([`IdeServer::apply_settings`])。同時接続の枠は置かない
    /// (相手はサンドボックスの外の、利用者自身の権限のプロセス)。
    fn connection_gate(&self) -> ConnectionGate {
        let closed = || Some(Arc::new(AtomicBool::new(false)));
        let (ide_open, assist_open) = match self.running.as_ref() {
            Some(running) => (
                Some(running.ide_open.clone()),
                Some(running.assist_open.clone()),
            ),
            None => (closed(), closed()),
        };
        ConnectionGate {
            ide: EndpointGate {
                open: ide_open,
                ..EndpointGate::unlimited(self.message_limits())
            },
            assist: EndpointGate {
                open: assist_open,
                ..EndpointGate::unlimited(assist_tools::ASSIST_MESSAGE_LIMITS)
            },
        }
    }

    /// ADR-F56: WebSocket (CLI の `ide`) にはこれまでどおり全ツール、`/mcp` (外部のエージェントの
    /// `zed`) には全ツールとアシストのツール、`/mcp/zed` にはアシストのツールだけを出す。
    fn connection_scope(
        &self,
        route: &transport::Route,
        cx: &App,
    ) -> Result<tools::ConnectionScope, String> {
        let settings = IdeServerSettings::get_global(cx);
        match (route.endpoint, route.kind) {
            (transport::Endpoint::Ide, _) if !settings.enabled => {
                Err(assist_tools::IDE_SERVER_DISABLED_MESSAGE.to_string())
            }
            (transport::Endpoint::Ide, transport::ConnectionKind::WebSocket) => Ok(
                tools::ConnectionScope::without_assist(tools::ToolScope::Full),
            ),
            (transport::Endpoint::Ide, transport::ConnectionKind::Http) => {
                Ok(tools::ConnectionScope {
                    tools: tools::ToolScope::Full,
                    assist: settings
                        .assist_tools
                        .then(|| self.assist_source(route.client.as_deref())),
                })
            }
            (transport::Endpoint::Assist, _) if settings.assist_tools => {
                Ok(tools::ConnectionScope {
                    tools: tools::ToolScope::Assist(None),
                    assist: Some(self.assist_source(route.client.as_deref())),
                })
            }
            (transport::Endpoint::Assist, _) => {
                Err(assist_tools::ASSIST_UNAVAILABLE_MESSAGE.to_string())
            }
        }
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
    credentials: impl Into<transport::Credentials>,
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
    let (route_tx, route_rx) = oneshot::channel();
    let (session_permits, undelivered_limit, gate) = this.read_with(&cx, |this, _| {
        (
            this.request_permits(),
            this.max_undelivered_responses(),
            this.connection_gate(),
        )
    });
    let handshake_deadline = cx.background_executor().timer(transport::HANDSHAKE_TIMEOUT);
    let credentials = credentials.into();
    let io_task = cx.background_spawn(async move {
        // トランスポートが閉じるまで、開いている接続として数える。
        let _open_transport = open_transport;
        if let Err(error) = transport::serve_connection(
            stream,
            credentials,
            move |route| gate.admit(route),
            incoming_tx,
            outgoing,
            route_tx,
            handshake_deadline,
        )
        .await
        {
            log::debug!("ide_server: connection ended: {error:#}");
        }
    });
    let connection_permits = undelivered_limit.map(|limit| Arc::new(Semaphore::new(limit)));
    let route = route_rx.await.ok();
    // 口が決まらないまま切れた接続 (認証の失敗・枠の外) からは、要求は 1 件も来ない。
    let scope = match route.as_ref() {
        Some(route) => this.read_with(&cx, |this, cx| this.connection_scope(route, cx)),
        None => Err(String::new()),
    };
    let is_websocket = scope.is_ok()
        && route
            .as_ref()
            .is_some_and(|route| route.kind == transport::ConnectionKind::WebSocket);
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
                        let scope = scope.clone();
                        async move {
                            let response =
                                handle_message_in(&this, &text, connection, &scope, &mut cx).await;
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

    /// `count` が `limit` に達していなければ数に入れる。
    pub(crate) fn try_new(count: &Arc<AtomicUsize>, limit: usize) -> Option<Self> {
        let previous = count.fetch_add(1, Ordering::SeqCst);
        if previous >= limit {
            count.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Self(count.clone()))
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

/// 1 件の JSON-RPC メッセージを、そのホストの既定の口 (アシストのツールなし) として処理する。
#[cfg(test)]
pub(crate) async fn handle_message<H: ToolHost>(
    this: &Entity<H>,
    text: &str,
    connection: tools::ConnectionId,
    cx: &mut AsyncApp,
) -> Option<String> {
    let scope = this.read_with(cx, |this, _| {
        tools::ConnectionScope::without_assist(this.tool_scope())
    });
    handle_message_in(this, text, connection, &Ok(scope), cx).await
}

/// 1 件の JSON-RPC メッセージを処理する。応答が要らない (通知) なら `None`。`scope` が `Err` の
/// 接続 (口が設定で閉じているなど) には、要求ごとにその理由で断る。
pub(crate) async fn handle_message_in<H: ToolHost>(
    this: &Entity<H>,
    text: &str,
    connection: tools::ConnectionId,
    scope: &Result<tools::ConnectionScope, String>,
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
    let scope = match scope {
        Ok(scope) => scope,
        Err(reason) => {
            return Some(protocol::error_response(
                id,
                protocol::REFUSED,
                reason.clone(),
            ));
        }
    };

    Some(match method.as_str() {
        "initialize" => protocol::success_response(id, tools::initialize_result_in(&params, scope)),
        "ping" => protocol::success_response(id, serde_json::json!({})),
        "tools/list" => protocol::success_response(id, tools::list_tools_result(scope)),
        "tools/call" => match tools::parse_call_params(params) {
            Ok(params) => {
                let name = params.name.clone();
                match tools::call_tool(this, params, connection, scope, cx).await {
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
