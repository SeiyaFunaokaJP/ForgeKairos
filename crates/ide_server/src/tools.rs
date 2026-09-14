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
//! サンドボックスのセッション (ADR-F49) には CLI が呼ぶ 4 本だけを出す ([`ToolScope`])。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use anyhow::{Context as _, Result, anyhow};
use collections::HashSet;
use context_server::types::{
    CallToolParams, Implementation, LATEST_PROTOCOL_VERSION, ProtocolVersion, ServerCapabilities,
    Tool, ToolsCapabilities, VERSION_2024_11_05, VERSION_2025_03_26, VERSION_2025_06_18,
};
use editor::{Editor, MultiBufferOffset, SelectionEffects, scroll::Autoscroll};
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, WeakEntity};
use language::{Buffer, DiagnosticSeverity, PointUtf16};
use project::{File as WorktreeFile, Fs, Project, ProjectPath};
use serde_json::{Value, json};
use smol::lock::Semaphore;
use workspace::Workspace;

use crate::diff_view::{self, ProposedDiff};
use util::path_mapping::SandboxPaths;

use crate::path_mapping::{self, MAX_SANDBOX_FILE_BYTES, RealRoots, SandboxFile};
use crate::transport::{HOST_MAX_MESSAGE_BYTES, Outbox};
use crate::{IdeServerDiff, IdeServerDiffOutcome};

/// 全体診断で開くバッファーの上限。壊れたワークツリーで固まらないための保険。
const MAX_DIAGNOSTIC_FILES: usize = 500;

/// サンドボックスのセッションで出すツール。CLI が内部から呼ぶ 4 本だけ。ホストの任意の
/// パスを開く `openFile` や、保存で整形 (= ワークツリーのコードの実行) を引き起こす
/// `saveDocument` などは出さない。
pub const SANDBOX_TOOL_NAMES: [&str; 4] = [
    "openDiff",
    "close_tab",
    "closeAllDiffTabs",
    "getDiagnostics",
];

/// どのツールを出し、ツールの引数と結果のパスをどう扱うか。
#[derive(Clone)]
pub enum ToolScope {
    /// ホストの TCP サーバー。ツールはすべて出し、パスはそのまま扱う。
    Full,
    /// サンドボックスのセッション。ツールは [`SANDBOX_TOOL_NAMES`] だけで、パスは変換した
    /// うえでワークツリーの中に閉じ込める。
    Sandboxed(Arc<SandboxPaths>),
}

impl ToolScope {
    pub fn allows_tool(&self, name: &str) -> bool {
        match self {
            ToolScope::Full => true,
            ToolScope::Sandboxed(_) => SANDBOX_TOOL_NAMES.contains(&name),
        }
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
    fn max_message_bytes(&self) -> usize {
        HOST_MAX_MESSAGE_BYTES
    }

    /// 同時に開いておける差分タブの数。`None` なら上限なし。
    fn max_open_diffs(&self) -> Option<usize> {
        None
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

fn client_error(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(ClientError(message.into()))
}

fn too_many_diffs_error(limit: usize) -> anyhow::Error {
    client_error(format!(
        "{limit} diff tabs are already open for this session; close one before proposing another"
    ))
}

/// ツールの失敗を、呼び出し元に見せる文にする。サンドボックスには [`ClientError`] だけを
/// そのまま見せ、残りはホストのログにだけ残す。
fn error_text(tool: &str, error: &anyhow::Error, scope: &ToolScope) -> String {
    match scope {
        ToolScope::Full => format!("{error:#}"),
        ToolScope::Sandboxed(_) => match error.downcast_ref::<ClientError>() {
            Some(visible) => visible.to_string(),
            None => {
                // 相手の求めごとに出るので、ログを埋めさせないよう debug に留める。
                log::debug!("ide_server: {tool} failed for a sandboxed client: {error:#}");
                format!("{tool} failed")
            }
        },
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

    /// CLI が受ける `selection_changed` 通知の形。
    pub fn to_notification_params(&self) -> Value {
        let file_path = self
            .file_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        self.notification_params(file_path.as_deref())
    }

    /// パスだけを CLI 側の形に差し替えた `selection_changed` 通知。
    pub fn notification_params(&self, file_path: Option<&str>) -> Value {
        json!({
            "selection": {
                "start": { "line": self.start_line, "character": self.start_character },
                "end": { "line": self.end_line, "character": self.end_character },
            },
            "text": self.text,
            "filePath": file_path,
        })
    }

    /// CLI に見せてはいけないバッファーに移ったときの通知。選択が無くなったことだけを伝える。
    pub fn cleared_notification_params() -> Value {
        json!({
            "selection": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 0 },
            },
            "text": "",
            "filePath": null,
        })
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

pub fn initialize_result(params: &Value) -> Value {
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
            name: "Zed".to_string(),
            title: Some("Zed".to_string()),
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: None,
        },
    })
}

fn tool(name: &str, description: &str, schema: Value) -> Tool {
    Tool {
        name: name.to_string(),
        title: None,
        description: Some(description.to_string()),
        input_schema: schema,
        output_schema: None,
        annotations: None,
    }
}

fn object_schema(properties: Value, required: &[&str]) -> Value {
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
             or omit it to get diagnostics for every file that has them.",
            object_schema(
                json!({
                    "uri": {
                        "type": "string",
                        "description": "Optional file:// URI. Omit for all files."
                    }
                }),
                &[],
            ),
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
             accept or reject it.",
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
            "Not supported by Zed. Always returns an error.",
            object_schema(json!({ "code": { "type": "string" } }), &["code"]),
        ),
    ]
}

pub fn list_tools_result(scope: &ToolScope) -> Value {
    let tools = tools()
        .into_iter()
        .filter(|tool| scope.allows_tool(&tool.name))
        .collect::<Vec<_>>();
    json!({ "tools": tools })
}

fn text_content(text: impl Into<String>) -> Value {
    json!({ "type": "text", "text": text.into() })
}

fn json_content(value: &Value) -> Result<Value> {
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
    cx: &mut AsyncApp,
) -> Option<Value> {
    let (scope, workspace) =
        server.read_with(cx, |server, _| (server.tool_scope(), server.workspace()));
    if !scope.allows_tool(&params.name) {
        return None;
    }
    let arguments = params.arguments.unwrap_or(Value::Null);
    let result = match workspace {
        Some(workspace) => {
            run_tool(
                &params.name,
                arguments,
                server,
                &workspace,
                connection,
                &scope,
                cx,
            )
            .await
        }
        None => Err(anyhow!("no Zed window has this workspace open")),
    };
    Some(match result {
        Ok(content) => json!({ "content": content, "isError": false }),
        Err(error) => json!({
            "content": [text_content(error_text(&params.name, &error, &scope))],
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
            let diagnostics = collect_diagnostics(workspace, uri.as_deref(), scope, cx).await?;
            Ok(vec![json_content(&diagnostics)?])
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
            "Zed does not implement executeCode; there is no Jupyter kernel attached to this IDE server"
        )),
        other => Err(anyhow!("unknown tool {other}")),
    }
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
struct SandboxRequest<'a> {
    paths: &'a SandboxPaths,
    fs: Arc<dyn Fs>,
    executor: gpui::BackgroundExecutor,
    real_roots: RealRoots,
}

impl<'a> SandboxRequest<'a> {
    async fn new(paths: &'a SandboxPaths, project: &Entity<Project>, cx: &mut AsyncApp) -> Self {
        let fs = project.read_with(cx, |project, _| project.fs().clone());
        let real_roots = path_mapping::real_roots(paths, &fs).await;
        Self {
            paths,
            fs,
            executor: cx.background_executor().clone(),
            real_roots,
        }
    }

    /// CLI から来たパスを変換し、ワークツリーの外を指すもの (中に置かれたリンクの行き先を
    /// 含む) と、リンクを通るものを拒否する。
    async fn resolve(&self, text: &str) -> Result<PathBuf> {
        let host_path = self
            .paths
            .to_host(text)
            .map_err(|error| client_error(error.to_string()))?;
        match path_mapping::has_link_below_root(self.paths, &host_path, &self.fs, &self.executor)
            .await
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
        if let Err(error) = self.real_roots.ensure_contains(&host_path, &self.fs).await {
            log::debug!("ide_server: refused a path for a sandboxed client: {error:#}");
            return Err(client_error(format!("{text} is outside the workspace")));
        }
        Ok(host_path)
    }

    /// 差分の旧内容を、ファイルから 1 度だけ読む。まだ無いファイルなら空。
    async fn read_old_contents(
        &self,
        host_path: &Path,
        text: &str,
        cx: &mut AsyncApp,
    ) -> Result<String> {
        let executor = cx.background_executor().clone();
        match self
            .real_roots
            .read_file(host_path, &self.fs, &executor)
            .await
        {
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
    async fn verify_open_buffer(
        &self,
        buffer: &Entity<Buffer>,
        cx: &mut AsyncApp,
    ) -> Result<PathBuf> {
        let (abs_path, is_external) = buffer
            .read_with(cx, |buffer, cx| {
                let file = WorktreeFile::from_dyn(buffer.file())?;
                let worktree = file.worktree.read(cx);
                let is_external = file
                    .entry_id
                    .and_then(|entry_id| worktree.entry_for_id(entry_id))
                    .is_some_and(|entry| entry.is_external);
                Some((worktree.absolutize(&file.path), is_external))
            })
            .context("the buffer is not a file in a worktree")?;
        anyhow::ensure!(!is_external, "the worktree marks the file as outside it");
        anyhow::ensure!(
            self.paths.contains(&abs_path),
            "the file is outside the workspace"
        );
        anyhow::ensure!(
            !path_mapping::has_link_below_root(self.paths, &abs_path, &self.fs, &self.executor)
                .await?,
            "the file goes through a link"
        );
        self.real_roots.ensure_contains(&abs_path, &self.fs).await?;
        Ok(abs_path)
    }
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
        "message": "Opened file in Zed",
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

async fn collect_diagnostics(
    workspace: &WeakEntity<Workspace>,
    uri: Option<&str>,
    scope: &ToolScope,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let project = workspace.read_with(cx, |workspace, _| workspace.project().clone())?;
    let files = match scope {
        ToolScope::Full => host_diagnostics(&project, uri, cx).await?,
        ToolScope::Sandboxed(paths) => sandbox_diagnostics(&project, uri, paths, cx).await?,
    };
    Ok(Value::Array(files))
}

async fn host_diagnostics(
    project: &Entity<Project>,
    uri: Option<&str>,
    cx: &mut AsyncApp,
) -> Result<Vec<Value>> {
    let project_paths = match uri {
        Some(uri) => {
            let path = path_from_uri(uri);
            let project_path = project
                .read_with(cx, |project, cx| project.find_project_path(&path, cx))
                .ok_or_else(|| anyhow!("{} is not inside a worktree", path.display()))?;
            vec![project_path]
        }
        None => project.read_with(cx, |project, cx| {
            project
                .diagnostic_summaries(false, cx)
                .map(|(project_path, _, _)| project_path)
                .take(MAX_DIAGNOSTIC_FILES)
                .collect::<Vec<_>>()
        }),
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
    Ok(files)
}

/// サンドボックスへの診断。すでに開いているバッファーにある診断だけを返し、URI を指定されても
/// ファイルは開かない。サンドボックスの求めでファイルを開くと、サンドボックスが書いたファイル
/// でホストの言語サーバーが起きる。
async fn sandbox_diagnostics(
    project: &Entity<Project>,
    uri: Option<&str>,
    paths: &SandboxPaths,
    cx: &mut AsyncApp,
) -> Result<Vec<Value>> {
    let request = SandboxRequest::new(paths, project, cx).await;
    let buffers = match uri {
        Some(uri) => {
            let host_path = request.resolve(uri).await?;
            let project_path = project
                .read_with(cx, |project, cx| project.find_project_path(&host_path, cx))
                .ok_or_else(|| client_error(format!("{uri} is not inside a worktree")))?;
            project
                .read_with(cx, |project, cx| project.get_open_buffer(&project_path, cx))
                .into_iter()
                .collect::<Vec<_>>()
        }
        None => project.read_with(cx, |project, cx| {
            let mut seen = HashSet::default();
            project
                .diagnostic_summaries(false, cx)
                .filter(|(project_path, _, _)| seen.insert(project_path.clone()))
                .filter_map(|(project_path, _, _)| project.get_open_buffer(&project_path, cx))
                .take(MAX_SANDBOX_DIAGNOSTIC_FILES)
                .collect::<Vec<_>>()
        }),
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
    Ok(files)
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
                    "message": entry.diagnostic.message,
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
    if matches!(scope, ToolScope::Sandboxed(_)) {
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
        .unwrap_or("Zed diff")
        .to_string();

    // 上限のあるセッションでは、パスを調べて旧内容を読む前に枠を押さえる。枠は登録した差分が
    // 引き継ぐ。同時に来た `openDiff` が、みな読み終えてから上限で断られることのないように。
    let reservation = server.update(cx, |server, _| {
        server
            .max_open_diffs()
            .map(|limit| server.open_diffs().reserve(limit))
            .transpose()
    })?;

    let (old_buffer, new_buffer, new_file_path) = match scope {
        ToolScope::Full => {
            let old_file_path = path_from_uri(old_text);
            let (old_buffer, new_buffer) =
                project_diff_buffers(&project, &old_file_path, new_file_contents, cx).await?;
            (old_buffer, new_buffer, path_from_uri(new_text))
        }
        ToolScope::Sandboxed(paths) => {
            let request = SandboxRequest::new(paths, &project, cx).await;
            let old_file_path = request.resolve(old_text).await?;
            let new_file_path = request.resolve(new_text).await?;
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
    let sandboxed = matches!(scope, ToolScope::Sandboxed(_));
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
    let outcome = outcome.await.unwrap_or(IdeServerDiffOutcome::Rejected);
    server.update(cx, |server, _| server.open_diffs().finish(diff_id));

    // `FILE_SAVED` の 2 番目のテキストが、CLI が「承認された本文」として読む唯一の
    // 出力。CLI はこれと差分を開く前に自分で読んだ本文から編集を組み直し、保留中の
    // Edit / Write ツールに書かせる。詳細は [`crate::diff_view`] を参照。
    Ok(match outcome {
        IdeServerDiffOutcome::Accepted { contents } => {
            vec![text_content("FILE_SAVED"), text_content(contents)]
        }
        IdeServerDiffOutcome::Rejected => vec![text_content("DIFF_REJECTED")],
    })
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
