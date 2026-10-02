//! ADR-F56: エージェントに渡す**アシストのツール** (MCP サーバー `zed`、口 `/mcp/zed`)。
//!
//! 表示 (`highlightCode` / `annotateCode` / `startCodeTour` / `clearAnnotations`) は
//! [`ide_query::code_display`] 越しに `ide_assist` が描き、読み取り (`getHover` /
//! `getDocumentSymbols` / `searchSymbols` / `findDefinition` / `findReferences` /
//! `getCallHierarchy`) は [`ide_query`] の関数に任せる。ここが受け持つのは引数の検査・パスの写像・結果の整形だけ。
//!
//! トークンはサンドボックスの中から読めるので、どのツールも「サンドボックスの中の任意の
//! プロセスから呼ばれても無害」を前提にする: 引数のパスには既存の検査 (ワークツリーの中・
//! リンクを通らない・走査に出る・4 MiB 以下) と `private_files` の拒否を掛け、件数・文字数には
//! 上限を置き、読み取り結果の中身 (1 行のプレビュー・シグネチャ) はプロジェクトの中で同じ
//! 検査を通るファイルのものだけを返す。

use std::collections::VecDeque;
use std::path::Path;

use anyhow::{Result, anyhow};
use collections::HashMap;
use context_server::types::Tool;
use gpui::{App, AsyncApp, Entity, WeakEntity};
use ide_query::{
    AssistSource, CallCandidates, CallDirection, CallExpansion, CallHierarchy, CallHierarchyAnswer,
    CallHierarchyOptions, CallHierarchyTarget, CallNode, CallResolution, CallSource, CallWalkStop,
    CodeAnnotation, CodeDisplay, CodeHighlight, CodeTour, DEFAULT_CALL_HIERARCHY_RESULTS,
    DefinitionCounts, DisplayOutcome, IndexBuilding, LinePosition, LineRange, MAX_ANNOTATIONS,
    MAX_CALL_HIERARCHY_DEPTH, MAX_DISPLAY_MARKDOWN_CHARS, MAX_HIGHLIGHTS, MAX_QUERY_RESULTS,
    MAX_TOUR_STEPS, QueryLocation, SymbolHit, SymbolSearchOptions, TourStep,
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

#[path = "type_layout_tool.rs"]
mod type_layout_tool;

/// アシストのツール。モデルからは `mcp__zed__<名>` に見える。
pub const ASSIST_TOOL_NAMES: [&str; 11] = [
    "highlightCode",
    "annotateCode",
    "startCodeTour",
    "clearAnnotations",
    "getHover",
    "getDocumentSymbols",
    "searchSymbols",
    "findDefinition",
    "findReferences",
    "getCallHierarchy",
    "getTypeLayout",
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
pub(crate) const INSTRUCTIONS: &str = branding::concat_const!(
    "You are connected to the ",
    crate::PRODUCT_NAME,
    " editor (a fork of the Zed editor) that the user is looking at.
Use highlightCode / annotateCode to point at code in the user's editor instead of pasting it, and startCodeTour for step-by-step walkthroughs across files (the user steps with ‹ › buttons).
Use getHover / findDefinition / findReferences / getDocumentSymbols / searchSymbols for precise, language-server-backed navigation (searchSymbols is a structural symbol search, not embeddings).
Use getCallHierarchy before grep to find the callers of a function or the functions it calls.
In C and C++ getCallHierarchy also answers from a name index (even without a language server, and for a function name alone); calls with source nameIndex, macro or candidate are name matches, not resolved calls.
Use getTypeLayout to see what a C or C++ struct, union, enum, typedef or macro contains before searching for its header.
Paths may be absolute or relative to a workspace folder; lines are 1-based and inclusive.
Annotations are not saved, and calling a display tool again replaces what you showed before."
);

pub(crate) const ASSIST_UNAVAILABLE_MESSAGE: &str = branding::concat_const!(
    "The IDE tools (zed) are disabled in ",
    crate::PRODUCT_NAME,
    "'s settings."
);
pub(crate) const IDE_SERVER_DISABLED_MESSAGE: &str =
    branding::concat_const!(crate::PRODUCT_NAME, "'s IDE server is disabled.");

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

/// CLI に足す引数: `--mcp-config <JSON>` と、[`ASSIST_TOOL_NAMES`] の全部を事前に許可する
/// `--allowedTools`。
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
            branding::concat_const!(
                "Highlight line ranges in the user's ",
                crate::PRODUCT_NAME,
                " editor, with an optional short note each. Replaces your previous highlights."
            ),
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
            branding::concat_const!(
                "Show Markdown explanations next to line ranges in the user's ",
                crate::PRODUCT_NAME,
                " editor. Replaces your previous annotations. Images, links and HTML are not rendered."
            ),
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
            branding::concat_const!(
                "Start a step-by-step tour across files in the user's ",
                crate::PRODUCT_NAME,
                " editor. The user moves between the steps with ‹ › buttons. Replaces your previous tour."
            ),
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
            branding::concat_const!(
                "Remove every highlight, annotation and tour you showed in the user's ",
                crate::PRODUCT_NAME,
                " editor."
            ),
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
        tool(
            "getCallHierarchy",
            "Get the functions that call a function (incoming) or the functions it calls \
             (outgoing) as a tree, using the language server and, in C and C++, a name index. \
             Point it at the function's name where it is declared or called (path, line and \
             symbol), or in C and C++ give the name alone as symbol. Prefer it over grep for \
             callers and callees.",
            call_hierarchy_schema(),
        ),
        tool(
            "getTypeLayout",
            type_layout_tool::DESCRIPTION,
            type_layout_tool::type_layout_schema(),
        ),
    ]
}

fn call_hierarchy_schema() -> Value {
    let mut schema = position_schema();
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        properties.insert(
            "path".to_string(),
            json!({
                "type": "string",
                "description": format!("{PATH_DESCRIPTION} Omit it with line to look the function up by symbol alone (C and C++)."),
            }),
        );
        properties.insert(
            "line".to_string(),
            line_schema("1-based line where the function's name appears (declared or called). Give it with path."),
        );
        properties.insert(
            "symbol".to_string(),
            text_schema(
                MAX_QUERY_TEXT_CHARS,
                "The function's name. With path and line it picks the column; alone (C and C++) \
                 the function is looked up by name, and several functions of the name come back \
                 as candidates to call again with.",
            ),
        );
        properties.insert(
            "direction".to_string(),
            json!({
                "type": "string",
                "enum": ["incoming", "outgoing"],
                "description": "incoming: the functions that call it. outgoing: the functions it calls.",
            }),
        );
        properties.insert(
            "depth".to_string(),
            json!({
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_CALL_HIERARCHY_DEPTH,
                "description": "Levels to follow (default 1: only direct calls).",
            }),
        );
        properties.insert(
            "maxResults".to_string(),
            json!({
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_QUERY_RESULTS,
                "description": format!("Most calls to list over all levels (default {DEFAULT_CALL_HIERARCHY_RESULTS})."),
            }),
        );
    }
    if let Some(object) = schema.as_object_mut() {
        object.insert("required".to_string(), json!(["direction"]));
    }
    schema
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
        contents_checks: 0,
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
            let (locations, omitted) = if name == "findDefinition" {
                cx.update(|cx| ide_query::definitions(&project, path, position, cx))
                    .await
                    .map(|locations| (locations, 0))
            } else {
                cx.update(|cx| {
                    ide_query::references_reporting_omissions(&project, path, position, cx)
                })
                .await
            }
            .map_err(|error| context.query_failed(name, error))?;
            let mut result = context.locations_json(locations, cx).await;
            // Left out before anything was judged, so the count says nothing of any file.
            if omitted > 0 {
                result["omittedByResponseBound"] = json!(omitted);
            }
            result
        }
        "getCallHierarchy" => get_call_hierarchy(&mut context, &arguments, cx).await?,
        "getTypeLayout" => type_layout_tool::get_type_layout(&mut context, &arguments, cx).await?,
        other => return Err(anyhow!("unknown tool {other}")),
    };
    Ok(vec![json_content(&result)?])
}

fn code_display(cx: &mut AsyncApp) -> Result<std::sync::Arc<dyn CodeDisplay>> {
    cx.update(|cx| ide_query::code_display(cx))
        .ok_or_else(|| client_error("This window can't show anything in the editor."))
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
        return Err(client_error("query is empty."));
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

async fn get_call_hierarchy(
    context: &mut AssistContext<'_>,
    arguments: &Value,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let options = call_hierarchy_options(arguments)?;
    let target = context.call_hierarchy_target(arguments, cx).await?;
    let by_name = matches!(target, CallHierarchyTarget::Name { .. });
    let project = context.project.clone();
    let answer = cx
        .update(|cx| ide_query::call_hierarchy(&project, target, options, cx))
        .await
        .map_err(|error| context.query_failed("getCallHierarchy", error))?;
    Ok(match answer {
        CallHierarchyAnswer::Found(hierarchy) => context.call_hierarchy_json(hierarchy, cx).await,
        CallHierarchyAnswer::Candidates(candidates) => {
            context.call_candidates_json(candidates, cx).await
        }
        CallHierarchyAnswer::NoFunction {
            used_name_index,
            index_building,
        } => json!({
            "root": null,
            "message": if by_name {
                "The name index has no function of that name. Check the spelling, or give the path and line where it is declared or called."
            } else {
                "There is no function at that position. Give the line where the function is declared or called, and its name as symbol."
            },
            "indexStatus": index_status_json(used_name_index, index_building),
        }),
    })
}

/// `getCallHierarchy` の `indexStatus`。索引を使わなかったときは `null`。
fn index_status_json(used_name_index: bool, building: Option<IndexBuilding>) -> Value {
    match (used_name_index, building) {
        (false, _) => Value::Null,
        (true, Some(building)) => json!({
            "state": "building",
            "doneFiles": building.done,
            "totalFiles": building.total,
        }),
        (true, None) => json!({ "state": "ready" }),
    }
}

/// `getCallHierarchy` の 1 つの関数に並べる呼び出し箇所の行の数。残りは件数 (`callLineCount`) だけ。
const MAX_LISTED_CALL_LINES: usize = 20;

/// Files of one name's definitions a sandbox has judged before it counts them; on a remote host
/// each is a round trip.
const MAX_RECOUNTED_DEFINITION_FILES: usize = 20;

/// `getCallHierarchy` の `direction` / `depth` / `maxResults`。深さと件数は上限で切る。
pub(crate) fn call_hierarchy_options(arguments: &Value) -> Result<CallHierarchyOptions> {
    let direction = match string_field(arguments, "direction")? {
        "incoming" => CallDirection::Incoming,
        "outgoing" => CallDirection::Outgoing,
        _ => {
            return Err(client_error(
                "direction must be \"incoming\" or \"outgoing\".",
            ));
        }
    };
    let depth = match arguments.get("depth") {
        None | Some(Value::Null) => 1,
        Some(_) => (line_field(arguments, "depth")? as usize).min(MAX_CALL_HIERARCHY_DEPTH),
    };
    let max_results = match arguments.get("maxResults") {
        None | Some(Value::Null) => DEFAULT_CALL_HIERARCHY_RESULTS,
        Some(_) => (line_field(arguments, "maxResults")? as usize).min(MAX_QUERY_RESULTS),
    };
    Ok(CallHierarchyOptions {
        direction,
        depth,
        max_results,
    })
}

struct AssistContext<'a> {
    project: Entity<Project>,
    paths: AssistPaths<'a>,
    /// 読み取り結果のファイルごとの「中身を見せてよいか」。同じファイルの結果が並ぶので、
    /// 確かめ (ファイルシステム・接続先への問い合わせ) は 1 ファイル 1 度にする。
    contents_allowed: HashMap<ProjectPath, bool>,
    /// Files judged so far; for a remote project in a sandbox, each one is a round trip.
    contents_checks: usize,
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

    /// `getCallHierarchy` の起点: `path` + `line` (+ `symbol` / `column`) の位置か、`symbol` だけの
    /// 名前 (C / C++ の名前の索引で引く)。`path` だけを添えた名前は、そのファイルに近い定義を選ぶ。
    async fn call_hierarchy_target(
        &mut self,
        arguments: &Value,
        cx: &mut AsyncApp,
    ) -> Result<CallHierarchyTarget> {
        let has_line = !matches!(arguments.get("line"), None | Some(Value::Null));
        let path = optional_string(arguments, "path")?;
        if has_line {
            if path.is_none() {
                return Err(client_error("path is required with line."));
            }
            let (path, position) = self.position(arguments, cx).await?;
            return Ok(CallHierarchyTarget::Position { path, position });
        }
        let path = path.filter(|path| !path.trim().is_empty());
        let symbol = optional_text(arguments, "symbol", MAX_QUERY_TEXT_CHARS)?
            .map(|symbol| symbol.trim().to_string())
            .filter(|symbol| !symbol.is_empty())
            .ok_or_else(|| {
                client_error("Give path and line (with the name as symbol), or symbol alone.")
            })?;
        let near = match path {
            Some(path) => Some(self.resolve(path, cx).await?),
            None => None,
        };
        Ok(CallHierarchyTarget::Name { name: symbol, near })
    }

    /// 引数のパス (絶対パス・ルートからの相対パス・`file://`) を、検査を通したうえで
    /// プロジェクトのパスにする。断るときの理由はどれも日本語の短文。
    async fn resolve(&mut self, text: &str, cx: &mut AsyncApp) -> Result<ProjectPath> {
        let text = text.trim();
        if text.is_empty() {
            return Err(client_error("path is empty."));
        }
        if text.chars().count() > MAX_PATH_CHARS {
            return Err(client_error(format!(
                "path must be at most {MAX_PATH_CHARS} characters."
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
                        "{text} goes through a link or can't be checked, so it can't be used."
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
                "pathPrefix must be at most {MAX_PATH_CHARS} characters."
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
        self.contents_checks += 1;
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

    /// サンドボックスのクライアントに、`ide_query` が中身を出してよいとしたプロジェクト内の
    /// ファイルを読めないとき、名前 (関数名・シンボル名) もそのファイルの中身として伏せる。
    fn refuses_names(&self, allowed_by_query: bool, visible: bool) -> bool {
        matches!(self.paths, AssistPaths::Sandbox(_)) && allowed_by_query && !visible
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
            // 名前が問い合わせに一致したこと自体がそのファイルの中身なので、`ide_query` が
            // `private_files` のシンボルを落とすのと同じく、項目ごと出さない。
            if self.refuses_names(hit.in_project, visible) {
                continue;
            }
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

    /// 木に出てくるファイルごとの「中身を見せてよいか」。Only what [`Self::call_node_json`]
    /// will show is judged: a node `ide_query` already withheld stays hidden whatever the
    /// answer, and nothing below a hidden node is shown. On a remote host each file judged is
    /// a round trip.
    async fn visible_paths<'a>(
        &mut self,
        nodes: impl IntoIterator<Item = &'a CallNode>,
        cx: &mut AsyncApp,
    ) -> HashMap<ProjectPath, bool> {
        let mut visible_paths = HashMap::default();
        let mut pending = nodes.into_iter().collect::<VecDeque<_>>();
        while let Some(node) = pending.pop_front() {
            if let Some(counts) = &node.ambiguous {
                self.judge_definition_files(counts, &mut visible_paths, cx)
                    .await;
            }
            if !(node.in_project && node.contents_allowed) {
                continue;
            }
            let visible = self.allows_contents(&node.path, cx).await;
            visible_paths.insert(node.path.clone(), visible);
            if visible {
                pending.extend(node.children.iter());
            }
        }
        visible_paths
    }

    /// A sandbox may not read some of the files `ide_query` counted as shown (outside the
    /// session's roots, behind a link): those definitions are counted as withheld instead.
    /// Past [`MAX_RECOUNTED_DEFINITION_FILES`] files a name's definitions are not judged, so
    /// they count as withheld too.
    async fn judge_definition_files(
        &mut self,
        counts: &DefinitionCounts,
        visible_paths: &mut HashMap<ProjectPath, bool>,
        cx: &mut AsyncApp,
    ) {
        if !self.is_sandboxed() {
            return;
        }
        for (path, _) in counts
            .shown_files
            .iter()
            .take(MAX_RECOUNTED_DEFINITION_FILES)
        {
            if !visible_paths.contains_key(path) {
                let visible = self.allows_contents(path, cx).await;
                visible_paths.insert(path.clone(), visible);
            }
        }
    }

    /// (shown, withheld) for this client.
    fn definition_counts(
        &self,
        counts: &DefinitionCounts,
        visible_paths: &HashMap<ProjectPath, bool>,
    ) -> (usize, usize) {
        if !self.is_sandboxed() {
            return (counts.shown, counts.withheld);
        }
        let shown = counts
            .shown_files
            .iter()
            .take(MAX_RECOUNTED_DEFINITION_FILES)
            .filter(|(path, _)| visible_paths.get(path).copied().unwrap_or(false))
            .map(|(_, count)| count)
            .sum::<usize>();
        (shown, counts.total().saturating_sub(shown))
    }

    /// 呼び出しの木。中身 (プレビュー) と、そこから先の呼び出しは、引数のパスと同じ検査を通る
    /// ファイルのものだけ。`ide_query` が辿った先でも、ここで伏せたファイルの下は出さない。
    async fn call_hierarchy_json(&mut self, hierarchy: CallHierarchy, cx: &mut AsyncApp) -> Value {
        let visible_paths = self.visible_paths([&hierarchy.root], cx).await;
        log::debug!(
            "ide_server: getCallHierarchy judged {} files",
            self.contents_checks
        );
        let root = cx.update(|cx| self.call_node_json(&hierarchy.root, &visible_paths, cx));
        let stopped_early = hierarchy.stopped.map(|stop| match stop {
            CallWalkStop::RequestLimit => "requestLimit",
            CallWalkStop::ResultLimit => "resultLimit",
            CallWalkStop::TimeLimit => "timeLimit",
            CallWalkStop::RemoteFileLimit => "remoteFileLimit",
        });
        json!({
            "direction": match hierarchy.direction {
                CallDirection::Incoming => "incoming",
                CallDirection::Outgoing => "outgoing",
            },
            "depth": hierarchy.depth,
            "root": root,
            "omitted": hierarchy.omitted,
            "stoppedEarly": stopped_early,
            "truncated": hierarchy.omitted > 0
                || stopped_early.is_some()
                || hierarchy.index_truncated,
            "indexStatus": index_status_json(hierarchy.used_name_index, hierarchy.index_building),
            "withoutLanguageServer": hierarchy.without_language_server,
            "indexTruncated": hierarchy.index_truncated,
        })
    }

    /// 名前だけで引いて関数が複数あったとき。どれかの `path` と `line` で呼び直させる。
    async fn call_candidates_json(
        &mut self,
        candidates: CallCandidates,
        cx: &mut AsyncApp,
    ) -> Value {
        let mut visible_paths = self.visible_paths(&candidates.candidates, cx).await;
        self.judge_definition_files(&candidates.counts, &mut visible_paths, cx)
            .await;
        let (shown, withheld) = self.definition_counts(&candidates.counts, &visible_paths);
        let listed = cx.update(|cx| {
            candidates
                .candidates
                .iter()
                .map(|candidate| self.call_node_json(candidate, &visible_paths, cx))
                .collect::<Vec<_>>()
        });
        json!({
            "root": null,
            "candidates": listed,
            "totalCandidates": shown,
            "withheldCandidates": withheld,
            "message": "Several functions have this name (name matches from the name index). Call again with the path and line of the one you mean.",
            "indexStatus": index_status_json(true, candidates.index_building),
        })
    }

    fn call_node_json(
        &self,
        node: &CallNode,
        visible_paths: &HashMap<ProjectPath, bool>,
        cx: &App,
    ) -> Value {
        let visible = node.in_project
            && node.contents_allowed
            && visible_paths.get(&node.path).copied().unwrap_or(false);
        // プロジェクトの中で `ide_query` の検査は通ったが、このクライアント (サンドボックス) には
        // 読めないファイル。関数名もそのファイルの中身なので出さない。ホストのクライアントは
        // そのファイルを読めるので、項目の無いファイル (走査されていない無視ディレクトリ) の
        // 関数名も今まで通り出す。
        let refused_here = self.refuses_names(node.in_project && node.contents_allowed, visible);
        let mut object = serde_json::Map::new();
        // `ide_query` は private・除外のファイルの関数名を空にして返す (名前もファイルの中身)。
        let name = if node.name.is_empty() || refused_here {
            Value::Null
        } else {
            Value::String(node.name.clone())
        };
        object.insert("name".to_string(), name);
        object.insert("kind".to_string(), Value::String(node.kind.clone()));
        self.path_fields(&node.path, &mut object, cx);
        insert_lines(&mut object, node.lines);
        object.insert("inProject".to_string(), Value::Bool(visible));
        if !node.call_lines.is_empty() {
            // 言語サーバーは 1 つの呼び出し元に何件でも呼び出し箇所を返せる。
            let listed = node
                .call_lines
                .iter()
                .take(MAX_LISTED_CALL_LINES)
                .copied()
                .collect::<Vec<_>>();
            object.insert("callLines".to_string(), json!(listed));
            if node.call_lines.len() > MAX_LISTED_CALL_LINES {
                object.insert("callLineCount".to_string(), json!(node.call_lines.len()));
            }
        }
        if visible && !node.preview.is_empty() {
            object.insert("preview".to_string(), Value::String(node.preview.clone()));
        }
        object.insert(
            "source".to_string(),
            json!(match node.source {
                CallSource::LanguageServer => "languageServer",
                CallSource::NameIndex => "nameIndex",
                CallSource::Candidate => "candidate",
                CallSource::Macro => "macro",
            }),
        );
        if let Some(counts) = &node.ambiguous {
            let (shown, withheld) = self.definition_counts(counts, visible_paths);
            object.insert("ambiguous".to_string(), json!(shown));
            if withheld > 0 {
                object.insert("ambiguousWithheld".to_string(), json!(withheld));
            }
        }
        match &node.resolution {
            CallResolution::Definition => {}
            CallResolution::NoDefinition => {
                object.insert("noDefinition".to_string(), Value::Bool(true));
            }
            CallResolution::Indirect { member } => {
                let member = if member.is_empty() || refused_here {
                    Value::Null
                } else {
                    Value::String(member.clone())
                };
                object.insert("indirectVia".to_string(), member);
            }
        }
        let expansion = match node.expansion {
            CallExpansion::Expanded if !visible => "withheld",
            CallExpansion::Expanded => "expanded",
            CallExpansion::DepthLimit => "depthLimit",
            CallExpansion::Recursive => "recursive",
            CallExpansion::Repeated => "repeated",
            CallExpansion::Withheld => "withheld",
            CallExpansion::Stopped => "stopped",
            CallExpansion::Failed => "failed",
        };
        object.insert("expansion".to_string(), json!(expansion));
        if visible && !node.children.is_empty() {
            let calls = node
                .children
                .iter()
                .map(|child| self.call_node_json(child, visible_paths, cx))
                .collect::<Vec<_>>();
            object.insert("calls".to_string(), Value::Array(calls));
        }
        Value::Object(object)
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
        contents_checks: 0,
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
        contents_checks: 0,
    };
    context.symbols_json(hits, MAX_DOCUMENT_SYMBOLS, cx).await
}

#[cfg(test)]
pub(crate) async fn format_call_hierarchy_for_test(
    project: Entity<Project>,
    paths: AssistPaths<'_>,
    hierarchy: CallHierarchy,
    cx: &mut AsyncApp,
) -> Value {
    let mut context = AssistContext {
        project,
        paths,
        contents_allowed: HashMap::default(),
        contents_checks: 0,
    };
    context.call_hierarchy_json(hierarchy, cx).await
}

/// [`format_call_hierarchy_for_test`], with how many files were judged.
#[cfg(test)]
pub(crate) async fn count_call_hierarchy_checks_for_test(
    project: Entity<Project>,
    paths: AssistPaths<'_>,
    hierarchy: CallHierarchy,
    cx: &mut AsyncApp,
) -> (Value, usize) {
    let mut context = AssistContext {
        project,
        paths,
        contents_allowed: HashMap::default(),
        contents_checks: 0,
    };
    let formatted = context.call_hierarchy_json(hierarchy, cx).await;
    (formatted, context.contents_checks)
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
            .ok_or_else(|| client_error("limit must be an integer of 1 or more."))?
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
            "The language server isn't running (it is not installed, is starting, or is \
             waiting for approval). Wait for it to start, then call again.",
        );
    }
    if error
        .downcast_ref::<ide_query::NameIndexUnavailable>()
        .is_some()
    {
        return client_error(
            "This project has no name index (it covers C and C++ files when it is turned on), \
             so a function can't be looked up by name alone. Give the path and line where it is \
             declared or called, with its name as symbol.",
        );
    }
    if error
        .downcast_ref::<ide_query::CallHierarchyNotSupported>()
        .is_some()
    {
        return client_error(
            "The language server doesn't support call hierarchy, so this project can't answer \
             it. To find the callers, grep for `name(`; to find what a function calls, read its \
             body. Say that these are text matches, not resolved calls.",
        );
    }
    if sandboxed {
        log::debug!("ide_server: {tool} failed for a sandboxed client: {error:#}");
        client_error(format!("{error}"))
    } else {
        error.context("Couldn't get a result from the language server")
    }
}

fn outside_message(text: &str) -> String {
    format!("{text} is outside the workspace folders.")
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
            "{text} contains `..`, so it can't be used. Give the path from a root."
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
    first.ok_or_else(|| client_error("No folder is open in the workspace."))
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
            "{text} contains `..`, so it can't be used. Give the path from a root."
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
            .ok_or_else(|| client_error("No folder is open in the workspace.")),
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
    let link_message = || format!("{text} goes through a link, so it can't be used.");
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
            format!(
                "{text} is somewhere {} doesn't scan (.git or an excluded path), so it can't be used.",
                crate::PRODUCT_NAME
            )
        } else {
            format!("{text} wasn't found.")
        });
    };
    if entry.is_dir() {
        return Some(format!("{text} is a directory. Specify a file."));
    }
    if entry.is_external || entry.canonical_path.is_some() {
        return Some(link_message());
    }
    if entry.is_fifo {
        return Some(format!("{text} isn't a regular file."));
    }
    if is_private_file(worktree, project_path, Some(entry), cx) {
        return Some(format!(
            "{text} matches the private_files setting (.env and the like), so it can't be used."
        ));
    }
    if check_size && entry.size > MAX_SANDBOX_FILE_BYTES {
        return Some(format!(
            "{text} is larger than {} MiB, so it can't be used.",
            MAX_SANDBOX_FILE_BYTES / (1024 * 1024)
        ));
    }
    None
}

fn string_field<'a>(object: &'a Value, key: &str) -> Result<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| client_error(format!("{key} (a string) is required.")))
}

fn optional_string<'a>(object: &'a Value, key: &str) -> Result<Option<&'a str>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(client_error(format!("{key} must be a string."))),
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
            "{key} must be at most {max_chars} characters (it was {count})."
        )));
    }
    Ok(text.to_string())
}

fn array_field<'a>(object: &'a Value, key: &str, max_items: usize) -> Result<&'a [Value]> {
    let items = object
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| client_error(format!("{key} (an array) is required.")))?;
    if items.is_empty() {
        return Err(client_error(format!("{key} is empty.")));
    }
    if items.len() > max_items {
        return Err(client_error(format!(
            "{key} must have at most {max_items} items (it had {}).",
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
        .ok_or_else(|| client_error(format!("{key} must be an integer of 1 or more.")))
}

fn line_range(item: &Value) -> Result<LineRange> {
    let start_line = line_field(item, "startLine")?;
    let end_line = line_field(item, "endLine")?;
    if end_line < start_line {
        return Err(client_error(
            "endLine must be at least startLine (lines are 1-based and inclusive).",
        ));
    }
    Ok(LineRange {
        start_line,
        end_line,
    })
}
