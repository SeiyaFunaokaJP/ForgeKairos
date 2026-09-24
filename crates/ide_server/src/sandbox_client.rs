//! サンドボックス (WSL・SSH 接続先) の中で動く Claude Code CLI 用の IDE セッション
//! (ADR-F49 決定 3・4、ADR-F53)。
//!
//! ホストの [`crate::IdeServer`] と同じ WebSocket / JSON-RPC を話す (トランスポートと
//! ディスパッチは共通の [`crate::run_connection`]) が、
//!
//! - ホストには TCP ポートもロックファイルも出さない。接続は、サンドボックスのヘルパーが
//!   名前空間内の `127.0.0.1:<port>` から標準入出力のブリッジで運んでくるストリーム
//!   (接続ごとに 1 本) として受け取る。ロックファイルはブリッジが書く
//!   ([`sandbox_lock_file_contents`])
//! - `ide_server.enabled` とは無関係に動く。認証トークンはセッションごと
//! - ツールは CLI が実際に呼ぶ 4 本だけで、パスは WSL ↔ Windows を変換したうえで
//!   ワークツリーの中に閉じ込める ([`util::path_mapping`])。こちらから送る通知
//!   (`selection_changed` / `at_mentioned`) のパスも CLI 側の形に直し、ワークツリーの外は
//!   見せない
//! - サンドボックスの中のプロセスはロックファイルからトークンを読めるので、認証を通った
//!   相手も信用しない。同時に処理する要求・届け終えていない応答・開いておける差分タブ・
//!   開いている接続の数と、1 メッセージの大きさに上限を置く
//!
//! 外部の ACP エージェント (Claude Code CLI 以外) 用には、同じ口を `getDiagnostics` だけに
//! 絞った**制限セッション**を出す ([`start_restricted_sandbox_session`]、ADR-F49 決定 24)。
//!
//! どちらのセッションも、同じトークン・同じ転送の `POST /mcp/zed` でアシストのツールを出す
//! (ADR-F56)。同時接続はセッションの IDE の口の枠とは別に数える。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Context as _;
use editor::{Editor, EditorEvent};
use futures::stream::FuturesUnordered;
use futures::{AsyncRead, AsyncWrite, Stream, StreamExt as _};
use gpui::{
    App, AppContext as _, AsyncApp, BorrowAppContext as _, Context, Entity, EntityId, SharedString,
    Subscription, Task, WeakEntity,
};
use ide_query::AssistSource;
use project::{File as WorktreeFile, Project, ProjectPath};
use serde_json::Value;
use settings::Settings as _;
use smol::lock::Semaphore;
use workspace::Workspace;

use util::path_mapping::{self, PathMapper, SandboxPaths};

use crate::assist_tools::{
    ASSIST_MESSAGE_LIMITS, ASSIST_UNAVAILABLE_MESSAGE, MAX_ASSIST_CONNECTIONS,
};
use crate::protocol::LockFileContents;
use crate::tools::{self, ConnectionScope, OpenDiffs, SelectionSnapshot, ToolHost, ToolScope};
use crate::transport::{Endpoint, MessageLimits, Outbox, Route};
use crate::{
    ConnectionGate, EndpointGate, IDE_NAME, IdeServerRegistry, IdeServerSettings, OpenTransport,
};

/// サンドボックスのロックファイルに書く pid。理由は [`sandbox_lock_file_contents`]。
const SANDBOX_LOCK_PID: u32 = 0;

/// サンドボックスのセッションを受けられないプロジェクトか (受けられるなら `None`)。
///
/// ADR-F49 のローカルのプロジェクトに加え、ADR-F53 では SSH リモート (接続先の
/// `remote_server`) も受ける。断るのは 2 つ:
///
/// - collab の相手として開いているプロジェクト (`is_local` でも `is_via_remote_server`
///   でもない)。ファイルを見られるのはホストだけで、実体の確認 (`verify_agent_read`) も
///   ホストへは届かない
/// - collab で共有中のプロジェクト (`remote_id` あり)。差分タブと診断は共有相手にも見える
pub(crate) fn session_project_refusal(
    is_local: bool,
    is_via_remote_server: bool,
    is_shared_over_collab: bool,
) -> Option<&'static str> {
    if !is_local && !is_via_remote_server {
        return Some("the project is opened over collab, whose host owns the files");
    }
    if is_shared_over_collab {
        return Some("the project is shared over collab, so others would see the session");
    }
    None
}

/// 1 セッションで同時に処理する要求。
const MAX_IN_FLIGHT_REQUESTS: usize = 8;
/// 1 本の接続で、相手に届け終えていない応答 (処理中を含む)。
const MAX_UNDELIVERED_RESPONSES: usize = 8;
const MAX_OPEN_DIFFS: usize = 4;
// ユーザーの決定を待つ `openDiff` は、どちらの枠も決定まで持ち続ける。開ける差分より枠が
// 多くないと、差分で枠が埋まり、それを閉じる `close_tab` さえ処理できなくなる。
const _: () =
    assert!(MAX_IN_FLIGHT_REQUESTS > MAX_OPEN_DIFFS && MAX_UNDELIVERED_RESPONSES > MAX_OPEN_DIFFS);
/// 同時に開いておけるトランスポート。差分の決定を待っているだけの接続は数えない。
pub(crate) const MAX_CONNECTIONS: usize = 4;
/// 制限セッションで同時に開いておけるトランスポート (ADR-F49 決定 24)。制限セッションが話す
/// のは streamable HTTP で、こちらは 1 要求ごとに 1 接続を使い捨てるので、WebSocket を 1 本
/// 張り続ける CLI より多めに要る。ブリッジが運べる本数 (`sandbox` 側の上限 64) の内側。
pub(crate) const MAX_RESTRICTED_CONNECTIONS: usize = 16;
const _: () = assert!(MAX_RESTRICTED_CONNECTIONS > MAX_CONNECTIONS);
/// 1 メッセージの上限。旧内容の上限 (4 MiB) ほどの提案を、JSON のエスケープで膨らんでも運べる大きさ。
const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
/// 制限セッションが**受け取る** 1 メッセージの上限 (ADR-F49 決定 24)。上の 16 MiB は
/// `openDiff` が運んでくる新しい本文のための値で、制限セッションが受け取るのは
/// `getDiagnostics` の引数 (URI 1 本) だけ。同時接続を 16 まで許すぶん、抱えうる受信の
/// 総量をここで抑える。送る側は据え置き (URI 無しの `getDiagnostics` はファイル 100 件を返す)。
pub(crate) const MAX_RESTRICTED_INCOMING_MESSAGE_BYTES: usize = 1024 * 1024;

pub struct SandboxSessionInfo {
    /// ロックファイルの `authToken`。CLI はこれを `X-Claude-Code-Ide-Authorization` で出す。
    pub auth_token: String,
}

/// 落とすとセッションが終わる。転送されてきた接続も、処理中の要求も閉じ、このセッションが
/// 開いた差分タブを畳み、描いたハイライト・注釈・ツアーを消す。
#[must_use = "dropping the handle ends the sandbox IDE session"]
pub struct SandboxIdeSessionHandle {
    session: WeakEntity<SandboxIdeSession>,
    _task: Task<()>,
}

impl SandboxIdeSessionHandle {
    /// ADR-F56: アシストのツールの表示の枠に出す名前 (ACP のエージェント名など)。
    pub fn set_assist_label(&self, label: impl Into<SharedString>, cx: &mut App) {
        let label = label.into();
        self.session
            .update(cx, |session, _| session.assist_label = label)
            .ok();
    }
}

/// CLI のタブの Claude Code に、選択範囲をどう渡すか (ADR-F56)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CliMention {
    /// IDE の口 (`at_mentioned`) で届けた。タブには何も貼らない。
    Sent,
    /// IDE に繋がっていないので、`@<path>#L<行>` をタブに貼る。`path` は CLI から見えるパス。
    /// `None` なら呼び出し側がホストのパスで組み立てる (サンドボックスの外の CLI)。
    Paste { cli_path: Option<String> },
    /// CLI から見えないファイル (サンドボックスのワークツリーの外)。貼りも送りもしない。
    Unreachable,
}

/// ADR-F56: `AddSelectionToThread` をエージェントパネルの CLI のタブに向けたとき。
///
/// `tab_key` はそのタブのサンドボックスのセッション ([`start_sandbox_session_for_tab`] の
/// キー)。セッションに CLI が繋がっていれば `at_mentioned` で送り、繋がっていなければ CLI から
/// 見えるパスを返す。サンドボックスの外のタブ (`tab_key` が `None`) は、ホストのサーバーに
/// 繋がっている CLI に送る。行は 0 始まり (`at_mentioned` の形)。
pub fn mention_selection_in_cli(
    tab_key: Option<&str>,
    project: &Entity<Project>,
    host_path: &Path,
    start_line: u32,
    end_line: u32,
    cx: &mut App,
) -> CliMention {
    let session = tab_key.and_then(|tab_key| session_for_tab(tab_key, cx));
    if let Some(session) = session {
        let sent = session.update(cx, |session, cx| {
            session.send_at_mention(host_path, start_line, end_line, cx)
        });
        if sent {
            return CliMention::Sent;
        }
        return match session.read(cx).paths.to_client(host_path) {
            Some(cli_path) => CliMention::Paste {
                cli_path: Some(cli_path),
            },
            None => CliMention::Unreachable,
        };
    }
    // サンドボックスのタブでも、IDE の中継が上がらなかった (SSH) セッションには IDE の
    // セッションが無い。接続先のパスはプロジェクトのパスそのものなので、そのまま貼れる。
    if tab_key.is_none()
        && let Some(server) = crate::server_for_project(project, cx)
    {
        let sent = server.update(cx, |server, cx| {
            server.send_at_mention(host_path, start_line, end_line, cx)
        });
        if sent {
            return CliMention::Sent;
        }
    }
    CliMention::Paste { cli_path: None }
}

fn session_for_tab(tab_key: &str, cx: &App) -> Option<Entity<SandboxIdeSession>> {
    cx.try_global::<IdeServerRegistry>()?
        .sandbox_sessions
        .iter()
        .filter_map(WeakEntity::upgrade)
        .find(|session| session.read(cx).tab_key.as_deref() == Some(tab_key))
}

/// サンドボックスのセッションが誰のためのものか。
#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionKind {
    /// サンドボックスの中の Claude Code CLI (ADR-F49 決定 3・4)。CLI が呼ぶ 4 本のツールを
    /// 出し、選択範囲の通知も送り、ユーザーからは「接続中の Claude Code」として見える。
    ClaudeCodeCli,
    /// サンドボックスの中の外部 ACP エージェント (ADR-F49 決定 24)。`getDiagnostics` だけを
    /// 出す。差分タブも通知も無いので、ユーザーの目に触れる口 (ステータスバー・右クリックの
    /// 送信先) には出さない。
    Restricted,
}

impl SessionKind {
    fn tool_scope(self, paths: Arc<SandboxPaths>) -> ToolScope {
        match self {
            SessionKind::ClaudeCodeCli => ToolScope::Sandboxed(paths),
            SessionKind::Restricted => ToolScope::SandboxedReadOnly(paths),
        }
    }

    /// IDE の口 (CLI の WebSocket・制限セッションの `/mcp`) で同時に開いておけるトランスポート。
    fn max_connections(self) -> usize {
        match self {
            SessionKind::ClaudeCodeCli => MAX_CONNECTIONS,
            SessionKind::Restricted => MAX_RESTRICTED_CONNECTIONS,
        }
    }

    /// 要求の頭を読む前に受けておけるトランスポートの総数。どの口に来たかは頭を読むまで
    /// 分からないので、ここでは両方の口の枠の合計で抑え、口ごとの枠は頭を読んでから数える
    /// (ADR-F56)。
    fn max_transports(self) -> usize {
        self.max_connections() + MAX_ASSIST_CONNECTIONS
    }

    /// アシストのツールの表示の枠に出す名前の既定。制限セッションは、呼び出し側が
    /// [`SandboxIdeSessionHandle::set_assist_label`] でエージェント名にする。
    fn default_assist_label(self) -> &'static str {
        match self {
            SessionKind::ClaudeCodeCli => "Claude Code",
            SessionKind::Restricted => "エージェント",
        }
    }

    fn message_limits(self) -> MessageLimits {
        match self {
            SessionKind::ClaudeCodeCli => MessageLimits::same(MAX_MESSAGE_BYTES),
            SessionKind::Restricted => MessageLimits {
                incoming: MAX_RESTRICTED_INCOMING_MESSAGE_BYTES,
                outgoing: MAX_MESSAGE_BYTES,
            },
        }
    }

    /// ユーザーに見せる口 (ステータスバーの「接続中」・右クリックの送信先) の相手になるか。
    fn is_visible_to_the_user(self) -> bool {
        match self {
            SessionKind::ClaudeCodeCli => true,
            SessionKind::Restricted => false,
        }
    }
}

/// サンドボックスの中の CLI 用に IDE セッションを始める。
///
/// 差分タブを開き診断を集めるのは `workspace`。ローカルで誰とも共有していないプロジェクトで、
/// `worktree_roots` (ホストのパス) がすべてそのワークツリーでなければ、セッションは何も受け
/// 付けずに終わる。`workspace` を読むのはセッションのタスクの中なので、Workspace を更新中の
/// アクションの中から呼んでもよい。`connections` はブリッジが運んでくる接続 (1 本 = CLI からの
/// TCP 接続 1 本) で、これが終わったらブリッジが無くなったとみなしてセッションを畳む。
pub fn start_sandbox_session<S, C>(
    workspace: WeakEntity<Workspace>,
    connections: C,
    mapper: Arc<dyn PathMapper>,
    worktree_roots: Vec<PathBuf>,
    cx: &mut App,
) -> (SandboxIdeSessionHandle, SandboxSessionInfo)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    C: Stream<Item = S> + Send + Unpin + 'static,
{
    start_session(
        workspace,
        connections,
        Arc::new(SandboxPaths::new(mapper, worktree_roots)),
        SessionKind::ClaudeCodeCli,
        None,
        cx,
    )
}

/// [`start_sandbox_session`] に、セッションの CLI が走るタブのキーを添えたもの (ADR-F56)。
/// エージェントパネルの `AddSelectionToThread` は、このキーでタブからセッションを引き、選択範囲を
/// そのタブの CLI だけに送る ([`mention_selection_in_cli`])。
pub fn start_sandbox_session_for_tab<S, C>(
    workspace: WeakEntity<Workspace>,
    connections: C,
    mapper: Arc<dyn PathMapper>,
    worktree_roots: Vec<PathBuf>,
    tab_key: String,
    cx: &mut App,
) -> (SandboxIdeSessionHandle, SandboxSessionInfo)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    C: Stream<Item = S> + Send + Unpin + 'static,
{
    start_session(
        workspace,
        connections,
        Arc::new(SandboxPaths::new(mapper, worktree_roots)),
        SessionKind::ClaudeCodeCli,
        Some(tab_key),
        cx,
    )
}

/// サンドボックスの中の外部 ACP エージェント用に、`getDiagnostics` だけの制限セッションを
/// 始める (ADR-F49 決定 24)。
///
/// 受け入れの条件 (ローカルで共有していないプロジェクト・`paths` のルートがすべてその
/// ワークスペースのワークツリー)・認証・メッセージの上限は [`start_sandbox_session`] と
/// 同じで、違うのは次の 3 点。
///
/// - 出すツールは [`crate::tools::RESTRICTED_TOOL_NAMES`] だけで、差分タブは開かない
/// - ステータスバーの「接続中」表示にも、右クリックの「選択範囲を送る」の宛先にもしない。
///   ユーザーに見えている Claude Code の導線と混ざらないようにするため
/// - `connections` は streamable HTTP (1 要求 = 1 接続) なので、同時接続の上限が多い
///
/// `paths` を外から受けるのは、エージェントに渡す引数と結果のパスを変換している
/// `SandboxPaths` (ADR-F49 決定 23) と同じ実体を共有するため。
pub fn start_restricted_sandbox_session<S, C>(
    workspace: WeakEntity<Workspace>,
    connections: C,
    paths: Arc<SandboxPaths>,
    cx: &mut App,
) -> (SandboxIdeSessionHandle, SandboxSessionInfo)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    C: Stream<Item = S> + Send + Unpin + 'static,
{
    start_session(
        workspace,
        connections,
        paths,
        SessionKind::Restricted,
        None,
        cx,
    )
}

fn start_session<S, C>(
    workspace: WeakEntity<Workspace>,
    connections: C,
    paths: Arc<SandboxPaths>,
    kind: SessionKind,
    tab_key: Option<String>,
    cx: &mut App,
) -> (SandboxIdeSessionHandle, SandboxSessionInfo)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    C: Stream<Item = S> + Send + Unpin + 'static,
{
    let auth_token: Arc<str> = uuid::Uuid::new_v4().to_string().into();
    let session = cx.new(|cx| SandboxIdeSession::new(paths, kind, tab_key, cx));
    let weak_session = session.downgrade();
    // レジストリに載せるのはユーザーに見せるためなので、制限セッションは載せない。
    if kind.is_visible_to_the_user() && cx.has_global::<IdeServerRegistry>() {
        let weak_session = session.downgrade();
        cx.update_global::<IdeServerRegistry, ()>(|registry, _| {
            registry
                .sandbox_sessions
                .retain(|session| session.upgrade().is_some());
            registry.sandbox_sessions.push(weak_session);
        });
    }
    let task = cx.spawn({
        let auth_token = auth_token.clone();
        async move |cx| {
            run_session(
                session,
                workspace,
                connections,
                auth_token,
                kind.max_transports(),
                cx,
            )
            .await
        }
    });
    (
        SandboxIdeSessionHandle {
            session: weak_session,
            _task: task,
        },
        SandboxSessionInfo {
            auth_token: auth_token.to_string(),
        },
    )
}

/// サンドボックスの中の CLI が読むロックファイル (`<config>/ide/<port>.lock`) の中身。
///
/// 書くのは信頼できる側 (ヘルパーのブリッジ) で、`<config>/ide` はサンドボックスからは
/// 読み取り専用 (ADR-F49 決定 3)。CLI 2.1.269 はポートをファイル名から取るので、JSON には
/// ポートを入れない。
///
/// - `runningInWindows: false`: `true` だと CLI は WSL から Windows 側のゲートウェイの
///   アドレスへ繋ぎに行き、パスも `wslpath -w` で Windows の形に直してくる。ここでは
///   名前空間内の `127.0.0.1` に繋がせ、パスの変換は Zed が受け持つ
/// - `pid: 0`: CLI は pid が偽のとき、古いロックの掃除でポートに繋いでみて生死を決める
///   (`127.0.0.1:<port>` はヘルパーの転送口なので、ちょうどセッションの生死と一致する)。
///   祖先プロセスの照合はポートが `CLAUDE_CODE_SSE_PORT` と一致するロックでは行わず、
///   一致しないロックは pid が偽なら選ばない。Zed の pid は WSL の pid 空間では意味が無く、
///   無関係なプロセスと一致しうる
/// - `workspace_folders` は呼び出し側が CLI 側 (Linux) の形にしたもの
pub fn sandbox_lock_file_contents(auth_token: &str, workspace_folders: &[String]) -> Vec<u8> {
    LockFileContents {
        pid: SANDBOX_LOCK_PID,
        workspace_folders: workspace_folders.to_vec(),
        ide_name: IDE_NAME.to_string(),
        transport: "ws".to_string(),
        running_in_windows: false,
        auth_token: auth_token.to_string(),
    }
    .to_value()
    .to_string()
    .into_bytes()
}

async fn run_session<S, C>(
    session: Entity<SandboxIdeSession>,
    workspace: WeakEntity<Workspace>,
    connections: C,
    auth_token: Arc<str>,
    max_connections: usize,
    cx: &mut AsyncApp,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    C: Stream<Item = S> + Unpin + 'static,
{
    if let Err(error) = session.update(cx, |session, cx| session.attach(workspace, cx)) {
        log::warn!("ide_server: refused to start a sandbox IDE session: {error:#}");
        return;
    }
    let mut connections = connections.fuse();
    let mut active_connections = FuturesUnordered::new();
    let open_transports = Arc::new(AtomicUsize::new(0));
    loop {
        futures::select_biased! {
            () = active_connections.select_next_some() => {}
            connection = connections.next() => match connection {
                Some(stream) if open_transports.load(Ordering::SeqCst) >= max_connections => {
                    log::debug!(
                        "ide_server: refused a sandbox connection beyond the limit of {max_connections}"
                    );
                    drop(stream);
                }
                Some(stream) => {
                    active_connections.push(crate::run_connection(
                        session.clone(),
                        stream,
                        auth_token.clone(),
                        Some(OpenTransport::new(&open_transports)),
                        cx.clone(),
                    ));
                }
                None => break,
            },
        }
    }
    // ブリッジが無くなった = 転送されていた接続もすべて死んでいるので、処理中の要求ごと畳む。
    drop(active_connections);
    session.update(cx, |session, cx| session.shut_down(cx));
}

pub struct SandboxIdeSession {
    entity_id: EntityId,
    kind: SessionKind,
    paths: Arc<SandboxPaths>,
    workspace: Option<WeakEntity<Workspace>>,
    project_id: Option<EntityId>,
    connections: Vec<Arc<Outbox>>,
    open_diffs: OpenDiffs,
    request_permits: Arc<Semaphore>,
    /// 最後に送った `selection_changed`。同じものを撃ち直さない。
    last_selection: Option<Value>,
    selection_subscription: Option<Subscription>,
    workspace_subscription: Option<Subscription>,
    /// 結び付いたプロジェクト。表示を消すのに使う (ワークスペースは解放の途中で読めないことがある)。
    project: Option<WeakEntity<Project>>,
    /// ADR-F56: このセッションの CLI が走るタブ ([`start_sandbox_session_for_tab`])。
    tab_key: Option<String>,
    /// ADR-F56: アシストのツールの表示の枠に出す名前。
    assist_label: SharedString,
    /// 口ごとに開いているトランスポート (IDE の口とアシストの口で別の枠)。
    ide_transports: Arc<AtomicUsize>,
    assist_transports: Arc<AtomicUsize>,
}

impl SandboxIdeSession {
    fn new(
        paths: Arc<SandboxPaths>,
        kind: SessionKind,
        tab_key: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        // ハンドルを落とされたときは `run_session` の後始末が走らないので、ここで畳む。
        cx.on_release(|this, cx| this.shut_down(cx)).detach();
        Self {
            entity_id: cx.entity_id(),
            kind,
            paths,
            workspace: None,
            project_id: None,
            connections: Vec::new(),
            open_diffs: OpenDiffs::default(),
            request_permits: Arc::new(Semaphore::new(MAX_IN_FLIGHT_REQUESTS)),
            last_selection: None,
            selection_subscription: None,
            workspace_subscription: None,
            project: None,
            tab_key,
            assist_label: kind.default_assist_label().into(),
            ide_transports: Arc::new(AtomicUsize::new(0)),
            assist_transports: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// ADR-F56: このセッションのエージェントが描く表示の出どころ。セッションが終わると消す。
    fn assist_source(&self) -> AssistSource {
        AssistSource {
            id: format!("zed-ide-session-{}", self.entity_id.as_u64()).into(),
            label: self.assist_label.clone(),
        }
    }

    /// セッションをワークスペースに結び付ける。差分タブと診断はこのプロジェクトのものを
    /// 使うので、共有していない (collab の相手に見えない) プロジェクトで、セッションの
    /// ルートがすべてそのワークツリーであるときだけ受ける。
    ///
    /// ADR-F53: SSH リモートのプロジェクトも受ける。ファイルを持っているのは接続先なので、
    /// パスの実体確認はクライアントの FS ではなく接続先で行う ([`crate::tools`])。collab は
    /// 受けない ([`session_project_refusal`])。
    fn attach(
        &mut self,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let workspace = workspace
            .upgrade()
            .context("the workspace closed before the session started")?;
        let project = workspace.read(cx).project().clone();
        let (refusal, worktree_paths) = {
            let project = project.read(cx);
            let worktree_paths = project
                .visible_worktrees(cx)
                .map(|worktree| worktree.read(cx).abs_path())
                .collect::<Vec<_>>();
            (
                session_project_refusal(
                    project.is_local(),
                    project.is_via_remote_server(),
                    project.remote_id().is_some(),
                ),
                worktree_paths,
            )
        };
        if let Some(refusal) = refusal {
            anyhow::bail!(refusal);
        }
        let roots = self.paths.roots();
        let roots_are_worktrees = !roots.is_empty()
            && roots.iter().all(|root| {
                worktree_paths
                    .iter()
                    .any(|worktree_path| path_mapping::same_place(&self.paths, root, worktree_path))
            });
        anyhow::ensure!(
            roots_are_worktrees,
            "the sandbox's worktrees are not all open in the workspace"
        );

        self.project_id = Some(project.entity_id());
        self.project = Some(project.downgrade());
        self.workspace = Some(workspace.downgrade());
        // 制限セッションは通知を送らないので、アクティブなエディターも追わない
        // (ADR-F49 決定 24)。
        if self.kind.is_visible_to_the_user() {
            self.workspace_subscription = Some(cx.subscribe(
                &workspace,
                |this, _, event: &workspace::Event, cx| {
                    if matches!(
                        event,
                        workspace::Event::ActiveItemChanged
                            | workspace::Event::ItemAdded { .. }
                            | workspace::Event::ItemRemoved { .. }
                    ) {
                        this.watch_active_editor(cx);
                    }
                },
            ));
            self.watch_active_editor(cx);
        }
        self.publish_connection_state(cx);
        Ok(())
    }

    /// セッションの終わり: 差分タブを畳み、このセッションが描いたハイライト・注釈・ツアーを
    /// 消し、接続中の表示を戻す。何度呼んでもよい。
    fn shut_down(&mut self, cx: &mut App) {
        self.connections.clear();
        self.selection_subscription = None;
        self.workspace_subscription = None;
        for diff in self.open_diffs.take_all() {
            diff.update(cx, |diff, cx| diff.dismiss(cx));
        }
        self.clear_assist_display(cx);
        self.publish_connection_state(cx);
    }

    fn clear_assist_display(&self, cx: &mut App) {
        let Some(display) = ide_query::code_display(cx) else {
            return;
        };
        let Some(project) = self.project.as_ref().and_then(WeakEntity::upgrade) else {
            return;
        };
        let source = self.assist_source();
        // セッションはワークスペースの更新の中で落とされうる (ウィンドウを閉じたとき)。
        // 表示の側はエディターとワークスペースを更新するので、今の更新の外で消す。
        cx.defer(move |cx| display.clear(&project, &source, cx));
    }

    fn publish_connection_state(&self, cx: &mut App) {
        // 制限セッションはレジストリにも載っていないので、ここで数えると自分だけが
        // 「Claude Code が接続中」を立ててしまい、誰も降ろせなくなる。
        if !self.kind.is_visible_to_the_user() {
            return;
        }
        let Some(project_id) = self.project_id else {
            return;
        };
        let connected = !self.connections.is_empty()
            || crate::host_connection_count(project_id, cx) > 0
            || has_connected_session(project_id, Some(self.entity_id), cx);
        zed_actions::ide_server::set_claude_code_connected(project_id, connected, cx);
        // ステータスバーの項目が観測しているのはワークスペースの `IdeServer` なので、
        // サンドボックスの接続の変化もそこへ知らせる。
        if let Some(server) = crate::server_for_project_id(project_id, cx) {
            cx.notify(server.entity_id());
        }
    }

    fn watch_active_editor(&mut self, cx: &mut Context<Self>) {
        let editor = self
            .workspace
            .as_ref()
            .and_then(WeakEntity::upgrade)
            .and_then(|workspace| workspace.read(cx).active_item(cx))
            .and_then(|item| item.act_as::<Editor>(cx));
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

    fn publish_selection(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let params = match self.shareable_file_path(editor, cx) {
            Some(client_path) => match tools::selection_snapshot(editor, cx) {
                Some(snapshot) => snapshot.notification_params(Some(&client_path)),
                None => return,
            },
            // パスも本文も見せず、選択が無くなったことだけを伝える。
            None => SelectionSnapshot::cleared_notification_params(),
        };
        if self.last_selection.as_ref() == Some(&params) {
            return;
        }
        self.last_selection = Some(params.clone());
        self.broadcast("selection_changed", params);
    }

    /// 選択の本文を CLI に見せてよいファイルの (CLI 側の) パス。1 つのバッファーだけを映す
    /// エディターで、そのファイルがセッションのワークツリーの中にあり、ワークツリーの走査が
    /// リンクをたどった結果として外を指すと判断していないときだけ。検索結果のように複数の
    /// ファイルを並べるビューでは、選択が別のファイルにまたがりうる。
    fn shareable_file_path(&self, editor: &Entity<Editor>, cx: &App) -> Option<String> {
        let buffer = editor.read(cx).buffer().read(cx).as_singleton()?;
        let buffer = buffer.read(cx);
        let file = WorktreeFile::from_dyn(buffer.file())?;
        let worktree = file.worktree.read(cx);
        let worktree_is_a_root = self
            .paths
            .roots()
            .iter()
            .any(|root| path_mapping::same_place(&self.paths, root, &worktree.abs_path()));
        let entry = file
            .entry_id
            .and_then(|entry_id| worktree.entry_for_id(entry_id))?;
        if !worktree_is_a_root || entry.is_external {
            return None;
        }
        // ADR-F56: `private_files` (`.env` など) の選択は、パスも本文も CLI に見せない。
        let project_path = ProjectPath {
            worktree_id: worktree.id(),
            path: file.path.clone(),
        };
        if tools::is_private_file(&project_path, Some(entry), cx) {
            return None;
        }
        self.paths.to_client(&worktree.absolutize(&file.path))
    }

    /// 右クリックの「選択範囲を Claude Code に送る」。ワークツリーの外のファイルは送らない。
    /// 1 本にも積めなければ (相手が居ない・読まずに溜まっている) `false`。
    pub(crate) fn send_at_mention(
        &mut self,
        file_path: &Path,
        start_line: u32,
        end_line: u32,
        cx: &mut Context<Self>,
    ) -> bool {
        // 制限セッションはレジストリに載っていないので、ここには届かないはず。届いても、
        // ユーザーが選んだ範囲を ACP のエージェントへ横流ししない (ADR-F49 決定 24)。
        if !self.kind.is_visible_to_the_user() || self.connections.is_empty() {
            return false;
        }
        let Some(client_path) = self.paths.to_client(file_path) else {
            return false;
        };
        let sent = self.broadcast(
            "at_mentioned",
            tools::at_mention_params(&client_path, start_line, end_line),
        );
        self.publish_connection_state(cx);
        sent
    }

    fn broadcast(&mut self, method: &str, params: Value) -> bool {
        crate::broadcast_notification(&mut self.connections, method, params)
    }
}

impl ToolHost for SandboxIdeSession {
    fn workspace(&self) -> Option<WeakEntity<Workspace>> {
        self.workspace.clone()
    }

    fn tool_scope(&self) -> ToolScope {
        self.kind.tool_scope(self.paths.clone())
    }

    /// `getLatestSelection` はサンドボックスには出していない。
    fn latest_selection(&self) -> Option<&SelectionSnapshot> {
        None
    }

    fn open_diffs(&mut self) -> &mut OpenDiffs {
        &mut self.open_diffs
    }

    fn add_connection(&mut self, connection: Arc<Outbox>, cx: &mut Context<Self>) {
        self.connections.push(connection);
        self.publish_connection_state(cx);
    }

    fn remove_connection(&mut self, connection: &Arc<Outbox>, cx: &mut Context<Self>) {
        self.connections
            .retain(|existing| !Arc::ptr_eq(existing, connection) && !existing.is_closed());
        self.publish_connection_state(cx);
    }

    fn request_permits(&self) -> Option<Arc<Semaphore>> {
        Some(self.request_permits.clone())
    }

    fn max_undelivered_responses(&self) -> Option<usize> {
        Some(MAX_UNDELIVERED_RESPONSES)
    }

    fn message_limits(&self) -> MessageLimits {
        self.kind.message_limits()
    }

    fn max_open_diffs(&self) -> Option<usize> {
        Some(MAX_OPEN_DIFFS)
    }

    /// ADR-F56: IDE の口とアシストの口で、同時接続の枠を別に持つ。アシストの口の枠
    /// ([`MAX_ASSIST_CONNECTIONS`]) は CLI の 4 本とも制限セッションの 16 本とも別。
    fn connection_gate(&self) -> ConnectionGate {
        ConnectionGate {
            ide: EndpointGate {
                quota: Some((self.ide_transports.clone(), self.kind.max_connections())),
                ..EndpointGate::unlimited(self.kind.message_limits())
            },
            assist: EndpointGate {
                quota: Some((self.assist_transports.clone(), MAX_ASSIST_CONNECTIONS)),
                ..EndpointGate::unlimited(ASSIST_MESSAGE_LIMITS)
            },
        }
    }

    /// ADR-F56: `/mcp/zed` にはアシストのツールを、セッションと同じパスの規則で出す。
    /// 設定 `ide_server.assist_tools` が切れていれば、要求をその理由で断る。
    fn connection_scope(&self, route: &Route, cx: &App) -> Result<ConnectionScope, String> {
        match route.endpoint {
            Endpoint::Ide => Ok(ConnectionScope::without_assist(self.tool_scope())),
            Endpoint::Assist if IdeServerSettings::get_global(cx).assist_tools => {
                Ok(ConnectionScope {
                    tools: ToolScope::Assist(Some(self.paths.clone())),
                    assist: Some(self.assist_source()),
                })
            }
            Endpoint::Assist => Err(ASSIST_UNAVAILABLE_MESSAGE.to_string()),
        }
    }
}

/// このワークスペースに結び付いたサンドボックスのセッション。
pub(crate) fn sessions_for_workspace(
    workspace_id: EntityId,
    cx: &App,
) -> Vec<Entity<SandboxIdeSession>> {
    let Some(registry) = cx.try_global::<IdeServerRegistry>() else {
        return Vec::new();
    };
    registry
        .sandbox_sessions
        .iter()
        .filter_map(WeakEntity::upgrade)
        .filter(|session| {
            session
                .read(cx)
                .workspace
                .as_ref()
                .is_some_and(|workspace| workspace.entity_id() == workspace_id)
        })
        .collect()
}

/// このプロジェクトのサンドボックスのセッションのうち、CLI が WebSocket で繋がっているものの数。
pub(crate) fn connected_session_count(project_id: EntityId, cx: &App) -> usize {
    let Some(registry) = cx.try_global::<IdeServerRegistry>() else {
        return 0;
    };
    registry
        .sandbox_sessions
        .iter()
        .filter_map(WeakEntity::upgrade)
        .filter(|session| {
            let session = session.read(cx);
            session.project_id == Some(project_id) && !session.connections.is_empty()
        })
        .count()
}

/// このプロジェクトのサンドボックスのセッションに、WebSocket で繋がっている CLI があるか。
/// `except` は更新中で読めない (呼び出し元自身の) セッション。
pub(crate) fn has_connected_session(
    project_id: EntityId,
    except: Option<EntityId>,
    cx: &App,
) -> bool {
    let Some(registry) = cx.try_global::<IdeServerRegistry>() else {
        return false;
    };
    registry
        .sandbox_sessions
        .iter()
        .filter(|session| Some(session.entity_id()) != except)
        .filter_map(WeakEntity::upgrade)
        .any(|session| {
            let session = session.read(cx);
            session.project_id == Some(project_id) && !session.connections.is_empty()
        })
}
