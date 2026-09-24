//! ADR-F56: エージェントに渡す**アシストのツール** (MCP サーバー `zed`、口 `/mcp/zed`)。
//!
//! 表示 (`highlightCode` / `annotateCode` / `startCodeTour` / `clearAnnotations`) は
//! [`ide_query::code_display`] 越しに `ide_assist` が描き、読み取り (`getHover` /
//! `getDocumentSymbols` / `searchSymbols` / `findDefinition` / `findReferences`) は
//! [`ide_query`] の関数に任せる。ここが受け持つのは引数の検査・パスの写像・結果の整形だけ。
//!
//! トークンはサンドボックスの中から読めるので、どのツールも「サンドボックスの中の任意の
//! プロセスから呼ばれても無害」を前提にする: 引数のパスには既存の検査 (ワークツリーの中・
//! リンクを通らない・走査に出る・4 MiB 以下) と `private_files` の拒否を掛け、件数・文字数には
//! 上限を置き、読み取り結果の中身 (1 行のプレビュー・シグネチャ) はプロジェクトの中で同じ
//! 検査を通るファイルのものだけを返す。

use std::path::Path;

use anyhow::{Result, anyhow};
use collections::HashMap;
use context_server::types::Tool;
use gpui::{App, AsyncApp, Entity, WeakEntity};
use ide_query::{
    AssistSource, CodeAnnotation, CodeDisplay, CodeHighlight, CodeTour, DisplayOutcome,
    LinePosition, LineRange, MAX_ANNOTATIONS, MAX_DISPLAY_MARKDOWN_CHARS, MAX_HIGHLIGHTS,
    MAX_QUERY_RESULTS, MAX_TOUR_STEPS, QueryLocation, SymbolHit, SymbolSearchOptions, TourStep,
};
use project::{Project, ProjectPath, WorktreeSettings};
use serde_json::{Value, json};
use settings::Settings as _;
use util::rel_path::RelPath;
use workspace::Workspace;

use crate::MCP_SERVER_NAME;
use crate::path_mapping::MAX_SANDBOX_FILE_BYTES;
use crate::tools::{
    SandboxRequest, ToolScope, client_error, is_private_file, json_content, object_schema,
    path_from_uri, prefix_client_error, tool,
};
use crate::transport::MessageLimits;

/// アシストのツール。モデルからは `mcp__zed__<名>` に見える。
pub const ASSIST_TOOL_NAMES: [&str; 9] = [
    "highlightCode",
    "annotateCode",
    "startCodeTour",
    "clearAnnotations",
    "getHover",
    "getDocumentSymbols",
    "searchSymbols",
    "findDefinition",
    "findReferences",
];

/// CLI に渡す `--mcp-config` の JSON が `Authorization` ヘッダーの値 (`Bearer <トークン>`) を
/// 丸ごと読む環境変数 (CLI が `${…}` を展開する)。トークンをコマンド引数に載せない
/// (ADR-F49 item 5) ためで、名前は CLI が機密として展開を拒みうる名前 (`TOKEN` / `KEY` /
/// `SECRET` を含むもの) を避けてある。
///
/// 値に `Bearer ` まで含めるのは、JSON に空白を残さないため: Windows PowerShell 5.1 から npm の
/// `claude.ps1` を通すと、空白を含む引数が割れて CLI が起動しない。
pub const ASSIST_BEARER_ENV_VAR: &str = "ZED_IDE_BEARER";

/// アシストの口の同時接続。ACP の制限セッションと同じく streamable HTTP は 1 要求ごとに
/// 接続を使い捨てるので多めに取り、セッションの IDE の口の枠とは別に数える。
pub(crate) const MAX_ASSIST_CONNECTIONS: usize = 16;

/// アシストの口が受け取るのはツールの引数だけ (注釈の本文は上限 4000 文字 × 20 件)。
/// 同時接続を多めに許すぶん、受け取る総量をここで抑える。送る側は結果 100 件ぶんを運べる大きさ。
pub(crate) const ASSIST_MESSAGE_LIMITS: MessageLimits = MessageLimits {
    incoming: 1024 * 1024,
    outgoing: 16 * 1024 * 1024,
};

/// `searchSymbols` の `query` と、位置の `symbol` の上限。
const MAX_QUERY_TEXT_CHARS: usize = 1000;
/// `getHover` の本文の上限 (`ide_query` は長さを切らない)。
const MAX_HOVER_CHARS: usize = 8000;
/// `getDocumentSymbols` の件数の上限 (`ide_query` は 1,000 件まで返す)。
const MAX_DOCUMENT_SYMBOLS: usize = 500;
/// `searchSymbols` の `limit` を省いたとき (上限は [`MAX_QUERY_RESULTS`])。
const DEFAULT_SEARCH_RESULTS: usize = 50;
/// ツアーとその各段の見出しの上限。
const MAX_TITLE_CHARS: usize = 200;
/// 引数のパスの上限。
const MAX_PATH_CHARS: usize = 4096;

/// MCP の initialize の結果に載せる、モデル向けの説明 (英語、短く)。
pub(crate) const INSTRUCTIONS: &str = "You are connected to the Zed editor that the user is looking at (zed-jp-enhance).
Use highlightCode / annotateCode to point at code in the user's editor instead of pasting it, and startCodeTour for step-by-step walkthroughs across files (the user steps with ‹ › buttons).
Use getHover / findDefinition / findReferences / getDocumentSymbols / searchSymbols for precise, language-server-backed navigation (searchSymbols is a structural symbol search, not embeddings).
Paths may be absolute or relative to a workspace folder; lines are 1-based and inclusive.
Annotations are not saved, and calling a display tool again replaces what you showed before.";

pub(crate) const ASSIST_UNAVAILABLE_MESSAGE: &str =
    "IDE ツール (zed) は Zed の設定で無効になっています。";
pub(crate) const IDE_SERVER_DISABLED_MESSAGE: &str = "Zed の IDE サーバーは無効になっています。";

/// CLI の `--mcp-config` に渡す JSON。URL だけが具体的で、`Authorization` の値は
/// [`ASSIST_BEARER_ENV_VAR`] を CLI に展開させる。空白を含まない (理由は
/// [`ASSIST_BEARER_ENV_VAR`])。
pub fn cli_assist_config_json(url: &str) -> String {
    json!({
        "mcpServers": {
            MCP_SERVER_NAME: {
                "type": "http",
                "url": url,
                "headers": {
                    "Authorization": format!("${{{ASSIST_BEARER_ENV_VAR}}}"),
                },
            }
        }
    })
    .to_string()
}

/// CLI に足す引数: `--mcp-config <JSON>` と、9 本を事前に許可する `--allowedTools`。
/// どれも表示だけか、ワークツリーの中の読み取りだけ (CLI の Read / Grep と同等)。
pub fn cli_assist_arguments(url: &str) -> Vec<String> {
    let allowed_tools = ASSIST_TOOL_NAMES
        .iter()
        .map(|name| format!("mcp__{MCP_SERVER_NAME}__{name}"))
        .collect::<Vec<_>>()
        .join(",");
    vec![
        "--mcp-config".to_string(),
        cli_assist_config_json(url),
        "--allowedTools".to_string(),
        allowed_tools,
    ]
}

const PATH_DESCRIPTION: &str =
    "Absolute path as you see it, or a path relative to a workspace folder.";

fn line_schema(description: &str) -> Value {
    json!({ "type": "integer", "minimum": 1, "description": description })
}

fn text_schema(max_chars: usize, description: &str) -> Value {
    json!({ "type": "string", "maxLength": max_chars, "description": description })
}

fn items_schema(max_items: usize, properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "array",
        "minItems": 1,
        "maxItems": max_items,
        "items": object_schema(properties, required),
    })
}

fn position_schema() -> Value {
    object_schema(
        json!({
            "path": { "type": "string", "description": PATH_DESCRIPTION },
            "line": line_schema("1-based line the symbol is on."),
            "symbol": text_schema(
                MAX_QUERY_TEXT_CHARS,
                "Text of the symbol on that line; picks the column.",
            ),
            "column": line_schema("1-based column in characters; wins over symbol."),
        }),
        &["path", "line"],
    )
}

pub(crate) fn tools() -> Vec<Tool> {
    vec![
        tool(
            "highlightCode",
            "Highlight line ranges in the user's Zed editor, with an optional short note each. \
             Replaces your previous highlights.",
            object_schema(
                json!({
                    "items": items_schema(
                        MAX_HIGHLIGHTS,
                        json!({
                            "path": { "type": "string", "description": PATH_DESCRIPTION },
                            "startLine": line_schema("1-based first line."),
                            "endLine": line_schema("1-based last line (inclusive)."),
                            "note": text_schema(MAX_DISPLAY_MARKDOWN_CHARS, "Optional short note."),
                        }),
                        &["path", "startLine", "endLine"],
                    ),
                }),
                &["items"],
            ),
        ),
        tool(
            "annotateCode",
            "Show Markdown explanations next to line ranges in the user's Zed editor. \
             Replaces your previous annotations. Images, links and HTML are not rendered.",
            object_schema(
                json!({
                    "items": items_schema(
                        MAX_ANNOTATIONS,
                        json!({
                            "path": { "type": "string", "description": PATH_DESCRIPTION },
                            "startLine": line_schema("1-based first line."),
                            "endLine": line_schema("1-based last line (inclusive)."),
                            "markdown": text_schema(MAX_DISPLAY_MARKDOWN_CHARS, "The explanation."),
                        }),
                        &["path", "startLine", "endLine", "markdown"],
                    ),
                }),
                &["items"],
            ),
        ),
        tool(
            "startCodeTour",
            "Start a step-by-step tour across files in the user's Zed editor. The user moves \
             between the steps with ‹ › buttons. Replaces your previous tour.",
            object_schema(
                json!({
                    "title": text_schema(MAX_TITLE_CHARS, "Title of the tour."),
                    "steps": items_schema(
                        MAX_TOUR_STEPS,
                        json!({
                            "path": { "type": "string", "description": PATH_DESCRIPTION },
                            "startLine": line_schema("1-based first line."),
                            "endLine": line_schema("1-based last line (inclusive)."),
                            "title": text_schema(MAX_TITLE_CHARS, "Title of the step."),
                            "markdown": text_schema(MAX_DISPLAY_MARKDOWN_CHARS, "The explanation."),
                        }),
                        &["path", "startLine", "endLine", "title", "markdown"],
                    ),
                }),
                &["title", "steps"],
            ),
        ),
        tool(
            "clearAnnotations",
            "Remove every highlight, annotation and tour you showed in the user's Zed editor.",
            object_schema(json!({}), &[]),
        ),
        tool(
            "getHover",
            "Get the language server's hover (type and documentation) for a symbol.",
            position_schema(),
        ),
        tool(
            "getDocumentSymbols",
            "List the symbols (functions, types, …) defined in a file, with their line ranges.",
            object_schema(
                json!({ "path": { "type": "string", "description": PATH_DESCRIPTION } }),
                &["path"],
            ),
        ),
        tool(
            "searchSymbols",
            "Search the workspace's symbols by name (language-server workspace symbols and \
             syntax outlines, ranked by name, container, path and signature). This is a \
             structural search, not an embedding search.",
            object_schema(
                json!({
                    "query": text_schema(MAX_QUERY_TEXT_CHARS, "Name or part of a name."),
                    "pathPrefix": {
                        "type": "string",
                        "description": "Only return symbols under this folder (absolute, or relative to a workspace folder).",
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_QUERY_RESULTS,
                        "description": format!("Maximum number of results (default {DEFAULT_SEARCH_RESULTS})."),
                    },
                }),
                &["query"],
            ),
        ),
        tool(
            "findDefinition",
            "Find where a symbol is defined, using the language server.",
            position_schema(),
        ),
        tool(
            "findReferences",
            "Find the references to a symbol, using the language server.",
            position_schema(),
        ),
    ]
}

/// 引数のパスの確かめ方。ホストのクライアント (サンドボックスの外) か、サンドボックスの
/// セッション (ローカルの WSL・SSH の接続先) か。
pub(crate) enum AssistPaths<'a> {
    Host,
    Sandbox(SandboxRequest<'a>),
}

pub(crate) async fn run_assist_tool(
    name: &str,
    arguments: Value,
    workspace: &WeakEntity<Workspace>,
    source: &AssistSource,
    scope: &ToolScope,
    cx: &mut AsyncApp,
) -> Result<Vec<Value>> {
    let project = workspace.read_with(cx, |workspace, _| workspace.project().clone())?;
    let sandbox_paths = scope.sandbox_paths().cloned();
    let paths = match sandbox_paths.as_deref() {
        Some(paths) => AssistPaths::Sandbox(SandboxRequest::new(paths, &project, cx).await),
        None => AssistPaths::Host,
    };
    run_assist_tool_with(name, arguments, project, paths, source, cx).await
}

/// [`run_assist_tool`] の中身。テストはリモートの確かめ方 (`SandboxChecksKind::OnTheRemoteHost`)
/// をローカルのプロジェクトに当てるために、確かめ方を外から渡す。
pub(crate) async fn run_assist_tool_with(
    name: &str,
    arguments: Value,
    project: Entity<Project>,
    paths: AssistPaths<'_>,
    source: &AssistSource,
    cx: &mut AsyncApp,
) -> Result<Vec<Value>> {
    let mut context = AssistContext {
        project,
        paths,
        contents_allowed: HashMap::default(),
    };
    let result = match name {
        "highlightCode" => highlight_code(&mut context, &arguments, source, cx).await?,
        "annotateCode" => annotate_code(&mut context, &arguments, source, cx).await?,
        "startCodeTour" => start_code_tour(&mut context, &arguments, source, cx).await?,
        "clearAnnotations" => {
            let display = code_display(cx)?;
            let project = context.project.clone();
            cx.update(|cx| display.clear(&project, source, cx));
            json!({ "cleared": true })
        }
        "getHover" => {
            let (path, position) = context.position(&arguments, cx).await?;
            let project = context.project.clone();
            let hover = cx
                .update(|cx| ide_query::hover(&project, path, position, cx))
                .await
                .map_err(|error| context.query_failed("getHover", error))?;
            hover_json(hover)
        }
        "getDocumentSymbols" => {
            let path_text = string_field(&arguments, "path")?;
            let path = context.resolve(path_text, cx).await?;
            let project = context.project.clone();
            let symbols = cx
                .update(|cx| ide_query::document_symbols(&project, path, cx))
                .await
                .map_err(|error| context.query_failed("getDocumentSymbols", error))?;
            context
                .symbols_json(symbols, MAX_DOCUMENT_SYMBOLS, cx)
                .await
        }
        "searchSymbols" => search_symbols(&mut context, &arguments, cx).await?,
        "findDefinition" | "findReferences" => {
            let (path, position) = context.position(&arguments, cx).await?;
            let project = context.project.clone();
            let locations = if name == "findDefinition" {
                cx.update(|cx| ide_query::definitions(&project, path, position, cx))
                    .await
            } else {
                cx.update(|cx| ide_query::references(&project, path, position, cx))
                    .await
            }
            .map_err(|error| context.query_failed(name, error))?;
            context.locations_json(locations, cx).await
        }
        other => return Err(anyhow!("unknown tool {other}")),
    };
    Ok(vec![json_content(&result)?])
}

fn code_display(cx: &mut AsyncApp) -> Result<std::sync::Arc<dyn CodeDisplay>> {
    cx.update(|cx| ide_query::code_display(cx))
        .ok_or_else(|| client_error("このウィンドウでは、エディターに表示する機能が使えません。"))
}

async fn highlight_code(
    context: &mut AssistContext<'_>,
    arguments: &Value,
    source: &AssistSource,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let items = array_field(arguments, "items", MAX_HIGHLIGHTS)?;
    let mut highlights = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let highlight = async {
            let lines = line_range(item)?;
            let note = optional_text(item, "note", MAX_DISPLAY_MARKDOWN_CHARS)?;
            let path = context.resolve(string_field(item, "path")?, cx).await?;
            anyhow::Ok(CodeHighlight { path, lines, note })
        }
        .await
        .map_err(|error| prefix_client_error(&format!("items[{index}]: "), error))?;
        highlights.push(highlight);
    }
    let display = code_display(cx)?;
    let project = context.project.clone();
    let source = source.clone();
    let outcome = cx
        .update(|cx| display.show_highlights(&project, source, highlights, cx))
        .await?;
    Ok(context.outcome_json(outcome, cx))
}

async fn annotate_code(
    context: &mut AssistContext<'_>,
    arguments: &Value,
    source: &AssistSource,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let items = array_field(arguments, "items", MAX_ANNOTATIONS)?;
    let mut annotations = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let annotation = async {
            let lines = line_range(item)?;
            let markdown = required_text(item, "markdown", MAX_DISPLAY_MARKDOWN_CHARS)?;
            let path = context.resolve(string_field(item, "path")?, cx).await?;
            anyhow::Ok(CodeAnnotation {
                path,
                lines,
                markdown,
            })
        }
        .await
        .map_err(|error| prefix_client_error(&format!("items[{index}]: "), error))?;
        annotations.push(annotation);
    }
    let display = code_display(cx)?;
    let project = context.project.clone();
    let source = source.clone();
    let outcome = cx
        .update(|cx| display.show_annotations(&project, source, annotations, cx))
        .await?;
    Ok(context.outcome_json(outcome, cx))
}

async fn start_code_tour(
    context: &mut AssistContext<'_>,
    arguments: &Value,
    source: &AssistSource,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let title = required_text(arguments, "title", MAX_TITLE_CHARS)?;
    let items = array_field(arguments, "steps", MAX_TOUR_STEPS)?;
    let mut steps = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let step = async {
            let lines = line_range(item)?;
            let title = required_text(item, "title", MAX_TITLE_CHARS)?;
            let markdown = required_text(item, "markdown", MAX_DISPLAY_MARKDOWN_CHARS)?;
            let path = context.resolve(string_field(item, "path")?, cx).await?;
            anyhow::Ok(TourStep {
                path,
                lines,
                title,
                markdown,
            })
        }
        .await
        .map_err(|error| prefix_client_error(&format!("steps[{index}]: "), error))?;
        steps.push(step);
    }
    let display = code_display(cx)?;
    let project = context.project.clone();
    let source = source.clone();
    let outcome = cx
        .update(|cx| display.start_tour(&project, source, CodeTour { title, steps }, cx))
        .await?;
    Ok(context.outcome_json(outcome, cx))
}

async fn search_symbols(
    context: &mut AssistContext<'_>,
    arguments: &Value,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let query = required_text(arguments, "query", MAX_QUERY_TEXT_CHARS)?;
    if query.trim().is_empty() {
        return Err(client_error("query が空です。"));
    }
    let limit = search_limit(arguments)?;
    let path_prefix = match optional_string(arguments, "pathPrefix")? {
        Some(prefix) if !prefix.trim().is_empty() => Some(context.path_prefix(prefix, cx)?),
        _ => None,
    };
    let project = context.project.clone();
    let hits = cx
        .update(|cx| {
            ide_query::search_symbols(
                &project,
                &query,
                SymbolSearchOptions { limit, path_prefix },
                cx,
            )
        })
        .await
        .map_err(|error| context.query_failed("searchSymbols", error))?;
    Ok(context.symbols_json(hits, limit, cx).await)
}

struct AssistContext<'a> {
    project: Entity<Project>,
    paths: AssistPaths<'a>,
    /// 読み取り結果のファイルごとの「中身を見せてよいか」。同じファイルの結果が並ぶので、
    /// 確かめ (ファイルシステム・接続先への問い合わせ) は 1 ファイル 1 度にする。
    contents_allowed: HashMap<ProjectPath, bool>,
}

impl AssistContext<'_> {
    fn is_sandboxed(&self) -> bool {
        matches!(self.paths, AssistPaths::Sandbox(_))
    }

    /// 言語サーバーへの問い合わせの失敗。言語サーバーが居ない (未インストール・起動中・承認待ち)
    /// ときは、エージェントが待ってからやり直せるよう、それと分かる短文で返す。
    ///
    /// ほかの失敗は `ide_query` が組み立てた一文 (位置が見つからない・時間切れなど、パスは
    /// ワークツリーの名前から始まる形) だけを見せ、原因の連鎖はホストのログにだけ残す
    /// (サンドボックスにはホストのエラー文を見せない、[`crate::tools`] の方針)。
    fn query_failed(&self, tool: &str, error: anyhow::Error) -> anyhow::Error {
        query_failure(self.is_sandboxed(), tool, error)
    }

    /// 位置を取るツール (`getHover` / `findDefinition` / `findReferences`) の引数。
    async fn position(
        &mut self,
        arguments: &Value,
        cx: &mut AsyncApp,
    ) -> Result<(ProjectPath, LinePosition)> {
        let line = line_field(arguments, "line")?;
        let symbol = optional_text(arguments, "symbol", MAX_QUERY_TEXT_CHARS)?
            .filter(|symbol| !symbol.trim().is_empty());
        let column = match arguments.get("column") {
            None | Some(Value::Null) => None,
            Some(_) => Some(line_field(arguments, "column")?),
        };
        let path = self.resolve(string_field(arguments, "path")?, cx).await?;
        Ok((
            path,
            LinePosition {
                line,
                symbol,
                column,
            },
        ))
    }

    /// 引数のパス (絶対パス・ルートからの相対パス・`file://`) を、検査を通したうえで
    /// プロジェクトのパスにする。断るときの理由はどれも日本語の短文。
    async fn resolve(&mut self, text: &str, cx: &mut AsyncApp) -> Result<ProjectPath> {
        let text = text.trim();
        if text.is_empty() {
            return Err(client_error("path が空です。"));
        }
        if text.chars().count() > MAX_PATH_CHARS {
            return Err(client_error(format!(
                "path は {MAX_PATH_CHARS} 文字までにしてください。"
            )));
        }
        let project_path = match &self.paths {
            AssistPaths::Host => self
                .project
                .read_with(cx, |project, cx| host_project_path(project, text, cx))?,
            AssistPaths::Sandbox(request) => {
                let client_text = self.project.read_with(cx, |project, cx| {
                    sandbox_client_text(request.paths(), project, text, cx)
                })?;
                let host_path = request
                    .paths()
                    .to_host(&client_text)
                    .map_err(|_| client_error(outside_message(text)))?;
                if let Err(error) = request.resolve(&client_text, cx).await {
                    log::debug!(
                        "ide_server: refused an assist path for a sandboxed client: {error:#}"
                    );
                    return Err(client_error(format!(
                        "{text} はリンクを経由しているか、確かめられないため扱えません。"
                    )));
                }
                self.project
                    .read_with(cx, |project, cx| project.find_project_path(&host_path, cx))
                    .ok_or_else(|| client_error(outside_message(text)))?
            }
        };
        self.project.read_with(cx, |project, cx| {
            match entry_refusal(project, &project_path, text, true, cx) {
                Some(refusal) => Err(client_error(refusal)),
                None => Ok(()),
            }
        })?;
        Ok(project_path)
    }

    /// `searchSymbols` の `pathPrefix` を、`ide_query` が受ける形 (ワークツリーのルートの名前から
    /// 始まる `/` 区切り) にする。ルートの名前を付けるのは、同じ相対パスを持つ別のワークツリーを
    /// 巻き込まないため。
    fn path_prefix(&self, text: &str, cx: &mut AsyncApp) -> Result<String> {
        let text = text.trim();
        if text.chars().count() > MAX_PATH_CHARS {
            return Err(client_error(format!(
                "pathPrefix は {MAX_PATH_CHARS} 文字までにしてください。"
            )));
        }
        let project_path = match &self.paths {
            AssistPaths::Host => self
                .project
                .read_with(cx, |project, cx| host_project_path(project, text, cx))?,
            AssistPaths::Sandbox(request) => {
                let client_text = self.project.read_with(cx, |project, cx| {
                    sandbox_client_text(request.paths(), project, text, cx)
                })?;
                let host_path = request
                    .paths()
                    .to_host(&client_text)
                    .map_err(|_| client_error(outside_message(text)))?;
                self.project
                    .read_with(cx, |project, cx| project.find_project_path(&host_path, cx))
                    .ok_or_else(|| client_error(outside_message(text)))?
            }
        };
        let root_name = self.project.read_with(cx, |project, cx| {
            project
                .worktree_for_id(project_path.worktree_id, cx)
                .filter(|worktree| worktree.read(cx).is_visible())
                .map(|worktree| worktree.read(cx).root_name_str().to_string())
        });
        let root_name = root_name.ok_or_else(|| client_error(outside_message(text)))?;
        Ok(if project_path.path.is_empty() {
            root_name
        } else {
            format!("{root_name}/{}", project_path.path.as_unix_str())
        })
    }

    /// 結果のパスを、エージェントから見える形にする。サンドボックスでは写像できないパスは
    /// `None` (ファイル名だけを別に返す)。
    fn display_path(&self, project_path: &ProjectPath, cx: &App) -> Option<String> {
        let abs_path = self.project.read(cx).absolute_path(project_path, cx)?;
        match &self.paths {
            AssistPaths::Host => Some(abs_path.to_string_lossy().into_owned()),
            AssistPaths::Sandbox(request) => request
                .paths()
                .to_client(&abs_path)
                .or_else(|| request.paths().mapper().to_client(&abs_path)),
        }
    }

    /// 読み取り結果の中身 (プレビュー・シグネチャ) を見せてよいファイルか。引数のパスと同じ
    /// 検査 (大きさを除く) を通るものだけ。
    async fn allows_contents(&mut self, project_path: &ProjectPath, cx: &mut AsyncApp) -> bool {
        if let Some(allowed) = self.contents_allowed.get(project_path) {
            return *allowed;
        }
        let allowed = self.check_contents(project_path, cx).await;
        self.contents_allowed.insert(project_path.clone(), allowed);
        allowed
    }

    async fn check_contents(&self, project_path: &ProjectPath, cx: &mut AsyncApp) -> bool {
        let (passes_entry_checks, abs_path) = self.project.read_with(cx, |project, cx| {
            (
                entry_refusal(project, project_path, "", false, cx).is_none(),
                project.absolute_path(project_path, cx),
            )
        });
        if !passes_entry_checks {
            return false;
        }
        match (&self.paths, abs_path) {
            (AssistPaths::Host, _) => true,
            (AssistPaths::Sandbox(request), Some(abs_path)) => {
                request
                    .allows_result_contents(project_path, &abs_path, cx)
                    .await
            }
            (AssistPaths::Sandbox(_), None) => false,
        }
    }

    fn path_fields(
        &self,
        project_path: &ProjectPath,
        object: &mut serde_json::Map<String, Value>,
        cx: &App,
    ) {
        match self.display_path(project_path, cx) {
            Some(path) => {
                object.insert("path".to_string(), Value::String(path));
            }
            None => {
                object.insert("path".to_string(), Value::Null);
                if let Some(file_name) = project_path.path.file_name() {
                    object.insert("fileName".to_string(), Value::String(file_name.to_string()));
                }
            }
        }
    }

    async fn locations_json(&mut self, locations: Vec<QueryLocation>, cx: &mut AsyncApp) -> Value {
        let truncated = locations.len() > MAX_QUERY_RESULTS;
        let mut results = Vec::new();
        for location in locations.into_iter().take(MAX_QUERY_RESULTS) {
            let visible = location.in_project && self.allows_contents(&location.path, cx).await;
            let mut object = serde_json::Map::new();
            cx.update(|cx| self.path_fields(&location.path, &mut object, cx));
            insert_lines(&mut object, location.lines);
            object.insert("inProject".to_string(), Value::Bool(visible));
            // `ide_query` が空にして返した中身 (`private_files` など) は、読み直して補わない。
            if visible && !location.preview.is_empty() {
                object.insert("preview".to_string(), Value::String(location.preview));
            }
            results.push(Value::Object(object));
        }
        json!({ "locations": results, "truncated": truncated })
    }

    async fn symbols_json(
        &mut self,
        hits: Vec<SymbolHit>,
        limit: usize,
        cx: &mut AsyncApp,
    ) -> Value {
        let truncated = hits.len() > limit;
        let mut results = Vec::new();
        for hit in hits.into_iter().take(limit) {
            let visible = hit.in_project && self.allows_contents(&hit.path, cx).await;
            let mut object = serde_json::Map::new();
            object.insert("name".to_string(), Value::String(hit.name));
            object.insert("kind".to_string(), Value::String(hit.kind));
            if let Some(container) = hit.container {
                object.insert("container".to_string(), Value::String(container));
            }
            cx.update(|cx| self.path_fields(&hit.path, &mut object, cx));
            insert_lines(&mut object, hit.lines);
            object.insert("inProject".to_string(), Value::Bool(visible));
            if visible && !hit.signature.is_empty() {
                object.insert("signature".to_string(), Value::String(hit.signature));
            }
            results.push(Value::Object(object));
        }
        json!({ "symbols": results, "truncated": truncated })
    }

    fn outcome_json(&self, outcome: DisplayOutcome, cx: &mut AsyncApp) -> Value {
        let skipped = outcome
            .skipped
            .into_iter()
            .map(|skipped| {
                let mut object = serde_json::Map::new();
                cx.update(|cx| self.path_fields(&skipped.path, &mut object, cx));
                object.insert("reason".to_string(), Value::String(skipped.reason));
                Value::Object(object)
            })
            .collect::<Vec<_>>();
        json!({ "shown": outcome.shown, "skipped": skipped })
    }
}

/// 読み取り結果の整形だけを試すための入口 (読み取りの関数は `ide_query` が受け持つ)。
#[cfg(test)]
pub(crate) async fn format_locations_for_test(
    project: Entity<Project>,
    paths: AssistPaths<'_>,
    locations: Vec<QueryLocation>,
    cx: &mut AsyncApp,
) -> Value {
    let mut context = AssistContext {
        project,
        paths,
        contents_allowed: HashMap::default(),
    };
    context.locations_json(locations, cx).await
}

#[cfg(test)]
pub(crate) async fn format_symbols_for_test(
    project: Entity<Project>,
    paths: AssistPaths<'_>,
    hits: Vec<SymbolHit>,
    cx: &mut AsyncApp,
) -> Value {
    let mut context = AssistContext {
        project,
        paths,
        contents_allowed: HashMap::default(),
    };
    context.symbols_json(hits, MAX_DOCUMENT_SYMBOLS, cx).await
}

/// `getHover` の結果。本文は [`MAX_HOVER_CHARS`] 文字で切る。
pub(crate) fn hover_json(hover: Option<String>) -> Value {
    let (hover, truncated) = match hover {
        Some(hover) => {
            let (hover, truncated) = truncate_chars(&hover, MAX_HOVER_CHARS);
            (Some(hover), truncated)
        }
        None => (None, false),
    };
    json!({ "hover": hover, "truncated": truncated })
}

/// `searchSymbols` の `limit`。省けば [`DEFAULT_SEARCH_RESULTS`]、多くても [`MAX_QUERY_RESULTS`]。
pub(crate) fn search_limit(arguments: &Value) -> Result<usize> {
    Ok(match arguments.get("limit") {
        None | Some(Value::Null) => DEFAULT_SEARCH_RESULTS,
        Some(value) => value
            .as_u64()
            .filter(|limit| *limit >= 1)
            .map(|limit| usize::try_from(limit).unwrap_or(usize::MAX))
            .ok_or_else(|| client_error("limit は 1 以上の整数にしてください。"))?
            .min(MAX_QUERY_RESULTS),
    })
}

/// `text` を `max_chars` 文字 (文字の境界) までにする。切ったら `true`。
fn truncate_chars(text: &str, max_chars: usize) -> (String, bool) {
    match text.char_indices().nth(max_chars) {
        Some((end, _)) => (text[..end].to_string(), true),
        None => (text.to_string(), false),
    }
}

fn insert_lines(object: &mut serde_json::Map<String, Value>, lines: LineRange) {
    object.insert("startLine".to_string(), json!(lines.start_line));
    object.insert("endLine".to_string(), json!(lines.end_line));
}

/// [`AssistContext::query_failed`] の中身。
pub(crate) fn query_failure(sandboxed: bool, tool: &str, error: anyhow::Error) -> anyhow::Error {
    if error
        .downcast_ref::<ide_query::LanguageServerNotRunning>()
        .is_some()
    {
        return client_error(
            "言語サーバーが起動していません (未インストール・起動中・承認待ち)。\
             起動を待ってから、もう一度呼んでください。",
        );
    }
    if sandboxed {
        log::debug!("ide_server: {tool} failed for a sandboxed client: {error:#}");
        client_error(format!("{error}"))
    } else {
        error.context("言語サーバーから結果を得られませんでした")
    }
}

fn outside_message(text: &str) -> String {
    format!("{text} はワークスペースのフォルダーの外です。")
}

/// `..` を含む相対パスは、ルートの外を指しうるので受けない。
fn has_parent_component(text: &str) -> bool {
    text.split(['/', '\\']).any(|component| component == "..")
}

/// ホストのクライアントのパス: 絶対パスはそのまま、相対パスは見えているワークツリーの
/// どれかのルートから (先頭がルートの名前ならそれも受ける)。
fn host_project_path(project: &Project, text: &str, cx: &App) -> Result<ProjectPath> {
    let path_style = project.path_style(cx);
    let path = path_from_uri(text);
    let path_text = path.to_string_lossy();
    if has_parent_component(&path_text) {
        return Err(client_error(format!(
            "{text} は `..` を含むので扱えません。ルートからのパスで指定してください。"
        )));
    }
    if util::paths::is_absolute(&path_text, path_style) {
        return project
            .find_project_path(&path, cx)
            .ok_or_else(|| client_error(outside_message(text)));
    }
    let relative = RelPath::new(Path::new(path_text.as_ref()), path_style)
        .map_err(|_| client_error(outside_message(text)))?;
    let worktrees = project.visible_worktrees(cx).collect::<Vec<_>>();
    let candidates = worktrees.iter().flat_map(|worktree| {
        let worktree = worktree.read(cx);
        let stripped = relative
            .strip_prefix(RelPath::from_unix_str(worktree.root_name_str()).ok()?)
            .ok()
            .map(RelPath::into_arc);
        Some(
            [Some(relative.as_ref().into_arc()), stripped]
                .into_iter()
                .flatten()
                .map(move |path| ProjectPath {
                    worktree_id: worktree.id(),
                    path,
                }),
        )
    });
    let mut first = None;
    for candidate in candidates.flatten() {
        let exists = project
            .worktree_for_id(candidate.worktree_id, cx)
            .is_some_and(|worktree| worktree.read(cx).entry_for_path(&candidate.path).is_some());
        if exists {
            return Ok(candidate);
        }
        first.get_or_insert(candidate);
    }
    first.ok_or_else(|| client_error("ワークスペースにフォルダーが開かれていません。"))
}

/// サンドボックスのクライアントのパスを、サンドボックスから見た絶対パスにする。相対パスは
/// セッションのルートのどれかから (先頭がルートの名前ならそれも受ける)。
fn sandbox_client_text(
    paths: &util::path_mapping::SandboxPaths,
    project: &Project,
    text: &str,
    cx: &App,
) -> Result<String> {
    if has_parent_component(text) {
        return Err(client_error(format!(
            "{text} は `..` を含むので扱えません。ルートからのパスで指定してください。"
        )));
    }
    if text.starts_with('/') || text.starts_with("file://") {
        return Ok(text.to_string());
    }
    let relative = text.trim_start_matches("./").trim_end_matches('/');
    let mut candidates = Vec::new();
    for root in paths.roots() {
        let Some(client_root) = paths.to_client(root) else {
            continue;
        };
        let client_root = client_root.trim_end_matches('/').to_string();
        candidates.push(format!("{client_root}/{relative}"));
        let root_name = client_root.rsplit('/').next().unwrap_or_default();
        if let Some(rest) = relative
            .strip_prefix(root_name)
            .and_then(|rest| rest.strip_prefix('/'))
            && !root_name.is_empty()
        {
            candidates.push(format!("{client_root}/{rest}"));
        }
    }
    let exists = |candidate: &String| {
        paths
            .to_host(candidate)
            .ok()
            .and_then(|host_path| project.find_project_path(&host_path, cx))
            .is_some_and(|project_path| {
                project
                    .worktree_for_id(project_path.worktree_id, cx)
                    .is_some_and(|worktree| {
                        worktree
                            .read(cx)
                            .entry_for_path(&project_path.path)
                            .is_some()
                    })
            })
    };
    match candidates.iter().find(|candidate| exists(candidate)) {
        Some(candidate) => Ok(candidate.clone()),
        None => candidates
            .into_iter()
            .next()
            .ok_or_else(|| client_error("ワークスペースにフォルダーが開かれていません。")),
    }
}

/// ワークツリーの項目の検査 (ADR-F56): 見えているワークツリーの中・走査に出る・通常の
/// ファイル・リンクを通らない・`private_files` に当たらない・(`check_size` なら) 4 MiB 以下。
/// 断るなら理由 (日本語)。
fn entry_refusal(
    project: &Project,
    project_path: &ProjectPath,
    text: &str,
    check_size: bool,
    cx: &App,
) -> Option<String> {
    let Some(worktree) = project.worktree_for_id(project_path.worktree_id, cx) else {
        return Some(outside_message(text));
    };
    let worktree = worktree.read(cx);
    if !worktree.is_visible() {
        return Some(outside_message(text));
    }
    let link_message = || format!("{text} はリンクを経由しているため扱えません。");
    let goes_through_link = project_path.path.ancestors().skip(1).any(|ancestor| {
        worktree
            .entry_for_path(ancestor)
            .is_some_and(|entry| entry.is_external || entry.canonical_path.is_some())
    });
    if goes_through_link {
        return Some(link_message());
    }
    let Some(entry) = worktree.entry_for_path(&project_path.path) else {
        let in_git_directory = project_path
            .path
            .components()
            .any(|component| component == ".git");
        let excluded = WorktreeSettings::get_global(cx).is_path_excluded(&project_path.path)
            || WorktreeSettings::get(Some(project_path.into()), cx)
                .is_path_excluded(&project_path.path);
        return Some(if in_git_directory || excluded {
            format!("{text} は Zed が走査しない場所 (.git や除外設定の対象) にあるため扱えません。")
        } else {
            format!("{text} は見つかりません。")
        });
    };
    if entry.is_dir() {
        return Some(format!(
            "{text} はディレクトリです。ファイルを指定してください。"
        ));
    }
    if entry.is_external || entry.canonical_path.is_some() {
        return Some(link_message());
    }
    if entry.is_fifo {
        return Some(format!("{text} は通常のファイルではありません。"));
    }
    if is_private_file(project_path, Some(entry), cx) {
        return Some(format!(
            "{text} は private_files の設定 (.env など) に当たるため扱えません。"
        ));
    }
    if check_size && entry.size > MAX_SANDBOX_FILE_BYTES {
        return Some(format!(
            "{text} は {} MiB を超えているため扱えません。",
            MAX_SANDBOX_FILE_BYTES / (1024 * 1024)
        ));
    }
    None
}

fn string_field<'a>(object: &'a Value, key: &str) -> Result<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| client_error(format!("{key} (文字列) が必要です。")))
}

fn optional_string<'a>(object: &'a Value, key: &str) -> Result<Option<&'a str>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(client_error(format!("{key} は文字列にしてください。"))),
    }
}

fn required_text(object: &Value, key: &str, max_chars: usize) -> Result<String> {
    let text = string_field(object, key)?;
    within_limit(text, key, max_chars)
}

fn optional_text(object: &Value, key: &str, max_chars: usize) -> Result<Option<String>> {
    optional_string(object, key)?
        .map(|text| within_limit(text, key, max_chars))
        .transpose()
}

fn within_limit(text: &str, key: &str, max_chars: usize) -> Result<String> {
    let count = text.chars().count();
    if count > max_chars {
        return Err(client_error(format!(
            "{key} は {max_chars} 文字までにしてください ({count} 文字でした)。"
        )));
    }
    Ok(text.to_string())
}

fn array_field<'a>(object: &'a Value, key: &str, max_items: usize) -> Result<&'a [Value]> {
    let items = object
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| client_error(format!("{key} (配列) が必要です。")))?;
    if items.is_empty() {
        return Err(client_error(format!("{key} が空です。")));
    }
    if items.len() > max_items {
        return Err(client_error(format!(
            "{key} は {max_items} 件までにしてください ({} 件でした)。",
            items.len()
        )));
    }
    Ok(items)
}

fn line_field(object: &Value, key: &str) -> Result<u32> {
    object
        .get(key)
        .and_then(Value::as_u64)
        .filter(|line| *line >= 1)
        .and_then(|line| u32::try_from(line).ok())
        .ok_or_else(|| client_error(format!("{key} は 1 以上の整数にしてください。")))
}

fn line_range(item: &Value) -> Result<LineRange> {
    let start_line = line_field(item, "startLine")?;
    let end_line = line_field(item, "endLine")?;
    if end_line < start_line {
        return Err(client_error(
            "endLine は startLine 以上にしてください (行は 1 始まりで、両端を含みます)。",
        ));
    }
    Ok(LineRange {
        start_line,
        end_line,
    })
}
