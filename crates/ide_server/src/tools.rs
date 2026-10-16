//! IDE が公開する MCP ツール。
//!
//! 名前と引数は Claude Code CLI 2.1.261 が実際に呼ぶ形に合わせてある
//! (`openDiff` / `close_tab` / `closeAllDiffTabs` / `getDiagnostics`)。残りは VSCode /
//! JetBrains 拡張と同じ名前で、`.mcp.json` から繋ぐ他のクライアント向け。
//!
//! CLI はモデルに見せるツールを `mcp__ide__executeCode` と `mcp__ide__getDiagnostics` の
//! 2 つに絞る (それ以外の `mcp__ide__*` は CLI が内部から直接呼ぶ) ので、ここで余分に
//! 公開してもモデルのツール一覧は膨らまない。
//!
//! サンドボックスのセッション (ADR-F49) には CLI が呼ぶ 4 本だけを出し、外部の ACP
//! エージェント用の制限セッション (ADR-F49 決定 24) には `getDiagnostics` だけを出す
//! ([`ToolScope`])。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use anyhow::{Context as _, Result, anyhow};
use base64::Engine as _;
use browser_control::{BrowserControl, BrowserOutput};
use collections::HashSet;
use context_server::types::{
    CallToolParams, Implementation, LATEST_PROTOCOL_VERSION, ProtocolVersion, ServerCapabilities,
    Tool, ToolsCapabilities, VERSION_2024_11_05, VERSION_2025_03_26, VERSION_2025_06_18,
};
use editor::{Editor, MultiBufferOffset, SelectionEffects, scroll::Autoscroll};
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, WeakEntity};
use language::{Buffer, DiagnosticSeverity, PointUtf16};
use project::{File as WorktreeFile, Fs, Project, ProjectPath, Worktree};
use serde_json::{Value, json};
use settings::Settings as _;
use smol::lock::Semaphore;
use workspace::Workspace;

use crate::diff_view::{self, ProposedDiff};
use ide_query::AssistSource;
use util::path_mapping::SandboxPaths;

pub use crate::assist_tools::ASSIST_TOOL_NAMES;
use crate::path_mapping::{self, MAX_SANDBOX_FILE_BYTES, RealRoots, SandboxFile};
use crate::transport::{Endpoint, HOST_MAX_MESSAGE_BYTES, MessageLimits, Outbox, Route};
use crate::{ConnectionGate, IdeServerDiff, IdeServerDiffOutcome};

/// 全体診断で 1 回に開くバッファーの上限。壊れたワークツリーで固まらないための保険。
/// 超えた分は `nextCursor` で続きを取る。
const MAX_DIAGNOSTIC_FILES: usize = 500;

/// `getLastTaskOutput` が返すタスクの出力の上限 (末尾を残す)。
pub(crate) const MAX_TASK_OUTPUT_BYTES: usize = 4096;

/// サンドボックスのセッションで出すツール。CLI が内部から呼ぶ 4 本だけ。ホストの任意の
/// パスを開く `openFile` や、保存で整形 (= ワークツリーのコードの実行) を引き起こす
/// `saveDocument` などは出さない。
pub const SANDBOX_TOOL_NAMES: [&str; 4] = [
    "openDiff",
    "close_tab",
    "closeAllDiffTabs",
    "getDiagnostics",
];

/// 制限セッションで出すツール (ADR-F49 決定 24)。
///
/// サンドボックスの中の外部 ACP エージェント (Claude Code CLI 以外) に渡す MCP は、
/// 読むだけの `getDiagnostics` 1 本に絞る。`openDiff` はユーザーが採用か拒否を決めるまで
/// 応答を返さないので、決定を待てない ACP のツール呼び出しには出せない
/// (`close_tab` / `closeAllDiffTabs` はその後始末なので、まとめて出さない)。
pub const RESTRICTED_TOOL_NAMES: [&str; 1] = ["getDiagnostics"];

/// どのツールを出し、ツールの引数と結果のパスをどう扱うか。
#[derive(Clone)]
pub enum ToolScope {
    /// ホストの TCP サーバー。ツールはすべて出し、パスはそのまま扱う。
    Full,
    /// サンドボックスのセッション。ツールは [`SANDBOX_TOOL_NAMES`] だけで、パスは変換した
    /// うえでワークツリーの中に閉じ込める。
    Sandboxed(Arc<SandboxPaths>),
    /// サンドボックスの制限セッション (ADR-F49 決定 24)。ツールは [`RESTRICTED_TOOL_NAMES`]
    /// だけで、パスの扱いは [`ToolScope::Sandboxed`] と同じ。
    SandboxedReadOnly(Arc<SandboxPaths>),
    /// アシストの口 (`/mcp/zed`、ADR-F56)。ここ自体が出すツールは無く、アシストのツール
    /// ([`ASSIST_TOOL_NAMES`]) は [`ConnectionScope::assist`] が出す。`Some` ならサンドボックスの
    /// セッションの口で、パスの扱いは [`ToolScope::Sandboxed`] と同じ。
    Assist(Option<Arc<SandboxPaths>>),
    /// ADR-F102: the AI browser's endpoint (`/mcp/zed-browser`) on the host's
    /// server, for one external agent's session. Its only tools are the
    /// browser's ([`offers_browser_tools`]), and only while that session still
    /// has them.
    Browser(BrowserSession),
}

/// ADR-F102: the session a `/mcp/zed-browser` request came in for. Looked up
/// again at each use, so that a session whose access is revoked after its
/// request was let in (reopened without the browser, closed) gets neither the
/// list nor the call.
#[derive(Clone)]
pub struct BrowserSession {
    tokens: crate::transport::ClientTokens,
    token: Arc<str>,
    client_id: Arc<str>,
}

impl BrowserSession {
    pub(crate) fn new(
        tokens: crate::transport::ClientTokens,
        token: &str,
        client_id: &str,
    ) -> Self {
        Self {
            tokens,
            token: token.into(),
            client_id: client_id.into(),
        }
    }

    pub(crate) fn is_live(&self) -> bool {
        self.tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&*self.token)
            .is_some_and(|client_id| **client_id == *self.client_id)
    }
}

impl ToolScope {
    pub fn allows_tool(&self, name: &str) -> bool {
        match self {
            ToolScope::Full => true,
            ToolScope::Sandboxed(_) => SANDBOX_TOOL_NAMES.contains(&name),
            ToolScope::SandboxedReadOnly(_) => RESTRICTED_TOOL_NAMES.contains(&name),
            ToolScope::Assist(_) | ToolScope::Browser(_) => false,
        }
    }

    /// サンドボックス向けのパスの変換。ホストの TCP サーバーでは `None` で、これが `Some` の
    /// あいだは相手をサンドボックスの中のプロセスとして扱う (パスを変換し、ワークツリーの外と
    /// ホストのエラー文を見せない)。
    pub fn sandbox_paths(&self) -> Option<&Arc<SandboxPaths>> {
        match self {
            ToolScope::Full | ToolScope::Assist(None) | ToolScope::Browser(_) => None,
            ToolScope::Sandboxed(paths)
            | ToolScope::SandboxedReadOnly(paths)
            | ToolScope::Assist(Some(paths)) => Some(paths),
        }
    }
}

/// 1 本の接続が出すツール。口 (`/mcp` か `/mcp/zed` か) と話し方で決まる。
#[derive(Clone)]
pub struct ConnectionScope {
    pub tools: ToolScope,
    /// `Some` なら、アシストのツールも出し、表示はこの出どころとして描く。
    pub assist: Option<AssistSource>,
    /// ADR-F102: the client takes the browser's tools from their own endpoint
    /// (an external agent's `zed-browser` server), so this one leaves them out
    /// and a thread can turn the browser off without losing the IDE tools.
    pub browser_separate: bool,
}

impl ConnectionScope {
    /// アシストのツールを持たない、これまでどおりの口。
    pub fn without_assist(tools: ToolScope) -> Self {
        Self {
            tools,
            assist: None,
            browser_separate: false,
        }
    }

    pub fn allows_tool(&self, name: &str) -> bool {
        self.tools.allows_tool(name) || (self.assist.is_some() && ASSIST_TOOL_NAMES.contains(&name))
    }

    pub fn is_assist_tool(&self, name: &str) -> bool {
        self.assist.is_some() && ASSIST_TOOL_NAMES.contains(&name)
    }
}

/// サンドボックスがまとめて診断を問い合わせたときに返すファイルの上限。
const MAX_SANDBOX_DIAGNOSTIC_FILES: usize = 100;
/// `close_tab` を待つあいだ覚えておく、決着の付いたタブ名の数。
const MAX_SETTLED_TAB_NAMES: usize = 32;

/// 差分タブを登録するときの鍵。タブ名は CLI が決めるので重なりうる。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiffId(u64);

impl DiffId {
    pub fn next() -> Self {
        static NEXT_DIFF_ID: AtomicU64 = AtomicU64::new(1);
        Self(NEXT_DIFF_ID.fetch_add(1, Ordering::Relaxed))
    }
}

/// 要求を運んできた接続。`close_tab` は、同じ接続が開いた差分と覚えた名前を先に見る。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConnectionId(u64);

impl ConnectionId {
    pub(crate) fn next() -> Self {
        static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);
        Self(NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed))
    }
}

struct OpenDiff {
    id: DiffId,
    connection: ConnectionId,
    tab_name: String,
    diff: Entity<IdeServerDiff>,
}

/// 開いている差分タブ。
///
/// CLI はタブにファイル名だけを付けるので、同じ名前の差分が同時に開きうる。CLI は `openDiff`
/// の答えを受け取るとその名前で `close_tab` を送ってくるが、それが指すのは答えを返した差分で
/// ある。ユーザーがタブで決めた (またはこちらが置き換えた) 差分の名前を接続ごとに覚えておき、
/// その `close_tab` は消し込むだけにして、同じ名前で待っている別の差分を閉じない。
#[derive(Default)]
pub struct OpenDiffs {
    entries: Vec<OpenDiff>,
    settled_tab_names: VecDeque<(ConnectionId, String)>,
    /// 旧内容を読み、バッファーを作っているところで、まだ登録していない `openDiff`。
    in_flight: Arc<AtomicUsize>,
}

/// `close_tab` が指していたもの。
enum ClosedDiff {
    /// 答えを返し終えた差分。タブはもう閉じている。
    Settled,
    Pending(Entity<IdeServerDiff>),
    Unknown,
}

/// 登録するまで [`OpenDiffs`] の上限の枠を 1 つ押さえておく。落とすと返す。
pub(crate) struct InFlightDiff(Arc<AtomicUsize>);

impl Drop for InFlightDiff {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl OpenDiffs {
    /// 旧内容を読む前に上限の枠を押さえる。登録済みの差分と準備中の差分を、同じ名前で置き換える
    /// ものも含めてすべて数える。
    pub(crate) fn reserve(&self, limit: usize) -> Result<InFlightDiff> {
        if self.entries.len() + self.in_flight.load(Ordering::SeqCst) >= limit {
            return Err(too_many_diffs_error(limit));
        }
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        Ok(InFlightDiff(self.in_flight.clone()))
    }

    /// 登録する。`replace_same_name` なら同じ名前の差分を閉じて (待っている `openDiff` には
    /// 拒否として返して) 置き換える。
    fn register(
        &mut self,
        new_diff: OpenDiff,
        replace_same_name: bool,
        limit: Option<usize>,
        cx: &mut App,
    ) -> Result<()> {
        let connection = new_diff.connection;
        let has_pending_same_name = self
            .entries
            .iter()
            .any(|entry| entry.connection == connection && entry.tab_name == new_diff.tab_name);
        if replace_same_name {
            let (replaced, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.entries)
                .into_iter()
                .partition(|entry| entry.tab_name == new_diff.tab_name);
            self.entries = kept;
            for entry in replaced {
                entry.diff.update(cx, |diff, cx| diff.dismiss(cx));
                self.settle(entry.connection, entry.tab_name);
            }
        }
        if let Some(limit) = limit
            && self.entries.len() >= limit
        {
            return Err(too_many_diffs_error(limit));
        }
        // この接続で同じ名前の差分が待っていないのに残っている決着済みの名前は、CLI が
        // `close_tab` を送ってこなかった古い差分のもの。残すと、この差分に宛てた `close_tab` を
        // 消し込んでしまう。
        if !has_pending_same_name {
            self.settled_tab_names
                .retain(|(settled_connection, tab_name)| {
                    *settled_connection != connection || *tab_name != new_diff.tab_name
                });
        }
        self.entries.push(new_diff);
        Ok(())
    }

    /// `openDiff` が答えを返すとき。まだ登録されていれば、ユーザーがタブで決めたので、
    /// その名前の `close_tab` を消し込めるよう覚えておく。
    fn finish(&mut self, id: DiffId) {
        if let Some(index) = self.entries.iter().position(|entry| entry.id == id) {
            let entry = self.entries.remove(index);
            self.settle(entry.connection, entry.tab_name);
        }
    }

    fn settle(&mut self, connection: ConnectionId, tab_name: String) {
        if self.settled_tab_names.len() >= MAX_SETTLED_TAB_NAMES {
            self.settled_tab_names.pop_front();
        }
        self.settled_tab_names.push_back((connection, tab_name));
    }

    /// CLI の `close_tab`。決着済みの名前を先に消し込み、なければ同じ名前で待っている
    /// いちばん古い差分を閉じる。どちらも同じ接続のものを先に探し、なければセッション全体
    /// (繋ぎ直す前の接続を含む) から探す。
    fn close_named(&mut self, connection: ConnectionId, tab_name: &str) -> ClosedDiff {
        for same_connection_only in [true, false] {
            let in_scope =
                |candidate: ConnectionId| !same_connection_only || candidate == connection;
            if let Some(index) =
                self.settled_tab_names
                    .iter()
                    .position(|(settled_connection, name)| {
                        in_scope(*settled_connection) && name == tab_name
                    })
            {
                self.settled_tab_names.remove(index);
                return ClosedDiff::Settled;
            }
            if let Some(index) = self
                .entries
                .iter()
                .position(|entry| in_scope(entry.connection) && entry.tab_name == tab_name)
            {
                return ClosedDiff::Pending(self.entries.remove(index).diff);
            }
        }
        ClosedDiff::Unknown
    }

    pub fn take_all(&mut self) -> Vec<Entity<IdeServerDiff>> {
        self.settled_tab_names.clear();
        std::mem::take(&mut self.entries)
            .into_iter()
            .map(|entry| entry.diff)
            .collect()
    }

    #[cfg(test)]
    pub fn oldest_named(&self, tab_name: &str) -> Option<Entity<IdeServerDiff>> {
        self.entries
            .iter()
            .find(|entry| entry.tab_name == tab_name)
            .map(|entry| entry.diff.clone())
    }
}

/// JSON-RPC の要求を受ける側 (ホストの [`crate::IdeServer`] とサンドボックスのセッション)。
pub trait ToolHost: 'static + Sized {
    /// 差分タブを開き、診断を集めるワークスペース。まだ決まっていなければ `None`。
    fn workspace(&self) -> Option<WeakEntity<Workspace>>;
    fn tool_scope(&self) -> ToolScope;
    fn latest_selection(&self) -> Option<&SelectionSnapshot>;
    fn open_diffs(&mut self) -> &mut OpenDiffs;
    /// `getLastTaskOutput` が返す、このワークスペースで最後に失敗したタスク。
    fn last_task_failure(&self) -> Option<&TaskFailure> {
        None
    }
    /// WebSocket の接続を、こちらから撃つ通知の宛先に加える。
    fn add_connection(&mut self, connection: Arc<Outbox>, cx: &mut Context<Self>);
    fn remove_connection(&mut self, connection: &Arc<Outbox>, cx: &mut Context<Self>);

    /// 1 セッションで同時に処理する要求の枠。`None` なら上限なし。
    fn request_permits(&self) -> Option<Arc<Semaphore>> {
        None
    }

    /// 1 本の接続で、相手に届け終えていない応答 (処理中を含む) の上限。`None` なら上限なし。
    fn max_undelivered_responses(&self) -> Option<usize> {
        None
    }

    /// WebSocket の 1 メッセージ・HTTP の 1 本文の上限。
    fn message_limits(&self) -> MessageLimits {
        MessageLimits::same(HOST_MAX_MESSAGE_BYTES)
    }

    /// 同時に開いておける差分タブの数。`None` なら上限なし。
    fn max_open_diffs(&self) -> Option<usize> {
        None
    }

    /// 口ごとの受け入れの判断 (開いているか・同時接続の枠・メッセージの上限)。トランスポートが
    /// 要求の頭を読んだところで、バックグラウンドで使う。
    fn connection_gate(&self) -> ConnectionGate {
        ConnectionGate::unlimited(self.message_limits())
    }

    /// 受け入れた接続が出すツール。`Err` は理由 (日本語) で、その接続への要求はすべてこれで
    /// 断る。既定では IDE の口だけを持ち、アシストの口は持たない。
    fn connection_scope(
        &self,
        route: &Route,
        _cx: &App,
    ) -> std::result::Result<ConnectionScope, String> {
        match route.endpoint {
            Endpoint::Ide => Ok(ConnectionScope::without_assist(self.tool_scope())),
            Endpoint::Assist | Endpoint::Browser => {
                Err(crate::assist_tools::ASSIST_UNAVAILABLE_MESSAGE.to_string())
            }
        }
    }
}

/// サンドボックスの CLI にそのまま見せてよいエラー (CLI 側のパスしか含まない)。それ以外の
/// エラーはホストのパスや OS のメッセージを含みうるので、サンドボックスには伏せる。
#[derive(Debug)]
struct ClientError(String);

impl std::fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ClientError {}

pub(crate) fn client_error(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(ClientError(message.into()))
}

/// 見せてよいエラー ([`ClientError`]) にだけ前置きを付ける。それ以外はホストの文を含みうるので
/// そのまま返し、[`error_text`] に伏せさせる。
pub(crate) fn prefix_client_error(prefix: &str, error: anyhow::Error) -> anyhow::Error {
    match error.downcast_ref::<ClientError>() {
        Some(visible) => client_error(format!("{prefix}{visible}")),
        None => error,
    }
}

fn too_many_diffs_error(limit: usize) -> anyhow::Error {
    client_error(format!(
        "{limit} diff tabs are already open for this session; close one before proposing another"
    ))
}

/// ツールの失敗を、呼び出し元に見せる文にする。サンドボックスには [`ClientError`] だけを
/// そのまま見せ、残りはホストのログにだけ残す。
pub(crate) fn error_text(tool: &str, error: &anyhow::Error, scope: &ToolScope) -> String {
    if scope.sandbox_paths().is_none() {
        return format!("{error:#}");
    }
    match error.downcast_ref::<ClientError>() {
        Some(visible) => visible.to_string(),
        None => {
            // 相手の求めごとに出るので、ログを埋めさせないよう debug に留める。
            log::debug!("ide_server: {tool} failed for a sandboxed client: {error:#}");
            format!("{tool} failed")
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectionSnapshot {
    pub file_path: Option<PathBuf>,
    pub text: String,
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
}

impl SelectionSnapshot {
    pub fn is_empty(&self) -> bool {
        self.start_line == self.end_line && self.start_character == self.end_character
    }

    fn range_json(&self) -> Value {
        json!({
            "start": { "line": self.start_line, "character": self.start_character },
            "end": { "line": self.end_line, "character": self.end_character },
            "isEmpty": self.is_empty(),
        })
    }

    /// CLI が受ける `selection_changed` 通知の形 (ホストのパスのまま)。
    pub fn to_notification_params(&self, share_active_file: bool) -> Value {
        let file_path = self
            .file_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        self.notification_params_for_cli(file_path.as_deref(), share_active_file)
    }

    /// パスだけを CLI 側の形に差し替えた `selection_changed` 通知。
    ///
    /// パスが無ければ `filePath` のキーごと省く。CLI (2.1.281) は `filePath` を
    /// `string().optional()` で検査するので、`null` を入れると通知ごと捨てられ、前のファイルの
    /// 「⧉ In <ファイル名>」が消えずに残る。
    pub fn notification_params(&self, file_path: Option<&str>) -> Value {
        let mut params = json!({
            "selection": {
                "start": { "line": self.start_line, "character": self.start_character },
                "end": { "line": self.end_line, "character": self.end_character },
            },
            "text": self.text,
        });
        if let (Some(file_path), Some(object)) = (file_path, params.as_object_mut()) {
            object.insert("filePath".to_string(), json!(file_path));
        }
        params
    }

    /// CLI に見せるファイルが無くなったとき (見せてはいけないバッファー・エディター以外の項目・
    /// 全部閉じた・共有を切った) の通知。選択が無くなったことだけを伝え、CLI の表示を消す。
    pub fn cleared_notification_params() -> Value {
        SelectionSnapshot {
            file_path: None,
            text: String::new(),
            start_line: 0,
            start_character: 0,
            end_line: 0,
            end_character: 0,
        }
        .notification_params(None)
    }

    /// `selection_changed` で CLI に送る形。`share_active_file` が切れていれば、空の選択
    /// (ファイルを開いているだけ) は消す通知にし、行を選んだときだけ送る。
    pub fn notification_params_for_cli(
        &self,
        file_path: Option<&str>,
        share_active_file: bool,
    ) -> Value {
        if !share_active_file && self.is_empty() {
            Self::cleared_notification_params()
        } else {
            self.notification_params(file_path)
        }
    }

    fn to_tool_result(&self) -> Value {
        json!({
            "success": true,
            "text": self.text,
            "filePath": self.file_path.as_ref().map(|path| path.to_string_lossy().into_owned()),
            "fileUrl": self.file_path.as_ref().map(|path| file_uri(path)),
            "selection": self.range_json(),
        })
    }
}

/// Windows のドライブ文字を含む絶対パスも扱えるようにした `file://` URI。
///
/// パーセントエンコードはしない — CLI 側の正規化 (`normalizeFileUri`) はデコードを
/// 行わないので、`%3A` を混ぜるとパスの一致判定に失敗する。
pub fn file_uri(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if text.starts_with('/') {
        format!("file://{text}")
    } else {
        format!("file:///{text}")
    }
}

/// `file://` URI とただの絶対パスの両方を受ける (CLI は `file://I:\a\b` のような
/// 非標準の形も送ってくる)。
pub fn path_from_uri(uri: &str) -> PathBuf {
    let rest = uri.strip_prefix("file://").unwrap_or(uri);
    let rest = match rest.strip_prefix('/') {
        Some(without_slash)
            if without_slash
                .as_bytes()
                .get(1)
                .is_some_and(|byte| *byte == b':') =>
        {
            without_slash
        }
        _ => rest,
    };
    PathBuf::from(rest)
}

/// アシストのツールを出す接続には、モデル向けの `instructions` (MCP の initialize の結果) を
/// 付ける。CLI は MCP サーバーの `instructions` をシステムプロンプトに入れるので、モデルは
/// これで「ユーザーが見ている Zed に繋がっている」ことと、いつ何を使えばよいかを知る。
/// CLI の IDE の口 (WebSocket、名前 `ide`) には付けない (CLI がどう扱うか確かめていない)。
pub fn initialize_result_in(params: &Value, scope: &ConnectionScope) -> Value {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let negotiated = match requested {
        VERSION_2024_11_05 | VERSION_2025_03_26 | VERSION_2025_06_18 | LATEST_PROTOCOL_VERSION => {
            requested
        }
        _ => LATEST_PROTOCOL_VERSION,
    };
    let mut result = initialize_result_body(negotiated);
    if scope.assist.is_some()
        && let Some(object) = result.as_object_mut()
    {
        object.insert(
            "instructions".to_string(),
            Value::String(crate::assist_tools::INSTRUCTIONS.to_string()),
        );
    }
    result
}

fn initialize_result_body(negotiated: &str) -> Value {
    json!({
        "protocolVersion": ProtocolVersion(negotiated.to_string()),
        "capabilities": ServerCapabilities {
            experimental: None,
            logging: None,
            completions: None,
            prompts: None,
            resources: None,
            tools: Some(ToolsCapabilities { list_changed: Some(false) }),
        },
        "serverInfo": Implementation {
            name: crate::PRODUCT_NAME.to_string(),
            title: Some(crate::PRODUCT_NAME.to_string()),
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: None,
        },
    })
}

pub(crate) fn tool(name: &str, description: &str, schema: Value) -> Tool {
    Tool {
        name: name.to_string(),
        title: None,
        description: Some(description.to_string()),
        input_schema: schema,
        output_schema: None,
        annotations: None,
    }
}

pub(crate) fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

pub fn tools() -> Vec<Tool> {
    vec![
        tool(
            "getWorkspaceFolders",
            "Get all workspace folders currently open in the IDE.",
            object_schema(json!({}), &[]),
        ),
        tool(
            "getOpenEditors",
            "Get information about the editors currently open in the IDE.",
            object_schema(json!({}), &[]),
        ),
        tool(
            "getCurrentSelection",
            "Get the current text selection in the active editor.",
            object_schema(json!({}), &[]),
        ),
        tool(
            "getLatestSelection",
            "Get the most recent text selection, even if the editor is no longer focused.",
            object_schema(json!({}), &[]),
        ),
        tool(
            "getDiagnostics",
            "Get language server diagnostics. Pass a file URI to limit the result to one file, \
             or omit it to get diagnostics for every file that has them. When there are more \
             files than one call returns, a second content block carries a nextCursor; pass it \
             back as cursor to get the next page.",
            object_schema(
                json!({
                    "uri": {
                        "type": "string",
                        "description": "Optional file:// URI. Omit for all files."
                    },
                    "cursor": {
                        "type": "string",
                        "description": "The nextCursor of a previous call without uri, to continue where it stopped."
                    }
                }),
                &[],
            ),
        ),
        tool(
            "getLastTaskOutput",
            branding::concat_const!(
                "Get the output of the most recent ",
                crate::PRODUCT_NAME,
                " task that failed (exited with a non-zero status) in this workspace: the last \
                 4 KB of its terminal, with the task's label, command and exit code."
            ),
            object_schema(json!({}), &[]),
        ),
        tool(
            "openFile",
            "Open a file in the editor and optionally select a range of text.",
            object_schema(
                json!({
                    "filePath": { "type": "string", "description": "Absolute or workspace-relative path." },
                    "preview": { "type": "boolean", "description": "Open as a preview tab." },
                    "startLine": { "type": "integer", "description": "Zero-based line to place the cursor on." },
                    "startColumn": { "type": "integer", "description": "Zero-based UTF-16 column of the selection start." },
                    "endLine": { "type": "integer", "description": "Zero-based line of the selection end." },
                    "endColumn": { "type": "integer", "description": "Zero-based UTF-16 column of the selection end." },
                    "startText": { "type": "string", "description": "Text to search for; the selection starts at its first occurrence." },
                    "endText": { "type": "string", "description": "Text to search for; the selection ends after its first occurrence past startText." },
                    "selectToEndOfLine": { "type": "boolean", "description": "Extend the selection to the end of the last line." },
                    "makeFrontmost": { "type": "boolean", "description": "Focus the tab. Defaults to true." }
                }),
                &["filePath"],
            ),
        ),
        tool(
            "checkDocumentDirty",
            "Check whether a file has unsaved changes in the IDE.",
            object_schema(json!({ "filePath": { "type": "string" } }), &["filePath"]),
        ),
        tool(
            "saveDocument",
            "Save a file that is open in the IDE.",
            object_schema(json!({ "filePath": { "type": "string" } }), &["filePath"]),
        ),
        tool(
            "close_tab",
            "Close a tab by its name.",
            object_schema(json!({ "tab_name": { "type": "string" } }), &["tab_name"]),
        ),
        tool(
            "closeAllDiffTabs",
            "Close every diff tab that this server opened.",
            object_schema(json!({}), &[]),
        ),
        tool(
            "openDiff",
            "Open a diff of the proposed contents against a file and wait for the user to \
             accept or reject it. A rejection may carry the user's reason as a second text \
             content block.",
            object_schema(
                json!({
                    "old_file_path": { "type": "string" },
                    "new_file_path": { "type": "string" },
                    "new_file_contents": { "type": "string" },
                    "tab_name": { "type": "string" }
                }),
                &[
                    "old_file_path",
                    "new_file_path",
                    "new_file_contents",
                    "tab_name",
                ],
            ),
        ),
        tool(
            "executeCode",
            branding::concat_const!(
                "Not supported by ",
                crate::PRODUCT_NAME,
                ". Always returns an error."
            ),
            object_schema(json!({ "code": { "type": "string" } }), &["code"]),
        ),
    ]
}

/// `tools/list` の結果。
///
/// 内蔵ブラウザーのツール (ADR-F89) は `browser.ai_control` が入っているときだけ載せる。一覧を
/// 返した後で切り替わっても、こちらから知らせる手段は無い: `listChanged` は出しておらず、
/// アシストの口は 1 要求ごとの HTTP で通知を流す道を持たない (WebSocket の CLI はモデルに
/// `mcp__ide__*` を見せない)。クライアントは繋ぎ直すまで古い一覧のままなので、呼び出しのたびに
/// [`call_tool`] が設定を見直して断る。
pub fn list_tools_result(scope: &ConnectionScope, cx: &App) -> Value {
    let mut tools = tools()
        .into_iter()
        .filter(|tool| scope.tools.allows_tool(&tool.name))
        .collect::<Vec<_>>();
    if scope.assist.is_some() {
        tools.extend(crate::assist_tools::tools());
    }
    if offers_browser_tools(scope) && enabled_browser_control(cx).is_some() {
        tools.extend(
            browser_control::tool_definitions()
                .into_iter()
                .map(|definition| {
                    tool(
                        definition.name,
                        definition.description,
                        definition.input_schema,
                    )
                }),
        );
    }
    json!({ "tools": tools })
}

/// 内蔵ブラウザーのツール (ADR-F89) を出す口: ホストの全ツールの口と、サンドボックスの外の
/// アシストの口。サンドボックスのセッションには、アシストの口も含めて出さない。ブラウザーは
/// ホストのネットワークの許可リストとこのマシンの `localhost` に届くので、サンドボックスの
/// 外へ出る抜け道になる。
pub(crate) fn offers_browser_tools(scope: &ConnectionScope) -> bool {
    // 口が増えたら、ここで出すかどうかを決めさせる (`matches!` だと黙って出さない側に倒れる)。
    match &scope.tools {
        ToolScope::Browser(session) => session.is_live(),
        ToolScope::Full | ToolScope::Assist(None) => !scope.browser_separate,
        ToolScope::Sandboxed(_) | ToolScope::SandboxedReadOnly(_) | ToolScope::Assist(Some(_)) => {
            false
        }
    }
}

/// ADR-F102: whether the AI browser's tools are on (`browser.ai_control`), so
/// that an external agent's session is given the `zed-browser` server.
pub fn browser_tools_enabled(cx: &App) -> bool {
    enabled_browser_control(cx).is_some()
}

fn enabled_browser_control(cx: &App) -> Option<Arc<dyn BrowserControl>> {
    browser_control::browser_control(cx).filter(|control| control.is_enabled(cx))
}

/// ブラウザーのツールの結果 1 件の上限。出すのはホストの全ツールの口とアシストの口だけで、
/// 小さいほうのアシストの口の送信の上限に、JSON-RPC の包みの分の余白を残して合わせる。
/// 超える結果は壊れたメッセージにせず、エラーで返す。
pub(crate) const MAX_BROWSER_RESULT_BYTES: usize =
    crate::assist_tools::ASSIST_MESSAGE_LIMITS.outgoing - 64 * 1024;

pub(crate) fn text_content(text: impl Into<String>) -> Value {
    json!({ "type": "text", "text": text.into() })
}

pub(crate) fn json_content(value: &Value) -> Result<Value> {
    Ok(text_content(
        serde_json::to_string(value).context("serializing a tool result")?,
    ))
}

pub fn parse_call_params(params: Value) -> Result<CallToolParams> {
    serde_json::from_value(params).context("parsing the tools/call parameters")
}

/// ツールを 1 つ実行する。ツール自体の失敗は `isError` 付きの結果として返し、この
/// セッションでは出していないツールなら `None` (呼び出し側が JSON-RPC のエラーで答える)。
pub(crate) async fn call_tool<H: ToolHost>(
    server: &Entity<H>,
    params: CallToolParams,
    connection: ConnectionId,
    scope: &ConnectionScope,
    cx: &mut AsyncApp,
) -> Option<Value> {
    let workspace = server.read_with(cx, |server, _| server.workspace());
    let is_browser_tool = browser_control::TOOL_NAMES.contains(&params.name.as_str());
    let offered = if is_browser_tool {
        offers_browser_tools(scope)
    } else {
        scope.allows_tool(&params.name)
    };
    if !offered {
        return None;
    }
    let arguments = params.arguments.unwrap_or(Value::Null);
    let result = match (workspace, scope.assist.as_ref()) {
        (Some(workspace), _) if is_browser_tool => {
            run_browser_tool(&params.name, arguments, &workspace, cx).await
        }
        (Some(workspace), Some(source)) if scope.is_assist_tool(&params.name) => {
            crate::assist_tools::run_assist_tool(
                &params.name,
                arguments,
                &workspace,
                source,
                &scope.tools,
                cx,
            )
            .await
        }
        (Some(workspace), _) => {
            run_tool(
                &params.name,
                arguments,
                server,
                &workspace,
                connection,
                &scope.tools,
                cx,
            )
            .await
        }
        (None, _) => Err(anyhow!(
            "no {} window has this workspace open",
            crate::PRODUCT_NAME
        )),
    };
    Some(match result {
        Ok(content) => json!({ "content": content, "isError": false }),
        Err(error) => json!({
            "content": [text_content(error_text(&params.name, &error, &scope.tools))],
            "isError": true,
        }),
    })
}

async fn run_tool<H: ToolHost>(
    name: &str,
    arguments: Value,
    server: &Entity<H>,
    workspace: &WeakEntity<Workspace>,
    connection: ConnectionId,
    scope: &ToolScope,
    cx: &mut AsyncApp,
) -> Result<Vec<Value>> {
    match name {
        "getWorkspaceFolders" => {
            let folders =
                workspace.read_with(cx, |workspace, cx| workspace_folders(workspace, cx))?;
            Ok(vec![json_content(&folders)?])
        }
        "getOpenEditors" => {
            let tabs = workspace.read_with(cx, |workspace, cx| open_editors(workspace, cx))?;
            Ok(vec![json_content(&tabs)?])
        }
        "getCurrentSelection" => {
            let snapshot = workspace.update(cx, |workspace, cx| {
                let editor = workspace
                    .active_item(cx)
                    .and_then(|item| item.act_as::<Editor>(cx))?;
                selection_snapshot(&editor, cx)
            })?;
            Ok(vec![json_content(&match snapshot {
                Some(snapshot) => snapshot.to_tool_result(),
                None => json!({ "success": false, "message": "No active editor found" }),
            })?])
        }
        "getLatestSelection" => {
            let snapshot = server.read_with(cx, |server, _| server.latest_selection().cloned());
            Ok(vec![json_content(&match snapshot {
                Some(snapshot) => snapshot.to_tool_result(),
                None => json!({ "success": false, "message": "No selection available" }),
            })?])
        }
        "getDiagnostics" => {
            let uri = arguments
                .get("uri")
                .and_then(Value::as_str)
                .map(str::to_string);
            let cursor = diagnostics_cursor(&arguments)?;
            // `cursor` continues the listing of every file and `uri` asks for one, so taking
            // either one quietly would answer a question the client did not ask.
            if uri.is_some() && !matches!(arguments.get("cursor"), None | Some(Value::Null)) {
                return Err(client_error(
                    "Pass either uri or cursor, not both: cursor continues the listing of \
                     every file, and uri asks for one file",
                ));
            }
            let page = collect_diagnostics(workspace, uri.as_deref(), cursor, scope, cx).await?;
            // CLI は最初のテキストだけを診断の配列として読むので、続きの案内は 2 つ目に置く。
            let mut content = vec![json_content(&page.files)?];
            if let Some(next_cursor) = page.next_cursor {
                content.push(json_content(&json!({
                    "nextCursor": next_cursor.to_string(),
                    "message": "More files have diagnostics. Call getDiagnostics again with this cursor to get the next page.",
                }))?);
            }
            Ok(content)
        }
        "getLastTaskOutput" => {
            let shares_task_output =
                cx.update(|cx| crate::IdeServerSettings::get_global(cx).share_task_output);
            if !shares_task_output {
                return Err(client_error(branding::concat_const!(
                    "Sharing task output is turned off in ",
                    crate::PRODUCT_NAME,
                    " (ide_server.share_task_output)"
                )));
            }
            let failure = server.read_with(cx, |server, _| server.last_task_failure().cloned());
            Ok(vec![json_content(&match failure {
                Some(failure) => failure.to_tool_result(),
                None => json!({
                    "success": false,
                    "message": format!(
                        "No task has failed in this workspace since {} started",
                        crate::PRODUCT_NAME
                    ),
                }),
            })?])
        }
        "openFile" => {
            let result = open_file(&arguments, workspace, cx).await?;
            Ok(vec![json_content(&result)?])
        }
        "checkDocumentDirty" => {
            let path = required_path(&arguments, "filePath")?;
            let result = workspace.read_with(cx, |workspace, cx| {
                match buffer_for_path(workspace, &path, cx) {
                    Some(buffer) => json!({
                        "success": true,
                        "filePath": path.to_string_lossy(),
                        "isDirty": buffer.read(cx).is_dirty(),
                        "isUntitled": buffer.read(cx).file().is_none(),
                    }),
                    None => json!({
                        "success": false,
                        "message": format!("Document not open: {}", path.display()),
                    }),
                }
            })?;
            Ok(vec![json_content(&result)?])
        }
        "saveDocument" => {
            let path = required_path(&arguments, "filePath")?;
            let result = save_document(workspace, &path, cx).await?;
            Ok(vec![json_content(&result)?])
        }
        "close_tab" => {
            let tab_name = arguments
                .get("tab_name")
                .and_then(Value::as_str)
                .ok_or_else(|| client_error("close_tab requires a tab_name"))?
                .to_string();
            close_tab(server, workspace, &tab_name, connection, scope, cx)?;
            Ok(vec![text_content("TAB_CLOSED")])
        }
        "closeAllDiffTabs" => {
            let closed = close_all_diff_tabs(server, cx);
            Ok(vec![text_content(format!("CLOSED_{closed}_DIFF_TABS"))])
        }
        "openDiff" => open_diff(&arguments, server, workspace, connection, scope, cx).await,
        "executeCode" => Err(anyhow!(
            "{} does not implement executeCode; there is no Jupyter kernel attached to this IDE server",
            crate::PRODUCT_NAME
        )),
        other => Err(anyhow!("unknown tool {other}")),
    }
}

/// 内蔵ブラウザーのツール (ADR-F89)。一覧に出した後で設定が切られていることがあるので、
/// 呼ばれるたびに `browser.ai_control` を見直す。
async fn run_browser_tool(
    name: &str,
    arguments: Value,
    workspace: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
) -> Result<Vec<Value>> {
    let control = cx
        .update(|cx| enabled_browser_control(cx))
        .ok_or_else(|| anyhow!(browser_control::AI_CONTROL_OFF_MESSAGE))?;
    let command = browser_control::parse_command(name, arguments)?;
    let project = workspace.read_with(cx, |workspace, _| workspace.project().clone())?;
    let output = cx.update(|cx| control.run(&project, command, cx)).await?;
    browser_output_content(name, output)
}

pub(crate) fn browser_output_content(name: &str, output: BrowserOutput) -> Result<Vec<Value>> {
    let mut content = vec![text_content(output.text)];
    if let Some(png) = output.image_png {
        content.push(json!({
            "type": "image",
            "data": base64::engine::general_purpose::STANDARD.encode(&png),
            "mimeType": "image/png",
        }));
    }
    let size = serde_json::to_vec(&content)
        .context("serializing a browser tool result")?
        .len();
    if size > MAX_BROWSER_RESULT_BYTES {
        return Err(anyhow!(
            "the {name} result is {size} bytes, more than the {MAX_BROWSER_RESULT_BYTES} bytes \
             one message can carry; ask for less (a smaller max_chars or limit)"
        ));
    }
    Ok(content)
}

fn required_path(arguments: &Value, key: &str) -> Result<PathBuf> {
    Ok(path_from_uri(required_string(arguments, key)?))
}

fn required_string<'a>(arguments: &'a Value, key: &str) -> Result<&'a str> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| client_error(format!("the tool requires {key}")))
}

/// サンドボックスの要求 1 件ぶんの文脈。ワークツリーのルートの実体は要求ごとに 1 度だけ求める。
pub(crate) struct SandboxRequest<'a> {
    paths: &'a SandboxPaths,
    project: Entity<Project>,
    checks: SandboxChecks,
}

/// ADR-F53: サンドボックスが指したパスの実体を、どこで確かめるか。
enum SandboxChecks {
    /// ローカルのプロジェクト。ファイルはこのマシンにあるので [`Fs`] で直接見る。
    OnThisMachine {
        fs: Arc<dyn Fs>,
        executor: gpui::BackgroundExecutor,
        real_roots: RealRoots,
    },
    /// リモート (SSH) のプロジェクト。ファイルを持っているのは接続先なので、「ルート直下
    /// から 1 段もリンクを通らずに開ける」の判定も接続先にさせる
    /// ([`Project::verify_agent_read`] = proto 506)。クライアントの [`Fs`] で見ると、接続先の
    /// 絶対パスはこのマシンには無いので、要求が残らず「ワークスペースの外」になってしまう。
    OnTheRemoteHost,
}

/// どちらの確認を使うか。ファイルを持っているのがこのマシンでないプロジェクト
/// (SSH リモート) は、すべて接続先に確かめさせる。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SandboxChecksKind {
    OnThisMachine,
    OnTheRemoteHost,
}

impl SandboxChecksKind {
    /// collab のプロジェクトはここまで来ない: セッションを結び付ける時点で断っている
    /// ([`crate::sandbox_client`] の `session_project_refusal`)。
    fn of(project: &Project) -> Self {
        if project.is_local() {
            Self::OnThisMachine
        } else {
            Self::OnTheRemoteHost
        }
    }
}

/// [`SandboxRequest::verify_on_host`] の結果。
enum RemoteEntry {
    /// 接続先に実体があり、リンクを 1 段も通らずに開ける。
    Present(ProjectPath),
    /// ワークツリーの中だが、接続先にはまだ無い (新規の `openDiff`)。
    Absent,
}

/// ワークツリーの走査から見た、リモートのパスの様子。
enum RemotePathState {
    File,
    Directory,
    /// 走査がこのパスを知らない。まだ無いか、走査が届いていない (除外パターン・`.git` 配下・
    /// 初回走査の途中) かの区別は走査からは付かないので、接続先に確かめさせる。
    /// `plain_ancestors` は、走査が知っている祖先がすべて素のディレクトリだったか。
    Unscanned {
        plain_ancestors: bool,
    },
}

impl<'a> SandboxRequest<'a> {
    pub(crate) async fn new(
        paths: &'a SandboxPaths,
        project: &Entity<Project>,
        cx: &mut AsyncApp,
    ) -> Self {
        let kind = project.read_with(cx, |project, _| SandboxChecksKind::of(project));
        Self::with_kind(paths, project, kind, cx).await
    }

    /// [`Self::new`] の中身。テストは、ローカルの [`fs::FakeFs`] のプロジェクトに
    /// [`SandboxChecksKind::OnTheRemoteHost`] を指定して、接続先で確かめる経路を通す
    /// (`verify_agent_read` はローカルのワークツリーでは `Fs::verify_read` に降りるので、
    /// 判定そのものは本物)。
    pub(crate) async fn with_kind(
        paths: &'a SandboxPaths,
        project: &Entity<Project>,
        kind: SandboxChecksKind,
        cx: &mut AsyncApp,
    ) -> Self {
        let checks = match kind {
            SandboxChecksKind::OnThisMachine => {
                let fs = project.read_with(cx, |project, _| project.fs().clone());
                SandboxChecks::OnThisMachine {
                    real_roots: path_mapping::real_roots(paths, &fs).await,
                    fs,
                    executor: cx.background_executor().clone(),
                }
            }
            SandboxChecksKind::OnTheRemoteHost => SandboxChecks::OnTheRemoteHost,
        };
        Self {
            paths,
            project: project.clone(),
            checks,
        }
    }

    /// テストが「どちらの確認を選んだか」を確かめるためだけのもの。実装はこの区別を
    /// [`Self::checks`] のパターンマッチで直接扱う。
    #[cfg(test)]
    pub(crate) fn checks_kind(&self) -> SandboxChecksKind {
        match self.checks {
            SandboxChecks::OnThisMachine { .. } => SandboxChecksKind::OnThisMachine,
            SandboxChecks::OnTheRemoteHost => SandboxChecksKind::OnTheRemoteHost,
        }
    }

    /// CLI から来たパスを変換し、ワークツリーの外を指すもの (中に置かれたリンクの行き先を
    /// 含む) と、リンクを通るものを拒否する。
    pub(crate) async fn resolve(&self, text: &str, cx: &mut AsyncApp) -> Result<PathBuf> {
        let host_path = self
            .paths
            .to_host(text)
            .map_err(|error| client_error(error.to_string()))?;
        match &self.checks {
            SandboxChecks::OnThisMachine {
                fs,
                executor,
                real_roots,
            } => {
                match path_mapping::has_link_below_root(self.paths, &host_path, fs, executor).await
                {
                    Ok(false) => {}
                    Ok(true) => return Err(link_refused_error(text)),
                    Err(error) => {
                        log::debug!(
                            "ide_server: could not inspect a path for a sandboxed client: {error:#}"
                        );
                        return Err(client_error(format!("{text} could not be inspected")));
                    }
                }
                if let Err(error) = real_roots.ensure_contains(&host_path, fs).await {
                    log::debug!("ide_server: refused a path for a sandboxed client: {error:#}");
                    return Err(client_error(format!("{text} is outside the workspace")));
                }
            }
            SandboxChecks::OnTheRemoteHost => {
                self.verify_on_host(&host_path, text, cx).await?;
            }
        }
        Ok(host_path)
    }

    /// 差分の旧内容を 1 度だけ読む。まだ無いファイルなら空。
    pub(crate) async fn read_old_contents(
        &self,
        host_path: &Path,
        text: &str,
        cx: &mut AsyncApp,
    ) -> Result<String> {
        let (fs, real_roots) = match &self.checks {
            SandboxChecks::OnThisMachine { fs, real_roots, .. } => (fs, real_roots),
            // ADR-F53 決定: 接続先のファイルはクライアントからは開けないので、確認を通した
            // うえでバッファーとして読む (ホストの経路と同じ手で、未保存の変更も見える)。
            SandboxChecks::OnTheRemoteHost => {
                let project_path = match self.verify_on_host(host_path, text, cx).await? {
                    RemoteEntry::Absent => return Ok(String::new()),
                    RemoteEntry::Present(project_path) => project_path,
                };
                // 開く**前**に断る: `Project::open_buffer` は接続先のファイルを丸ごと読んで
                // Rope にするので (ワークツリーの上限は 6 GiB)、読んでから大きさを見るのでは
                // サンドボックスの中の CLI がメモリを使い切らせられる。走査が大きさを知らない
                // パスは、読んだあとの控えの確認に任せる。
                let scanned_size = self.project.read_with(cx, |project, cx| {
                    let worktree = project.worktree_for_id(project_path.worktree_id, cx)?;
                    let entry = worktree.read(cx).entry_for_path(&project_path.path)?;
                    Some(entry.size)
                });
                if scanned_size.is_some_and(|size| size > MAX_SANDBOX_FILE_BYTES) {
                    return Err(client_error(format!(
                        "{text} is larger than {} MiB",
                        MAX_SANDBOX_FILE_BYTES / (1024 * 1024)
                    )));
                }
                let buffer = self
                    .project
                    .update(cx, |project, cx| project.open_buffer(project_path, cx))
                    .await
                    .map_err(|error| {
                        log::debug!(
                            "ide_server: could not open a remote file for a sandboxed client: \
                             {error:#}"
                        );
                        client_error(format!("{text} could not be read"))
                    })?;
                let contents = buffer.read_with(cx, |buffer, _| buffer.text());
                if u64::try_from(contents.len()).unwrap_or(u64::MAX) > MAX_SANDBOX_FILE_BYTES {
                    return Err(client_error(format!(
                        "{text} is larger than {} MiB",
                        MAX_SANDBOX_FILE_BYTES / (1024 * 1024)
                    )));
                }
                return Ok(contents);
            }
        };
        let executor = cx.background_executor().clone();
        match real_roots.read_file(host_path, fs, &executor).await {
            Ok(SandboxFile::Missing) => Ok(String::new()),
            Ok(SandboxFile::Text(contents)) => Ok(contents),
            Ok(SandboxFile::NotRegular) => {
                Err(client_error(format!("{text} is not a regular file")))
            }
            Ok(SandboxFile::TooLarge) => Err(client_error(format!(
                "{text} is larger than {} MiB",
                MAX_SANDBOX_FILE_BYTES / (1024 * 1024)
            ))),
            Ok(SandboxFile::Redirected) => Err(link_refused_error(text)),
            Err(error) => {
                log::debug!("ide_server: could not read a file for a sandboxed client: {error:#}");
                Err(client_error(format!("{text} could not be read")))
            }
        }
    }

    /// すでに開いているバッファーを CLI に見せてよいかの確認。ワークツリーの走査がリンクを
    /// たどった結果として外を指すと判断したファイルや、リンクを通るファイルは断る。通れば、
    /// そのファイルのホストの絶対パスを返す。
    pub(crate) async fn verify_open_buffer(
        &self,
        buffer: &Entity<Buffer>,
        cx: &mut AsyncApp,
    ) -> Result<PathBuf> {
        let (project_path, abs_path, is_external, is_private) = buffer
            .read_with(cx, |buffer, cx| {
                let file = WorktreeFile::from_dyn(buffer.file())?;
                let worktree = file.worktree.read(cx);
                let entry = file
                    .entry_id
                    .and_then(|entry_id| worktree.entry_for_id(entry_id));
                let is_external = entry.is_some_and(|entry| entry.is_external);
                let project_path = ProjectPath {
                    worktree_id: worktree.id(),
                    path: file.path.clone(),
                };
                let is_private = is_private_file(worktree, &project_path, entry, cx);
                Some((
                    project_path,
                    worktree.absolutize(&file.path),
                    is_external,
                    is_private,
                ))
            })
            .context("the buffer is not a file in a worktree")?;
        anyhow::ensure!(!is_external, "the worktree marks the file as outside it");
        // ADR-F56: `private_files` (`.env` など) は、サンドボックスには診断も見せない。
        anyhow::ensure!(!is_private, "the file matches the private_files setting");
        anyhow::ensure!(
            self.paths.contains(&abs_path),
            "the file is outside the workspace"
        );
        match &self.checks {
            SandboxChecks::OnThisMachine {
                fs,
                executor,
                real_roots,
            } => {
                anyhow::ensure!(
                    !path_mapping::has_link_below_root(self.paths, &abs_path, fs, executor).await?,
                    "the file goes through a link"
                );
                real_roots.ensure_contains(&abs_path, fs).await?;
            }
            SandboxChecks::OnTheRemoteHost => {
                self.verify_agent_read(project_path, cx).await?;
            }
        }
        Ok(abs_path)
    }

    /// ADR-F53: 接続先のパスの確認。ワークツリーの中にあることを確かめ、実体があるものは
    /// 「ルート直下から 1 段もリンクを通らずに開ける」ことを接続先に確かめさせる。
    ///
    /// 走査に出ないパスでも、まず接続先に開かせてみる: `.git` 配下や `file_scan_exclusions`
    /// に当たるもの・初回走査の途中のものは、実体があっても走査には出ない。走査だけで
    /// 「まだ無い」と決めると、既存のファイルが差分の旧側が空の「新規ファイル」として人の
    /// 確認を通ってしまうので、接続先が開けるものは (大きさを確かめられない以上) 断る。
    ///
    /// 接続先も開けなかったときだけ、まだ無いもの (新規の `openDiff`) として扱ってよいかを
    /// 走査が知っている祖先で判断する —— 一番近い既存の祖先が素のディレクトリであること
    /// (リンクでも、リンクの先でワークツリーの外を指すものでもないこと)。そこまで通れば
    /// [`RemoteEntry::Absent`] で、差分の旧側が空になる。Zed 自身はこのパスを読みも書きも
    /// しない (採用された本文を書くのはサンドボックスの中の CLI)。
    async fn verify_on_host(
        &self,
        host_path: &Path,
        text: &str,
        cx: &mut AsyncApp,
    ) -> Result<RemoteEntry> {
        let found =
            self.project.read_with(cx, |project, cx| {
                let project_path = project.find_project_path(host_path, cx)?;
                let worktree = project.worktree_for_id(project_path.worktree_id, cx)?;
                let worktree = worktree.read(cx);
                // 接続先が辿る基点はセッションのルートでなければならない。`find_project_path` は
                // 開いているワークツリーのうち綴りが前方一致する最初のものを返すので、セッションの
                // 開始後にルートの中へ入れ子で開かれたフォルダーが返ると、そのフォルダーまでの
                // リンクが確かめられないまま通ってしまう (ローカルの経路はセッションのルートから
                // 辿るので同じ穴が無い)。
                if !self.paths.roots().iter().any(|root| {
                    util::path_mapping::same_place(self.paths, root, &worktree.abs_path())
                }) {
                    return None;
                }
                // `canonical_path` が付いた項目はリンクを通って辿り着いたもので、`is_external`
                // はその先がワークツリーの外を指すもの。接続先に開かせられないパス (本当に無い
                // もの) は、走査が知っている祖先がすべて素のディレクトリであることが唯一の確認。
                let plain_ancestors = project_path.path.ancestors().skip(1).all(|ancestor| {
                    worktree.entry_for_path(ancestor).is_none_or(|entry| {
                        entry.is_dir() && !entry.is_external && entry.canonical_path.is_none()
                    })
                });
                let state = match worktree.entry_for_path(&project_path.path) {
                    Some(entry) if entry.is_dir() => RemotePathState::Directory,
                    Some(_) => RemotePathState::File,
                    None => RemotePathState::Unscanned { plain_ancestors },
                };
                Some((project_path, state))
            });
        let Some((project_path, state)) = found else {
            return Err(client_error(format!("{text} is not inside a worktree")));
        };
        match state {
            RemotePathState::Directory => {
                Err(client_error(format!("{text} is not a regular file")))
            }
            RemotePathState::File => {
                self.verify_agent_read(project_path.clone(), cx)
                    .await
                    .map_err(|error| {
                        log::debug!(
                            "ide_server: the host refused a path for a sandboxed client: {error:#}"
                        );
                        link_refused_error(text)
                    })?;
                Ok(RemoteEntry::Present(project_path))
            }
            // 走査に出ないからといって「まだ無い」とは限らない (`.git` 配下や
            // `file_scan_exclusions` に当たるものは実体があっても出ない)。実体の有無も接続先に
            // 決めさせる。
            RemotePathState::Unscanned { plain_ancestors } => {
                match self.verify_agent_read(project_path, cx).await {
                    // 実体はあるのに走査が知らない = 大きさも分からない。旧側を空で出すと
                    // 既にあるファイルが「新規ファイル」として人の確認を通ってしまい、かと
                    // いって大きさを確かめずに開くと接続先のファイルを丸ごと読み込むことに
                    // なる (ワークツリー自身の上限は 6 GiB)。どちらも取らず理由を出して断る。
                    Ok(()) => Err(client_error(format!(
                        "{text} is not in this session's view of the worktree, so its previous \
                         contents cannot be shown"
                    ))),
                    // 接続先にも開けない = 本当にまだ無い。走査が知っている祖先がすべて素の
                    // ディレクトリなら新規ファイルとして扱う (差分の旧側が空)。
                    Err(error) => {
                        log::debug!(
                            "ide_server: the host could not open a path the worktree does not \
                             know: {error:#}"
                        );
                        if plain_ancestors {
                            Ok(RemoteEntry::Absent)
                        } else {
                            Err(link_refused_error(text))
                        }
                    }
                }
            }
        }
    }

    /// proto 506: 接続先に「ルートから `path` までリンクを 1 段も通らずに開ける」を判定させる。
    async fn verify_agent_read(&self, path: ProjectPath, cx: &mut AsyncApp) -> Result<()> {
        self.project
            .read_with(cx, |project, cx| project.verify_agent_read(&path, cx))
            .await
    }

    pub(crate) fn paths(&self) -> &SandboxPaths {
        self.paths
    }

    /// アシストの読み取り結果 (定義・参照・シンボル) の中身を、サンドボックスに見せてよいか。
    /// 引数のパスと同じ確認 ([`Self::resolve`]) を、結果のファイルにも掛ける: ローカルでは
    /// リンクを 1 段も通らずワークツリーの中に実体があること、リモートでは接続先の
    /// `verify_agent_read` (proto 506) が通ること。
    pub(crate) async fn allows_result_contents(
        &self,
        project_path: &ProjectPath,
        abs_path: &Path,
        cx: &mut AsyncApp,
    ) -> bool {
        if !self.paths.contains(abs_path) {
            return false;
        }
        match &self.checks {
            SandboxChecks::OnThisMachine {
                fs,
                executor,
                real_roots,
            } => {
                matches!(
                    path_mapping::has_link_below_root(self.paths, abs_path, fs, executor).await,
                    Ok(false)
                ) && real_roots.ensure_contains(abs_path, fs).await.is_ok()
            }
            SandboxChecks::OnTheRemoteHost => self
                .verify_agent_read(project_path.clone(), cx)
                .await
                .is_ok(),
        }
    }
}

/// `private_files` (`.env` など) に当たるファイルか。内蔵エージェントの `read_file` と同じく、
/// 全体の設定とワークツリーの設定の両方を見る。
pub(crate) fn is_private_file(
    worktree: &Worktree,
    project_path: &ProjectPath,
    entry: Option<&project::Entry>,
    cx: &App,
) -> bool {
    entry.is_some_and(|entry| entry.is_private)
        || ide_query::worktree_matches_private_files(worktree, project_path, cx)
}

fn link_refused_error(text: &str) -> anyhow::Error {
    client_error(format!(
        "{text} is a link or goes through one, and a sandboxed session does not follow links"
    ))
}

/// ロックファイルに書くワークツリー・ルートの絶対パス。
pub fn workspace_folder_paths(workspace: &Entity<Workspace>, cx: &App) -> Vec<String> {
    workspace
        .read(cx)
        .project()
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path().to_string_lossy().into_owned())
        .collect()
}

fn workspace_folders(workspace: &Workspace, cx: &App) -> Value {
    let folders = workspace
        .project()
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| {
            let worktree = worktree.read(cx);
            let path = worktree.abs_path();
            json!({
                "name": worktree.root_name_str(),
                "uri": file_uri(&path),
                "path": path.to_string_lossy(),
            })
        })
        .collect::<Vec<_>>();
    let root_path = folders
        .first()
        .and_then(|folder| folder.get("path").cloned());
    json!({
        "success": true,
        "folders": folders,
        "rootPath": root_path,
    })
}

fn open_editors(workspace: &Workspace, cx: &App) -> Value {
    let active_item_id = workspace.active_item(cx).map(|item| item.item_id());
    let mut tabs = Vec::new();
    for pane in workspace.panes() {
        for item in pane.read(cx).items() {
            let Some(editor) = item.act_as::<Editor>(cx) else {
                continue;
            };
            let buffer = editor.read(cx).active_buffer(cx);
            let path = buffer
                .as_ref()
                .and_then(|buffer| buffer.read(cx).file())
                .map(|file| file.full_path(cx));
            let language = buffer
                .as_ref()
                .and_then(|buffer| buffer.read(cx).language().cloned())
                .map(|language| language.name().as_ref().to_string());
            tabs.push(json!({
                "uri": path.as_ref().map(|path| file_uri(path)),
                "path": path.as_ref().map(|path| path.to_string_lossy().into_owned()),
                "label": item.tab_content_text(0, cx).to_string(),
                "languageId": language,
                "isActive": Some(item.item_id()) == active_item_id,
                "isDirty": item.is_dirty(cx),
            }));
        }
    }
    json!({ "tabs": tabs })
}

/// `at_mentioned` 通知の中身。CLI は 0 始まりの行番号を `@ファイル#L1-L3` の形に直して
/// 会話に貼る。
pub fn at_mention_params(file_path: &str, start_line: u32, end_line: u32) -> Value {
    json!({
        "filePath": file_path,
        "lineStart": start_line,
        "lineEnd": end_line,
    })
}

/// 選択範囲を `at_mentioned` 通知の (絶対パス, 開始行, 終了行) に直す。行は 0 始まり。
pub fn at_mention_range(editor: &Entity<Editor>, cx: &mut App) -> Option<(PathBuf, u32, u32)> {
    let snapshot = selection_snapshot(editor, cx)?;
    let file_path = snapshot.file_path.clone()?;
    // 行全体を選ぶと選択の終端は次の行の 0 桁目に来る。CLI は行番号をそのまま
    // `@ファイル#L1-L3` に直すので、その分を戻さないと 1 行多く見える。
    let end_line = if snapshot.end_line > snapshot.start_line && snapshot.end_character == 0 {
        snapshot.end_line - 1
    } else {
        snapshot.end_line
    };
    Some((file_path, snapshot.start_line, end_line))
}

pub fn selection_snapshot(editor: &Entity<Editor>, cx: &mut App) -> Option<SelectionSnapshot> {
    editor.update(cx, |editor, cx| {
        // CLI はパスを自分の cwd と突き合わせるので、取れるなら絶対パスを渡す。
        let file_path = editor.active_buffer(cx).and_then(|buffer| {
            let buffer = buffer.read(cx);
            let file = buffer.file()?;
            Some(match file.as_local() {
                Some(local_file) => local_file.abs_path(cx),
                None => file.full_path(cx),
            })
        });
        let snapshot = editor.display_snapshot(cx);
        let selection = editor.selections.newest::<PointUtf16>(&snapshot);
        let text = snapshot
            .buffer_snapshot()
            .text_for_range(selection.start..selection.end)
            .collect::<String>();
        Some(SelectionSnapshot {
            file_path,
            text,
            start_line: selection.start.row,
            start_character: selection.start.column,
            end_line: selection.end.row,
            end_character: selection.end.column,
        })
    })
}

fn buffer_for_path(workspace: &Workspace, path: &Path, cx: &App) -> Option<Entity<Buffer>> {
    let project = workspace.project().read(cx);
    let project_path = project.project_path_for_absolute_path(path, cx)?;
    project.get_open_buffer(&project_path, cx)
}

fn project_path_for(
    workspace: &Workspace,
    path: &Path,
    cx: &App,
) -> Option<(Entity<Project>, ProjectPath)> {
    let project = workspace.project().clone();
    let project_path = project.read(cx).find_project_path(path, cx)?;
    Some((project, project_path))
}

async fn save_document(
    workspace: &WeakEntity<Workspace>,
    path: &Path,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let buffer = workspace.read_with(cx, |workspace, cx| buffer_for_path(workspace, path, cx))?;
    let Some(buffer) = buffer else {
        return Ok(json!({
            "success": false,
            "message": format!("Document not open: {}", path.display()),
        }));
    };
    let project = workspace.read_with(cx, |workspace, _| workspace.project().clone())?;
    project
        .update(cx, |project, cx| project.save_buffer(buffer, cx))
        .await
        .with_context(|| format!("saving {}", path.display()))?;
    Ok(json!({
        "success": true,
        "filePath": path.to_string_lossy(),
        "saved": true,
        "message": "Document saved successfully",
    }))
}

async fn open_file(
    arguments: &Value,
    workspace: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let path = required_path(arguments, "filePath")?;
    let preview = arguments
        .get("preview")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let focus = arguments
        .get("makeFrontmost")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let project_path =
        workspace.read_with(cx, |workspace, cx| project_path_for(workspace, &path, cx))?;
    let item = match project_path {
        Some((_, project_path)) => {
            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.open_path_preview(
                        project_path,
                        None,
                        focus,
                        preview,
                        true,
                        window,
                        cx,
                    )
                })?
                .await?
        }
        None => {
            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.open_abs_path(
                        path.clone(),
                        workspace::OpenOptions {
                            focus: Some(focus),
                            ..Default::default()
                        },
                        window,
                        cx,
                    )
                })?
                .await?
        }
    };

    let Some(editor) = item.downcast::<Editor>() else {
        return Ok(json!({
            "success": true,
            "filePath": path.to_string_lossy(),
            "message": "Opened, but the item is not a text editor",
        }));
    };

    let range = editor.update(cx, |editor, cx| selection_range(editor, arguments, cx));
    if let Some(range) = range {
        editor.downgrade().update_in(cx, |editor, window, cx| {
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::center()),
                window,
                cx,
                |selections| selections.select_ranges([range]),
            );
        })?;
    }

    Ok(json!({
        "success": true,
        "filePath": path.to_string_lossy(),
        "message": format!("Opened file in {}", crate::PRODUCT_NAME),
    }))
}

/// `startLine` / `startColumn` … と `startText` / `endText` の両方を受ける。
/// 行・列が与えられていればそちらを優先する。
fn selection_range(
    editor: &mut Editor,
    arguments: &Value,
    cx: &mut App,
) -> Option<std::ops::Range<PointUtf16>> {
    let number = |key: &str| {
        arguments
            .get(key)
            .and_then(Value::as_u64)
            .map(|value| value as u32)
    };
    let snapshot = editor.display_snapshot(cx);
    let buffer = snapshot.buffer_snapshot();

    if let Some(start_line) = number("startLine") {
        let start = PointUtf16::new(start_line, number("startColumn").unwrap_or(0));
        let end_line = number("endLine").unwrap_or(start_line);
        let end = PointUtf16::new(end_line, number("endColumn").unwrap_or(start.column));
        return Some(start..end);
    }

    let start_text = arguments.get("startText").and_then(Value::as_str)?;
    let text = buffer.text();
    let start_offset = text.find(start_text)?;
    let end_offset = match arguments.get("endText").and_then(Value::as_str) {
        Some(end_text) => text[start_offset..]
            .find(end_text)
            .map(|relative| start_offset + relative + end_text.len())
            .unwrap_or(start_offset + start_text.len()),
        None => start_offset + start_text.len(),
    };
    let start = buffer.offset_to_point_utf16(MultiBufferOffset(start_offset));
    let end = buffer.offset_to_point_utf16(MultiBufferOffset(end_offset));
    Some(start..end)
}

/// `getDiagnostics` の 1 回分。
pub(crate) struct DiagnosticsPage {
    pub files: Value,
    /// まだ続きがあるときの、次の呼び出しに渡す位置。
    pub next_cursor: Option<usize>,
}

/// `getDiagnostics` の `cursor`。前回の `nextCursor` (診断のあるファイルの並びの何番目から
/// 続けるか) をそのまま受け取る。並びは呼び出しのたびに作り直すので、あいだに診断が
/// 増減すると、ファイルが飛んだり重なったりしうる。
fn diagnostics_cursor(arguments: &Value) -> Result<usize> {
    match arguments.get("cursor") {
        None | Some(Value::Null) => Ok(0),
        Some(Value::String(text)) => text.parse::<usize>().map_err(|_| {
            client_error(format!(
                "cursor {text:?} is not a nextCursor returned by getDiagnostics"
            ))
        }),
        Some(_) => Err(client_error("cursor must be a string")),
    }
}

/// 全体の並び `items` から、`cursor` 番目以降の最大 `limit` 件と、続きの位置を返す。
pub(crate) fn page_of<T>(items: Vec<T>, cursor: usize, limit: usize) -> (Vec<T>, Option<usize>) {
    let total = items.len();
    let page = items
        .into_iter()
        .skip(cursor)
        .take(limit)
        .collect::<Vec<_>>();
    let end = cursor.saturating_add(page.len());
    (page, (end < total).then_some(end))
}

async fn collect_diagnostics(
    workspace: &WeakEntity<Workspace>,
    uri: Option<&str>,
    cursor: usize,
    scope: &ToolScope,
    cx: &mut AsyncApp,
) -> Result<DiagnosticsPage> {
    let project = workspace.read_with(cx, |workspace, _| workspace.project().clone())?;
    let (files, next_cursor) = match scope.sandbox_paths() {
        None => host_diagnostics(&project, uri, cursor, cx).await?,
        Some(paths) => sandbox_diagnostics(&project, uri, cursor, paths, cx).await?,
    };
    Ok(DiagnosticsPage {
        files: Value::Array(files),
        next_cursor,
    })
}

async fn host_diagnostics(
    project: &Entity<Project>,
    uri: Option<&str>,
    cursor: usize,
    cx: &mut AsyncApp,
) -> Result<(Vec<Value>, Option<usize>)> {
    let (project_paths, next_cursor) = match uri {
        Some(uri) => {
            let path = path_from_uri(uri);
            let project_path = project
                .read_with(cx, |project, cx| project.find_project_path(&path, cx))
                .ok_or_else(|| anyhow!("{} is not inside a worktree", path.display()))?;
            (vec![project_path], None)
        }
        None => {
            // 言語サーバーごとに 1 件ずつ並ぶので、同じファイルを 2 度返さないよう畳む。
            let all_paths = project.read_with(cx, |project, cx| {
                let mut seen = HashSet::default();
                project
                    .diagnostic_summaries(false, cx)
                    .map(|(project_path, _, _)| project_path)
                    .filter(|project_path| seen.insert(project_path.clone()))
                    .collect::<Vec<_>>()
            });
            page_of(all_paths, cursor, MAX_DIAGNOSTIC_FILES)
        }
    };

    let mut files = Vec::new();
    for project_path in project_paths {
        // CLI は自分の cwd の下の絶対パスと突き合わせるので、ワークツリー名から始まる形
        // (`full_path`) ではなく絶対パスで返す。
        let abs_path =
            project.read_with(cx, |project, cx| project.absolute_path(&project_path, cx));
        let Some(abs_path) = abs_path else {
            continue;
        };
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(project_path, cx))
            .await;
        let Ok(buffer) = buffer else {
            continue;
        };
        let diagnostics = buffer_diagnostics(&buffer, cx);
        if diagnostics.is_empty() {
            continue;
        }
        files.push(json!({ "uri": file_uri(&abs_path), "diagnostics": diagnostics }));
    }
    Ok((files, next_cursor))
}

/// サンドボックスへの診断。すでに開いているバッファーにある診断だけを返し、URI を指定されても
/// ファイルは開かない。サンドボックスの求めでファイルを開くと、サンドボックスが書いたファイル
/// でホストの言語サーバーが起きる。
async fn sandbox_diagnostics(
    project: &Entity<Project>,
    uri: Option<&str>,
    cursor: usize,
    paths: &SandboxPaths,
    cx: &mut AsyncApp,
) -> Result<(Vec<Value>, Option<usize>)> {
    let request = SandboxRequest::new(paths, project, cx).await;
    let (buffers, next_cursor) = match uri {
        Some(uri) => {
            let host_path = request.resolve(uri, cx).await?;
            let project_path = project
                .read_with(cx, |project, cx| project.find_project_path(&host_path, cx))
                .ok_or_else(|| client_error(format!("{uri} is not inside a worktree")))?;
            let buffers = project
                .read_with(cx, |project, cx| project.get_open_buffer(&project_path, cx))
                .into_iter()
                .collect::<Vec<_>>();
            (buffers, None)
        }
        None => {
            let all_buffers = project.read_with(cx, |project, cx| {
                let mut seen = HashSet::default();
                project
                    .diagnostic_summaries(false, cx)
                    .filter(|(project_path, _, _)| seen.insert(project_path.clone()))
                    .filter_map(|(project_path, _, _)| project.get_open_buffer(&project_path, cx))
                    .collect::<Vec<_>>()
            });
            page_of(all_buffers, cursor, MAX_SANDBOX_DIAGNOSTIC_FILES)
        }
    };
    let mut files = Vec::new();
    for buffer in buffers {
        match request.verify_open_buffer(&buffer, cx).await {
            Ok(abs_path) => files.extend(sandbox_diagnostics_entry(&buffer, &abs_path, paths, cx)),
            Err(error) => {
                log::debug!("ide_server: left out diagnostics for a sandboxed client: {error:#}")
            }
        }
    }
    Ok((files, next_cursor))
}

fn sandbox_diagnostics_entry(
    buffer: &Entity<Buffer>,
    abs_path: &Path,
    paths: &SandboxPaths,
    cx: &mut AsyncApp,
) -> Option<Value> {
    let client_uri = paths.to_client_uri(abs_path)?;
    let diagnostics = buffer_diagnostics(buffer, cx);
    (!diagnostics.is_empty()).then(|| json!({ "uri": client_uri, "diagnostics": diagnostics }))
}

fn buffer_diagnostics(buffer: &Entity<Buffer>, cx: &mut AsyncApp) -> Vec<Value> {
    buffer.read_with(cx, |buffer, _| {
        let snapshot = buffer.snapshot();
        snapshot
            .diagnostics_in_range::<_, PointUtf16>(text::Point::zero()..snapshot.max_point(), false)
            .map(|entry| {
                json!({
                    "message": entry.diagnostic.message.as_str(),
                    "severity": severity_name(entry.diagnostic.severity),
                    "range": {
                        "start": { "line": entry.range.start.row, "character": entry.range.start.column },
                        "end": { "line": entry.range.end.row, "character": entry.range.end.column },
                    },
                    "source": entry.diagnostic.source,
                    "code": entry.diagnostic.code.as_ref().map(number_or_string),
                })
            })
            .collect()
    })
}

fn severity_name(severity: DiagnosticSeverity) -> &'static str {
    if severity == DiagnosticSeverity::ERROR {
        "Error"
    } else if severity == DiagnosticSeverity::WARNING {
        "Warning"
    } else if severity == DiagnosticSeverity::INFORMATION {
        "Info"
    } else {
        "Hint"
    }
}

fn number_or_string(code: &lsp::NumberOrString) -> Value {
    match code {
        lsp::NumberOrString::Number(number) => json!(number),
        lsp::NumberOrString::String(text) => json!(text),
    }
}

fn close_tab<H: ToolHost>(
    server: &Entity<H>,
    workspace: &WeakEntity<Workspace>,
    tab_name: &str,
    connection: ConnectionId,
    scope: &ToolScope,
    cx: &mut AsyncApp,
) -> Result<()> {
    match server.update(cx, |server, _| {
        server.open_diffs().close_named(connection, tab_name)
    }) {
        ClosedDiff::Settled => return Ok(()),
        ClosedDiff::Pending(diff) => {
            diff.update(cx, |diff, cx| diff.dismiss(cx));
            return Ok(());
        }
        ClosedDiff::Unknown => {}
    }
    // サンドボックスからは、自分が開いた差分タブしか閉じさせない。
    if scope.sandbox_paths().is_some() {
        return Ok(());
    }

    let targets = workspace.read_with(cx, |workspace, cx| {
        workspace
            .panes()
            .iter()
            .flat_map(|pane| {
                pane.read(cx)
                    .items()
                    // 名前の揃った提案のタブは上で扱っている。決まっていない提案を名前だけで
                    // 閉じると、ユーザーが拒否したことになってしまう。
                    .filter(|item| {
                        item.tab_content_text(0, cx).as_ref() == tab_name
                            && item.downcast::<IdeServerDiff>().is_none()
                    })
                    .map(|item| (pane.clone(), item.item_id()))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
    })?;
    for (pane, item_id) in targets {
        pane.downgrade()
            .update_in(cx, |pane, window, cx| {
                pane.close_item_by_id(item_id, workspace::SaveIntent::Close, window, cx)
                    .detach();
            })
            .ok();
    }
    Ok(())
}

fn close_all_diff_tabs<H: ToolHost>(server: &Entity<H>, cx: &mut AsyncApp) -> usize {
    let diffs = server.update(cx, |server, _| server.open_diffs().take_all());
    let count = diffs.len();
    for diff in diffs {
        diff.update(cx, |diff, cx| diff.dismiss(cx));
    }
    count
}

async fn open_diff<H: ToolHost>(
    arguments: &Value,
    server: &Entity<H>,
    workspace: &WeakEntity<Workspace>,
    connection: ConnectionId,
    scope: &ToolScope,
    cx: &mut AsyncApp,
) -> Result<Vec<Value>> {
    let project = workspace.read_with(cx, |workspace, _| workspace.project().clone())?;
    let old_text = required_string(arguments, "old_file_path")?;
    let new_text = required_string(arguments, "new_file_path")?;
    let new_file_contents = arguments
        .get("new_file_contents")
        .and_then(Value::as_str)
        .ok_or_else(|| client_error("openDiff requires new_file_contents"))?
        .to_string();
    let tab_name = arguments
        .get("tab_name")
        .and_then(Value::as_str)
        .unwrap_or(branding::concat_const!(crate::PRODUCT_NAME, " diff"))
        .to_string();

    // 上限のあるセッションでは、パスを調べて旧内容を読む前に枠を押さえる。枠は登録した差分が
    // 引き継ぐ。同時に来た `openDiff` が、みな読み終えてから上限で断られることのないように。
    let reservation = server.update(cx, |server, _| {
        server
            .max_open_diffs()
            .map(|limit| server.open_diffs().reserve(limit))
            .transpose()
    })?;

    let (old_buffer, new_buffer, new_file_path) = match scope.sandbox_paths() {
        None => {
            let old_file_path = path_from_uri(old_text);
            let (old_buffer, new_buffer) =
                project_diff_buffers(&project, &old_file_path, new_file_contents, cx).await?;
            (old_buffer, new_buffer, path_from_uri(new_text))
        }
        Some(paths) => {
            let request = SandboxRequest::new(paths, &project, cx).await;
            let old_file_path = request.resolve(old_text, cx).await?;
            let new_file_path = request.resolve(new_text, cx).await?;
            let old_contents = request
                .read_old_contents(&old_file_path, old_text, cx)
                .await?;
            let (old_buffer, new_buffer) = standalone_diff_buffers(
                &project,
                &old_file_path,
                old_contents,
                new_file_contents,
                cx,
            )
            .await;
            (old_buffer, new_buffer, new_file_path)
        }
    };

    // 同じタブ名の提案を置き換えるのはサンドボックスのセッションだけ。ホストでは CLI が
    // ファイル名だけでタブを名付けるので、別のファイルの提案が同じ名前で並びうる。
    let sandboxed = scope.sandbox_paths().is_some();
    let diff_id = DiffId::next();
    let register = {
        let server = server.downgrade();
        let tab_name = tab_name.clone();
        move |diff: &Entity<IdeServerDiff>, cx: &mut App| -> Result<()> {
            let registered = server.update(cx, |server, cx| {
                let limit = server.max_open_diffs();
                let new_diff = OpenDiff {
                    id: diff_id,
                    connection,
                    tab_name,
                    diff: diff.clone(),
                };
                server.open_diffs().register(new_diff, sandboxed, limit, cx)
            })?;
            // 登録した差分が上限の枠を引き継いだので、押さえておいた分を返す。
            drop(reservation);
            registered
        }
    };
    let proposal = ProposedDiff {
        tab_name,
        old_buffer,
        new_buffer,
        new_file_path,
        // サンドボックスの提案でキーボードのフォーカスを奪うと、ターミナルに打つつもりの
        // キーが提案のバッファーに入ってしまう。
        take_focus: !sandboxed,
    };
    let outcome = IdeServerDiff::open(workspace.clone(), proposal, register, cx).await?;
    let outcome = outcome
        .await
        .unwrap_or(IdeServerDiffOutcome::Rejected { reason: None });
    server.update(cx, |server, _| server.open_diffs().finish(diff_id));

    // `FILE_SAVED` の 2 番目のテキストが、CLI が「承認された本文」として読む唯一の
    // 出力。CLI はこれと差分を開く前に自分で読んだ本文から編集を組み直し、保留中の
    // Edit / Write ツールに書かせる。詳細は [`crate::diff_view`] を参照。
    Ok(match outcome {
        IdeServerDiffOutcome::Accepted { contents } => {
            vec![text_content("FILE_SAVED"), text_content(contents)]
        }
        IdeServerDiffOutcome::Rejected { reason } => rejected_diff_content(reason.as_deref()),
    })
}

/// 拒否の応答。CLI は 1 つ目のテキストが `DIFF_REJECTED` かどうかだけを見る (2.1.282 で
/// 確認) ので、ユーザーの理由は 2 つ目のテキストに足す。
pub(crate) fn rejected_diff_content(reason: Option<&str>) -> Vec<Value> {
    let mut content = vec![text_content("DIFF_REJECTED")];
    if let Some(reason) = reason.map(str::trim).filter(|reason| !reason.is_empty()) {
        content.push(text_content(format!(
            "The user rejected this change. Their reason: {reason}"
        )));
    }
    content
}

/// このワークスペースで最後に失敗したタスク (`getLastTaskOutput`)。
#[derive(Clone, Debug, PartialEq)]
pub struct TaskFailure {
    pub task_id: task::TaskId,
    pub label: String,
    pub command: String,
    pub exit_code: Option<i32>,
    /// ターミナルの末尾 ([`MAX_TASK_OUTPUT_BYTES`] まで)。ターミナルが先に閉じられて
    /// 読めなかったら `None`。
    pub output: Option<String>,
    pub truncated: bool,
}

impl TaskFailure {
    pub fn new(
        task_id: task::TaskId,
        label: String,
        command: String,
        exit_code: Option<i32>,
        terminal_content: Option<&str>,
    ) -> Self {
        let (output, truncated) = match terminal_content {
            Some(content) => {
                let (output, truncated) = task_output_tail(content, MAX_TASK_OUTPUT_BYTES);
                (Some(output), truncated)
            }
            None => (None, false),
        };
        Self {
            task_id,
            label,
            command,
            exit_code,
            output,
            truncated,
        }
    }

    fn to_tool_result(&self) -> Value {
        let mut result = json!({
            "success": true,
            "label": self.label,
            "command": self.command,
            "exitCode": self.exit_code,
        });
        match &self.output {
            Some(output) => {
                result["output"] = json!(output);
                result["truncated"] = json!(self.truncated);
            }
            None => {
                result["message"] =
                    json!("The task's terminal was closed before its output could be read");
            }
        }
        result
    }
}

/// ターミナルの内容の末尾 `max_bytes` バイト (文字の途中では切らない)。行末の空白と
/// 前後の空行は落とす (グリッドの空きセルが空白で埋まっているため)。
pub(crate) fn task_output_tail(content: &str, max_bytes: usize) -> (String, bool) {
    let text = content
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim_matches('\n');
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    (text.get(start..).unwrap_or_default().to_string(), true)
}

/// ホストの提案: 旧内容は実ファイルのバッファー (未保存の変更も見える)、新内容は作業用の
/// バッファー。
async fn project_diff_buffers(
    project: &Entity<Project>,
    old_file_path: &Path,
    new_contents: String,
    cx: &mut AsyncApp,
) -> Result<(Entity<Buffer>, Entity<Buffer>)> {
    let old_buffer = diff_view::open_buffer(project, old_file_path, cx)
        .await
        .with_context(|| format!("opening {} for the diff", old_file_path.display()))?;
    let language = old_buffer.read_with(cx, |buffer, _| buffer.language().cloned());
    // `create_local_buffer` はリモートプロジェクトで panic するので、ローカル・リモート双方で
    // 動く `create_buffer` に本文を流し込む。
    let new_buffer = project
        .update(cx, |project, cx| project.create_buffer(language, false, cx))
        .await
        .context("creating the scratch buffer for the proposed contents")?;
    new_buffer.update(cx, |buffer, cx| buffer.set_text(new_contents, cx));
    Ok((old_buffer, new_buffer))
}

/// サンドボックスの提案は、旧内容も新内容もプロジェクトに属さない独立したバッファーで見せる。
/// プロジェクトのバッファーにすると、サンドボックスが書いたファイルでホストの言語サーバーが
/// 起きる。言語はパスから決めて、色付けだけに使う。
async fn standalone_diff_buffers(
    project: &Entity<Project>,
    old_file_path: &Path,
    old_contents: String,
    new_contents: String,
    cx: &mut AsyncApp,
) -> (Entity<Buffer>, Entity<Buffer>) {
    let languages = project.read_with(cx, |project, _| project.languages().clone());
    let language = languages
        .load_language_for_file_path(old_file_path)
        .await
        .ok();
    let old_buffer = cx.new(|cx| {
        let mut buffer = Buffer::local(old_contents, cx);
        buffer.set_language(language.clone(), cx);
        buffer
    });
    let new_buffer = cx.new(|cx| {
        let mut buffer = Buffer::local(new_contents, cx);
        buffer.set_language(language, cx);
        buffer
    });
    (old_buffer, new_buffer)
}
