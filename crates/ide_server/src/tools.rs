//! IDE が公開する MCP ツール。
//!
//! 名前と引数は Claude Code CLI 2.1.261 が実際に呼ぶ形に合わせてある
//! (`openDiff` / `close_tab` / `closeAllDiffTabs` / `getDiagnostics`)。残りは VSCode /
//! JetBrains 拡張と同じ名前で、`.mcp.json` から繋ぐ他のクライアント向け。
//!
//! CLI はモデルに見せるツールを `mcp__ide__executeCode` と `mcp__ide__getDiagnostics` の
//! 2 つに絞る (それ以外の `mcp__ide__*` は CLI が内部から直接呼ぶ) ので、ここで余分に
//! 公開してもモデルのツール一覧は膨らまない。

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow};
use context_server::types::{
    CallToolParams, Implementation, LATEST_PROTOCOL_VERSION, ProtocolVersion, ServerCapabilities,
    Tool, ToolsCapabilities, VERSION_2024_11_05, VERSION_2025_03_26, VERSION_2025_06_18,
};
use editor::{Editor, MultiBufferOffset, SelectionEffects, scroll::Autoscroll};
use gpui::{App, AsyncApp, Entity, WeakEntity};
use language::{Buffer, DiagnosticSeverity, PointUtf16};
use project::{Project, ProjectPath};
use serde_json::{Value, json};
use workspace::Workspace;

use crate::{IdeServer, IdeServerDiff, IdeServerDiffOutcome};

/// 全体診断で開くバッファーの上限。壊れたワークツリーで固まらないための保険。
const MAX_DIAGNOSTIC_FILES: usize = 500;

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
        json!({
            "selection": {
                "start": { "line": self.start_line, "character": self.start_character },
                "end": { "line": self.end_line, "character": self.end_character },
            },
            "text": self.text,
            "filePath": self.file_path.as_ref().map(|path| path.to_string_lossy().into_owned()),
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

pub fn list_tools_result() -> Value {
    json!({ "tools": tools() })
}

fn text_content(text: impl Into<String>) -> Value {
    json!({ "type": "text", "text": text.into() })
}

fn json_content(value: &Value) -> Result<Value> {
    Ok(text_content(
        serde_json::to_string(value).context("serializing a tool result")?,
    ))
}

pub async fn call_tool(
    server: &Entity<IdeServer>,
    params: Value,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let params: CallToolParams =
        serde_json::from_value(params).context("parsing the tools/call parameters")?;
    let arguments = params.arguments.unwrap_or(Value::Null);
    let workspace = server.read_with(cx, |server, _| server.workspace().clone());
    match run_tool(&params.name, arguments, server, &workspace, cx).await {
        Ok(content) => Ok(json!({ "content": content, "isError": false })),
        Err(error) => Ok(json!({
            "content": [text_content(format!("{error:#}"))],
            "isError": true,
        })),
    }
}

async fn run_tool(
    name: &str,
    arguments: Value,
    server: &Entity<IdeServer>,
    workspace: &WeakEntity<Workspace>,
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
            let diagnostics = collect_diagnostics(workspace, uri.as_deref(), cx).await?;
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
                .ok_or_else(|| anyhow!("close_tab requires a tab_name"))?
                .to_string();
            close_tab(server, workspace, &tab_name, cx)?;
            Ok(vec![text_content("TAB_CLOSED")])
        }
        "closeAllDiffTabs" => {
            let closed = close_all_diff_tabs(server, cx);
            Ok(vec![text_content(format!("CLOSED_{closed}_DIFF_TABS"))])
        }
        "openDiff" => open_diff(&arguments, server, workspace, cx).await,
        "executeCode" => Err(anyhow!(
            "Zed does not implement executeCode; there is no Jupyter kernel attached to this IDE server"
        )),
        other => Err(anyhow!("unknown tool {other}")),
    }
}

fn required_path(arguments: &Value, key: &str) -> Result<PathBuf> {
    let text = arguments
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("the tool requires {key}"))?;
    Ok(path_from_uri(text))
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
    cx: &mut AsyncApp,
) -> Result<Value> {
    let project = workspace.read_with(cx, |workspace, _| workspace.project().clone())?;
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
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(project_path, cx))
            .await;
        let Ok(buffer) = buffer else {
            continue;
        };
        let (path, diagnostics) = buffer.read_with(cx, |buffer, cx| {
            let path = buffer.file().map(|file| file.full_path(cx));
            let snapshot = buffer.snapshot();
            let diagnostics = snapshot
                .diagnostics_in_range::<_, PointUtf16>(
                    text::Point::zero()..snapshot.max_point(),
                    false,
                )
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
                .collect::<Vec<_>>();
            (path, diagnostics)
        });
        if diagnostics.is_empty() {
            continue;
        }
        let Some(path) = path else {
            continue;
        };
        files.push(json!({ "uri": file_uri(&path), "diagnostics": diagnostics }));
    }
    Ok(Value::Array(files))
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

fn close_tab(
    server: &Entity<IdeServer>,
    workspace: &WeakEntity<Workspace>,
    tab_name: &str,
    cx: &mut AsyncApp,
) -> Result<()> {
    let diff = server.update(cx, |server, _| server.take_diff(tab_name));
    if let Some(diff) = diff {
        diff.update(cx, |diff, cx| diff.dismiss(cx));
        return Ok(());
    }

    let targets = workspace.read_with(cx, |workspace, cx| {
        workspace
            .panes()
            .iter()
            .flat_map(|pane| {
                pane.read(cx)
                    .items()
                    .filter(|item| item.tab_content_text(0, cx).as_ref() == tab_name)
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

fn close_all_diff_tabs(server: &Entity<IdeServer>, cx: &mut AsyncApp) -> usize {
    let diffs = server.update(cx, |server, _| server.take_all_diffs());
    let count = diffs.len();
    for diff in diffs {
        diff.update(cx, |diff, cx| diff.dismiss(cx));
    }
    count
}

async fn open_diff(
    arguments: &Value,
    server: &Entity<IdeServer>,
    workspace: &WeakEntity<Workspace>,
    cx: &mut AsyncApp,
) -> Result<Vec<Value>> {
    let old_file_path = required_path(arguments, "old_file_path")?;
    let new_file_path = required_path(arguments, "new_file_path")?;
    let new_file_contents = arguments
        .get("new_file_contents")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("openDiff requires new_file_contents"))?
        .to_string();
    let tab_name = arguments
        .get("tab_name")
        .and_then(Value::as_str)
        .unwrap_or("Zed diff")
        .to_string();

    let (diff, outcome) = IdeServerDiff::open(
        workspace.clone(),
        tab_name.clone(),
        old_file_path,
        new_file_path,
        new_file_contents,
        cx,
    )
    .await?;
    server.update(cx, |server, _| {
        server.register_diff(tab_name.clone(), diff);
    });

    let outcome = outcome.await.unwrap_or(IdeServerDiffOutcome::Rejected);
    server.update(cx, |server, _| {
        server.take_diff(&tab_name);
    });

    Ok(match outcome {
        IdeServerDiffOutcome::Accepted { contents } => {
            vec![text_content("FILE_SAVED"), text_content(contents)]
        }
        IdeServerDiffOutcome::Rejected => vec![text_content("DIFF_REJECTED")],
    })
}
