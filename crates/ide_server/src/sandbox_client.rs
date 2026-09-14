//! サンドボックス (WSL) の中で動く Claude Code CLI 用の IDE セッション (ADR-F49 決定 3・4)。
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
//!   ワークツリーの中に閉じ込める ([`crate::path_mapping`])。こちらから送る通知
//!   (`selection_changed` / `at_mentioned`) のパスも CLI 側の形に直し、ワークツリーの外は
//!   見せない
//! - サンドボックスの中のプロセスはロックファイルからトークンを読めるので、認証を通った
//!   相手も信用しない。同時に処理する要求・届け終えていない応答・開いておける差分タブ・
//!   開いている接続の数と、1 メッセージの大きさに上限を置く

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Context as _;
use editor::{Editor, EditorEvent};
use futures::stream::FuturesUnordered;
use futures::{AsyncRead, AsyncWrite, Stream, StreamExt as _};
use gpui::{
    App, AppContext as _, AsyncApp, BorrowAppContext as _, Context, Entity, EntityId, Subscription,
    Task, WeakEntity,
};
use project::File as WorktreeFile;
use serde_json::Value;
use smol::lock::Semaphore;
use workspace::Workspace;

use crate::path_mapping::{self, PathMapper, SandboxPaths};
use crate::protocol::LockFileContents;
use crate::tools::{self, OpenDiffs, SelectionSnapshot, ToolHost, ToolScope};
use crate::transport::Outbox;
use crate::{IDE_NAME, IdeServerRegistry, OpenTransport};

/// サンドボックスのロックファイルに書く pid。理由は [`sandbox_lock_file_contents`]。
const SANDBOX_LOCK_PID: u32 = 0;
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
const MAX_CONNECTIONS: usize = 4;
/// 1 メッセージの上限。旧内容の上限 (4 MiB) ほどの提案を、JSON のエスケープで膨らんでも運べる大きさ。
const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

pub struct SandboxSessionInfo {
    /// ロックファイルの `authToken`。CLI はこれを `X-Claude-Code-Ide-Authorization` で出す。
    pub auth_token: String,
}

/// 落とすとセッションが終わる。転送されてきた接続も、処理中の要求も閉じ、このセッションが
/// 開いた差分タブを畳む。
#[must_use = "dropping the handle ends the sandbox IDE session"]
pub struct SandboxIdeSessionHandle {
    _task: Task<()>,
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
    let auth_token: Arc<str> = uuid::Uuid::new_v4().to_string().into();
    let paths = Arc::new(SandboxPaths::new(mapper, worktree_roots));
    let session = cx.new(|cx| SandboxIdeSession::new(paths, cx));
    if cx.has_global::<IdeServerRegistry>() {
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
        async move |cx| run_session(session, workspace, connections, auth_token, cx).await
    });
    (
        SandboxIdeSessionHandle { _task: task },
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
                Some(stream) if open_transports.load(Ordering::SeqCst) >= MAX_CONNECTIONS => {
                    log::debug!(
                        "ide_server: refused a sandbox connection beyond the limit of {MAX_CONNECTIONS}"
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
}

impl SandboxIdeSession {
    fn new(paths: Arc<SandboxPaths>, cx: &mut Context<Self>) -> Self {
        // ハンドルを落とされたときは `run_session` の後始末が走らないので、ここで畳む。
        cx.on_release(|this, cx| this.shut_down(cx)).detach();
        Self {
            entity_id: cx.entity_id(),
            paths,
            workspace: None,
            project_id: None,
            connections: Vec::new(),
            open_diffs: OpenDiffs::default(),
            request_permits: Arc::new(Semaphore::new(MAX_IN_FLIGHT_REQUESTS)),
            last_selection: None,
            selection_subscription: None,
            workspace_subscription: None,
        }
    }

    /// セッションをワークスペースに結び付ける。差分タブと診断はこのプロジェクトのものを
    /// 使うので、ローカルで共有もしていない (collab の相手に見えない) もので、セッションの
    /// ルートがすべてそのワークツリーであるときだけ受ける。
    fn attach(
        &mut self,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let workspace = workspace
            .upgrade()
            .context("the workspace closed before the session started")?;
        let project = workspace.read(cx).project().clone();
        let (is_private_local, worktree_paths) = {
            let project = project.read(cx);
            let worktree_paths = project
                .visible_worktrees(cx)
                .map(|worktree| worktree.read(cx).abs_path())
                .collect::<Vec<_>>();
            (
                project.is_local() && project.remote_id().is_none(),
                worktree_paths,
            )
        };
        anyhow::ensure!(
            is_private_local,
            "the project is not a local project that only this machine sees"
        );
        let roots = self.paths.roots();
        let roots_are_worktrees = !roots.is_empty()
            && roots.iter().all(|root| {
                worktree_paths
                    .iter()
                    .any(|worktree_path| path_mapping::same_location(root, worktree_path))
            });
        anyhow::ensure!(
            roots_are_worktrees,
            "the sandbox's worktrees are not all open in the workspace"
        );

        self.project_id = Some(project.entity_id());
        self.workspace = Some(workspace.downgrade());
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
        self.publish_connection_state(cx);
        Ok(())
    }

    /// セッションの終わり: 差分タブを畳み、接続中の表示を戻す。何度呼んでもよい。
    fn shut_down(&mut self, cx: &mut App) {
        self.connections.clear();
        self.selection_subscription = None;
        self.workspace_subscription = None;
        for diff in self.open_diffs.take_all() {
            diff.update(cx, |diff, cx| diff.dismiss(cx));
        }
        self.publish_connection_state(cx);
    }

    fn publish_connection_state(&self, cx: &mut App) {
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
            .any(|root| path_mapping::same_location(root, &worktree.abs_path()));
        let entry = file
            .entry_id
            .and_then(|entry_id| worktree.entry_for_id(entry_id))?;
        if !worktree_is_a_root || entry.is_external {
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
        if self.connections.is_empty() {
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
        ToolScope::Sandboxed(self.paths.clone())
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

    fn max_message_bytes(&self) -> usize {
        MAX_MESSAGE_BYTES
    }

    fn max_open_diffs(&self) -> Option<usize> {
        Some(MAX_OPEN_DIFFS)
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
