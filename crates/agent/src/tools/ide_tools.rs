use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol::schema::v1 as acp;
use gpui::{App, AsyncApp, Entity, SharedString, Task};
use ide_query::{
    AssistSource, CodeAnnotation, CodeDisplay, CodeHighlight, CodeTour, DisplayOutcome,
    LanguageServerNotRunning, LinePosition, LineRange, MAX_ANNOTATIONS, MAX_DISPLAY_MARKDOWN_CHARS,
    MAX_HIGHLIGHTS, MAX_QUERY_RESULTS, MAX_TOUR_STEPS, SymbolHit, SymbolSearchOptions, TourStep,
};
use project::{Project, ProjectPath, WorktreeSettings};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::Settings as _;
use util::markdown::MarkdownInlineCode;

use super::deserialize_maybe_stringified;
use super::symbol_locator::{mark_card_missing_language_server, no_language_server_message};
use super::tool_permissions::{
    ResolvedProjectPath, canonicalize_worktree_roots, resolve_project_path,
};
use crate::{AgentTool, ToolCallEventStream, ToolInput};

/// What the frame around this agent's annotations in the editor is labelled with.
pub const ZED_AGENT_ASSIST_LABEL: &str = "Zed Agent";

const DEFAULT_SYMBOL_SEARCH_LIMIT: usize = 30;
/// A hover can carry a whole documentation page; the model rarely needs more than its start.
const MAX_HOVER_OUTPUT_CHARS: usize = 8_000;
const MAX_TITLE_CHARS: usize = 200;
const DISPLAY_UNAVAILABLE: &str = "この環境ではエディターにコードを表示できません";

/// Shows what the language server knows about a symbol: its type, signature and documentation.
///
/// Use it to learn what a name refers to without opening its definition, e.g. the inferred type of a variable or the documentation of a library function.
///
/// This tool needs a running language server for the file's language. When it reports that none is running, tell the user and fall back to `read_file` or `grep`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct HoverToolInput {
    /// The path of the file, starting with a project root directory name (e.g. "my-app/src/main.rs").
    pub path: String,
    /// The 1-based line the symbol is on.
    pub line: u32,
    /// The symbol's text on that line; its first occurrence is used. Omit it to use the line's first non-blank character.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
}

pub struct HoverTool {
    project: Entity<Project>,
}

impl HoverTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for HoverTool {
    type Input = HoverToolInput;
    type Output = String;

    const NAME: &'static str = "hover";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Read
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => hover_title(&input).into(),
            Err(_) => "ホバー情報を取得".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|error| format!("Failed to receive tool input: {error}"))?;
            let path = resolve_shown_paths(&project, [input.path.as_str()], cx)
                .await?
                .pop()
                .ok_or_else(|| format!("`{}` を解決できませんでした", input.path))?;
            let position = LinePosition {
                line: input.line,
                symbol: input.symbol.clone(),
                column: None,
            };
            let hover = cx
                .update(|cx| ide_query::hover(&project, path, position, cx))
                .await;
            let location = format!("{}:{}", input.path, input.line);
            match hover {
                Ok(Some(markdown)) => Ok(format!(
                    "Hover for {} at {location}:\n\n{}",
                    hover_subject(&input),
                    truncate_chars(&markdown, MAX_HOVER_OUTPUT_CHARS)
                )),
                Ok(None) => Ok(format!(
                    "The language server has no hover information for {} at {location}.",
                    hover_subject(&input)
                )),
                Err(error) if error.downcast_ref::<LanguageServerNotRunning>().is_some() => {
                    mark_card_missing_language_server(&event_stream, &hover_title(&input));
                    Ok(no_language_server_message("ホバー情報の取得", &input.path))
                }
                Err(error) => Err(error.to_string()),
            }
        })
    }
}

fn hover_title(input: &HoverToolInput) -> String {
    match &input.symbol {
        Some(symbol) => format!("{} のホバー情報", MarkdownInlineCode(symbol)),
        None => format!(
            "{} の {} 行目のホバー情報",
            MarkdownInlineCode(&input.path),
            input.line
        ),
    }
}

fn hover_subject(input: &HoverToolInput) -> String {
    match &input.symbol {
        Some(symbol) => format!("`{symbol}`"),
        None => "the start of the line".to_string(),
    }
}

/// Searches the project's symbols (functions, types, methods, fields, ...) by name.
///
/// Matches whole names, prefixes and the words inside `snake_case` / `camelCase` names, so "user token" finds `validate_user_token` and `UserTokenStore`; a symbol whose container, path or signature holds the words is found too, ranked lower. It combines the running language servers' workspace symbols with the tree-sitter outline of the files, so it also works in languages without a language server.
///
/// Prefer it over `grep` when you are looking for where something is declared rather than for every place a string occurs.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SearchSymbolsToolInput {
    /// Words or an identifier to look for, e.g. "user token" or "UserTokenStore".
    pub query: String,
    /// Only return symbols under this path, starting with a project root directory name (e.g. "my-app/src/auth").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_prefix: Option<String>,
    /// The maximum number of results. Defaults to 30; at most 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

pub struct SearchSymbolsTool {
    project: Entity<Project>,
}

impl SearchSymbolsTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for SearchSymbolsTool {
    type Input = SearchSymbolsToolInput;
    type Output = String;

    const NAME: &'static str = "search_symbols";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Search
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => format!("シンボルを検索: {}", MarkdownInlineCode(&input.query)).into(),
            Err(_) => "シンボルを検索".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|error| format!("Failed to receive tool input: {error}"))?;
            let limit = input
                .limit
                .unwrap_or(DEFAULT_SYMBOL_SEARCH_LIMIT)
                .clamp(1, MAX_QUERY_RESULTS);
            let options = SymbolSearchOptions {
                limit,
                path_prefix: input.path_prefix.clone(),
            };
            let hits = cx
                .update(|cx| ide_query::search_symbols(&project, &input.query, options, cx))
                .await
                .map_err(|error| error.to_string())?;
            if hits.is_empty() {
                return Ok(format!("No symbols match `{}`.", input.query));
            }
            let mut output = format!("Found {} symbols matching `{}`:\n", hits.len(), input.query);
            project.read_with(cx, |project, cx| {
                for hit in &hits {
                    output.push_str(&format_symbol_hit(project, hit, cx));
                    output.push('\n');
                }
            });
            Ok(output)
        })
    }
}

fn format_symbol_hit(project: &Project, hit: &SymbolHit, cx: &App) -> String {
    let path = display_path(project, &hit.path, hit.in_project, cx);
    let mut line = if hit.lines.start_line == hit.lines.end_line {
        format!("{path}:{}", hit.lines.start_line)
    } else {
        format!("{path}:{}-{}", hit.lines.start_line, hit.lines.end_line)
    };
    write!(line, " {} {}", hit.kind, hit.name).ok();
    if let Some(container) = &hit.container {
        write!(line, " (in {container})").ok();
    }
    if !hit.in_project {
        line.push_str(" (outside the project)");
    } else if !hit.signature.is_empty() && hit.signature != hit.name {
        write!(line, " — {}", hit.signature).ok();
    }
    line
}

fn display_path(project: &Project, path: &ProjectPath, in_project: bool, cx: &App) -> String {
    if !in_project && let Some(abs_path) = project.absolute_path(path, cx) {
        return abs_path.display().to_string();
    }
    let path_style = project.path_style(cx);
    match project.worktree_for_id(path.worktree_id, cx) {
        Some(worktree) => worktree
            .read(cx)
            .root_name()
            .join(&path.path)
            .display(path_style)
            .into_owned(),
        None => path.path.display(path_style).into_owned(),
    }
}

/// Highlights ranges of code in the editor the user is looking at, each with an optional short note.
///
/// Use it when you point the user at specific code ("the check happens here") so they see it instead of only reading a path. Calling it again replaces your previous highlights. For an explanation that moves across several places, use `code_tour`; for longer explanations attached to code, use `annotate_code`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct HighlightCodeToolInput {
    /// The ranges to highlight (at most 50).
    #[serde(deserialize_with = "deserialize_maybe_stringified")]
    pub highlights: Vec<HighlightInput>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct HighlightInput {
    /// The path of the file, starting with a project root directory name (e.g. "my-app/src/main.rs").
    pub path: String,
    /// The 1-based first line to highlight.
    pub start_line: u32,
    /// The 1-based last line to highlight, inclusive. Defaults to `start_line`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    /// A short note shown with the highlight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

pub struct HighlightCodeTool {
    project: Entity<Project>,
    source: AssistSource,
}

impl HighlightCodeTool {
    pub fn new(project: Entity<Project>, source: AssistSource) -> Self {
        Self { project, source }
    }
}

impl AgentTool for HighlightCodeTool {
    type Input = HighlightCodeToolInput;
    type Output = String;

    const NAME: &'static str = "highlight_code";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => format!("コードをハイライト ({} 件)", input.highlights.len()).into(),
            Err(_) => "コードをハイライト".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let project = self.project.clone();
        let source = self.source.clone();
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|error| format!("Failed to receive tool input: {error}"))?;
            let display = require_code_display(cx)?;
            check_count("highlights", input.highlights.len(), MAX_HIGHLIGHTS)?;
            let mut ranges = Vec::with_capacity(input.highlights.len());
            for (index, highlight) in input.highlights.iter().enumerate() {
                let label = format!("highlights[{index}]");
                ranges.push(line_range(
                    highlight.start_line,
                    highlight.end_line,
                    &label,
                )?);
                if let Some(note) = &highlight.note {
                    check_text(note, "note", &label, false)?;
                }
            }
            let paths = resolve_shown_paths(
                &project,
                input
                    .highlights
                    .iter()
                    .map(|highlight| highlight.path.as_str()),
                cx,
            )
            .await?;
            let highlights = input
                .highlights
                .into_iter()
                .zip(paths)
                .zip(ranges)
                .map(|((highlight, path), lines)| CodeHighlight {
                    path,
                    lines,
                    note: highlight.note.filter(|note| !note.trim().is_empty()),
                })
                .collect();
            let outcome = cx
                .update(|cx| display.show_highlights(&project, source, highlights, cx))
                .await
                .map_err(|error| error.to_string())?;
            Ok(describe_outcome(&project, "highlights", &outcome, cx))
        })
    }
}

/// Attaches explanations to lines of code in the editor the user is looking at. Each shows under its code, in a frame marked as your annotation.
///
/// Use it to explain code in place: why a check exists, what a tricky block does. Calling it again replaces your previous annotations, and `clear_annotations` removes them. The text is Markdown without images, links or HTML.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct AnnotateCodeToolInput {
    /// The annotations to show (at most 20).
    #[serde(deserialize_with = "deserialize_maybe_stringified")]
    pub annotations: Vec<AnnotationInput>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct AnnotationInput {
    /// The path of the file, starting with a project root directory name (e.g. "my-app/src/main.rs").
    pub path: String,
    /// The 1-based first line the annotation is about.
    pub start_line: u32,
    /// The 1-based last line the annotation is about, inclusive. Defaults to `start_line`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    /// The explanation, in Markdown (at most 4000 characters).
    pub markdown: String,
}

pub struct AnnotateCodeTool {
    project: Entity<Project>,
    source: AssistSource,
}

impl AnnotateCodeTool {
    pub fn new(project: Entity<Project>, source: AssistSource) -> Self {
        Self { project, source }
    }
}

impl AgentTool for AnnotateCodeTool {
    type Input = AnnotateCodeToolInput;
    type Output = String;

    const NAME: &'static str = "annotate_code";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => format!("コードに注釈 ({} 件)", input.annotations.len()).into(),
            Err(_) => "コードに注釈".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let project = self.project.clone();
        let source = self.source.clone();
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|error| format!("Failed to receive tool input: {error}"))?;
            let display = require_code_display(cx)?;
            check_count("annotations", input.annotations.len(), MAX_ANNOTATIONS)?;
            let mut ranges = Vec::with_capacity(input.annotations.len());
            for (index, annotation) in input.annotations.iter().enumerate() {
                let label = format!("annotations[{index}]");
                ranges.push(line_range(
                    annotation.start_line,
                    annotation.end_line,
                    &label,
                )?);
                check_text(&annotation.markdown, "markdown", &label, true)?;
            }
            let paths = resolve_shown_paths(
                &project,
                input
                    .annotations
                    .iter()
                    .map(|annotation| annotation.path.as_str()),
                cx,
            )
            .await?;
            let annotations = input
                .annotations
                .into_iter()
                .zip(paths)
                .zip(ranges)
                .map(|((annotation, path), lines)| CodeAnnotation {
                    path,
                    lines,
                    markdown: annotation.markdown,
                })
                .collect();
            let outcome = cx
                .update(|cx| display.show_annotations(&project, source, annotations, cx))
                .await
                .map_err(|error| error.to_string())?;
            Ok(describe_outcome(&project, "annotations", &outcome, cx))
        })
    }
}

/// Starts a guided tour through code in the editor the user is looking at. The user steps through your stops one at a time (1/5, 2/5, ...); each stop shows a range of code with your explanation.
///
/// Use it for an explanation that moves across several places: how a request flows through the code, how the parts of a feature fit together. Give the stops in the order they should be read and keep each on one range. Starting a tour replaces your previous one. The text is Markdown without images, links or HTML.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct CodeTourToolInput {
    /// The tour's title.
    pub title: String,
    /// The stops, in order (at most 30).
    #[serde(deserialize_with = "deserialize_maybe_stringified")]
    pub steps: Vec<TourStepInput>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct TourStepInput {
    /// The path of the file, starting with a project root directory name (e.g. "my-app/src/main.rs").
    pub path: String,
    /// The 1-based first line of the stop.
    pub start_line: u32,
    /// The 1-based last line of the stop, inclusive. Defaults to `start_line`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    /// A short title for the stop.
    pub title: String,
    /// The explanation, in Markdown (at most 4000 characters).
    pub markdown: String,
}

pub struct CodeTourTool {
    project: Entity<Project>,
    source: AssistSource,
}

impl CodeTourTool {
    pub fn new(project: Entity<Project>, source: AssistSource) -> Self {
        Self { project, source }
    }
}

impl AgentTool for CodeTourTool {
    type Input = CodeTourToolInput;
    type Output = String;

    const NAME: &'static str = "code_tour";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => format!(
                "コードツアー「{}」({} ステップ)",
                truncate_chars(&input.title, 60),
                input.steps.len()
            )
            .into(),
            Err(_) => "コードツアー".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let project = self.project.clone();
        let source = self.source.clone();
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|error| format!("Failed to receive tool input: {error}"))?;
            let display = require_code_display(cx)?;
            check_title(&input.title, "title")?;
            check_count("steps", input.steps.len(), MAX_TOUR_STEPS)?;
            let mut ranges = Vec::with_capacity(input.steps.len());
            for (index, step) in input.steps.iter().enumerate() {
                let label = format!("steps[{index}]");
                ranges.push(line_range(step.start_line, step.end_line, &label)?);
                check_title(&step.title, &format!("{label}.title"))?;
                check_text(&step.markdown, "markdown", &label, true)?;
            }
            let paths = resolve_shown_paths(
                &project,
                input.steps.iter().map(|step| step.path.as_str()),
                cx,
            )
            .await?;
            let tour = CodeTour {
                title: input.title,
                steps: input
                    .steps
                    .into_iter()
                    .zip(paths)
                    .zip(ranges)
                    .map(|((step, path), lines)| TourStep {
                        path,
                        lines,
                        title: step.title,
                        markdown: step.markdown,
                    })
                    .collect(),
            };
            let outcome = cx
                .update(|cx| display.start_tour(&project, source, tour, cx))
                .await
                .map_err(|error| error.to_string())?;
            Ok(describe_outcome(&project, "tour steps", &outcome, cx))
        })
    }
}

/// Removes the highlights, annotations and tour you showed in the editor.
///
/// Use it when they no longer match the conversation, or when the user asks you to clear them.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ClearAnnotationsToolInput {}

pub struct ClearAnnotationsTool {
    project: Entity<Project>,
    source: AssistSource,
}

impl ClearAnnotationsTool {
    pub fn new(project: Entity<Project>, source: AssistSource) -> Self {
        Self { project, source }
    }
}

impl AgentTool for ClearAnnotationsTool {
    type Input = ClearAnnotationsToolInput;
    type Output = String;

    const NAME: &'static str = "clear_annotations";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "エディターの注釈を消去".into()
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let project = self.project.clone();
        let source = self.source.clone();
        cx.spawn(async move |cx| {
            input
                .recv()
                .await
                .map_err(|error| format!("Failed to receive tool input: {error}"))?;
            let display = require_code_display(cx)?;
            cx.update(|cx| display.clear(&project, &source, cx));
            Ok("Cleared your highlights, annotations and tour.".to_string())
        })
    }
}

fn require_code_display(cx: &mut AsyncApp) -> Result<Arc<dyn CodeDisplay>, String> {
    cx.update(|cx| ide_query::code_display(cx))
        .ok_or_else(|| DISPLAY_UNAVAILABLE.to_string())
}

fn check_count(field: &str, count: usize, max: usize) -> Result<(), String> {
    if count == 0 {
        return Err(format!("{field} が空です。1 件以上指定してください"));
    }
    if count > max {
        return Err(format!("{field} が多すぎます ({count} 件、上限 {max} 件)"));
    }
    Ok(())
}

fn line_range(start_line: u32, end_line: Option<u32>, label: &str) -> Result<LineRange, String> {
    if start_line == 0 {
        return Err(format!("{label}: 行番号は 1 から数えます"));
    }
    let end_line = end_line.unwrap_or(start_line);
    if end_line < start_line {
        return Err(format!(
            "{label}: end_line ({end_line}) が start_line ({start_line}) より前です"
        ));
    }
    Ok(LineRange {
        start_line,
        end_line,
    })
}

fn check_text(text: &str, field: &str, label: &str, required: bool) -> Result<(), String> {
    if required && text.trim().is_empty() {
        return Err(format!("{label}: {field} が空です"));
    }
    let count = text.chars().count();
    if count > MAX_DISPLAY_MARKDOWN_CHARS {
        return Err(format!(
            "{label}: {field} が長すぎます ({count} 文字、上限 {MAX_DISPLAY_MARKDOWN_CHARS} 文字)"
        ));
    }
    Ok(())
}

fn check_title(title: &str, label: &str) -> Result<(), String> {
    if title.trim().is_empty() {
        return Err(format!("{label} が空です"));
    }
    let count = title.chars().count();
    if count > MAX_TITLE_CHARS {
        return Err(format!(
            "{label} が長すぎます ({count} 文字、上限 {MAX_TITLE_CHARS} 文字)"
        ));
    }
    Ok(())
}

/// Resolves paths the model gave to files inside the project whose contents may be shown, with the
/// same checks as `read_file`: inside a visible worktree, not through a link that leaves the
/// project, and neither excluded from scanning nor private.
async fn resolve_shown_paths<'a>(
    project: &Entity<Project>,
    paths: impl IntoIterator<Item = &'a str>,
    cx: &mut AsyncApp,
) -> Result<Vec<ProjectPath>, String> {
    let fs = project.read_with(cx, |project, _| project.fs().clone());
    let canonical_roots = canonicalize_worktree_roots(project, &fs, cx).await;
    project.read_with(cx, |project, cx| {
        paths
            .into_iter()
            .map(|path| resolve_shown_path(project, path, &canonical_roots, cx))
            .collect()
    })
}

fn resolve_shown_path(
    project: &Project,
    path: &str,
    canonical_roots: &[PathBuf],
    cx: &App,
) -> Result<ProjectPath, String> {
    let project_path = match resolve_project_path(project, path, canonical_roots, cx) {
        Ok(ResolvedProjectPath::Safe(project_path)) => project_path,
        Ok(ResolvedProjectPath::SymlinkEscape { .. }) => {
            return Err(format!(
                "`{path}` はプロジェクトの外を指すリンクを通るため表示できません"
            ));
        }
        Err(_) => {
            return Err(format!(
                "`{path}` はプロジェクト内のパスではありません (先頭はプロジェクトのルートフォルダー名にしてください)"
            ));
        }
    };
    let in_visible_worktree = project
        .worktree_for_id(project_path.worktree_id, cx)
        .is_some_and(|worktree| worktree.read(cx).is_visible());
    if !in_visible_worktree {
        return Err(format!("`{path}` はプロジェクト内のパスではありません"));
    }
    let entry = project.entry_for_path(&project_path, cx);
    if entry.is_some_and(|entry| entry.is_dir()) {
        return Err(format!(
            "`{path}` はフォルダーです。ファイルを指定してください"
        ));
    }
    let global_settings = WorktreeSettings::get_global(cx);
    let worktree_settings = WorktreeSettings::get(Some((&project_path).into()), cx);
    if global_settings.is_path_excluded(&project_path.path)
        || worktree_settings.is_path_excluded(&project_path.path)
    {
        return Err(format!(
            "`{path}` は `file_scan_exclusions` に該当するため扱えません"
        ));
    }
    if entry.is_some_and(|entry| entry.is_private)
        || global_settings.is_path_private(&project_path.path)
        || worktree_settings.is_path_private(&project_path.path)
    {
        return Err(format!(
            "`{path}` は `private_files` に該当するため扱えません"
        ));
    }
    Ok(project_path)
}

fn describe_outcome(
    project: &Entity<Project>,
    noun: &str,
    outcome: &DisplayOutcome,
    cx: &mut AsyncApp,
) -> String {
    let mut description = format!("Showed {} {noun} in the editor.", outcome.shown);
    if !outcome.skipped.is_empty() {
        description.push_str("\nNot shown:");
        project.read_with(cx, |project, cx| {
            for skipped in &outcome.skipped {
                let path = display_path(project, &skipped.path, true, cx);
                write!(description, "\n- {path}: {}", skipped.reason).ok();
            }
        });
    }
    description
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((offset, _)) => format!("{}…", &text[..offset]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use ide_query::{GlobalCodeDisplay, SkippedDisplay};
    use parking_lot::Mutex;
    use project::FakeFs;
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;

    #[derive(Debug, PartialEq)]
    enum DisplayCall {
        Highlights(AssistSource, Vec<CodeHighlight>),
        Annotations(AssistSource, Vec<CodeAnnotation>),
        Tour(AssistSource, CodeTour),
        Clear(AssistSource),
    }

    #[derive(Default)]
    struct FakeCodeDisplay {
        calls: Mutex<Vec<DisplayCall>>,
        skip_everything: bool,
    }

    impl FakeCodeDisplay {
        fn outcome(&self, paths: Vec<ProjectPath>) -> Task<anyhow::Result<DisplayOutcome>> {
            let outcome = if self.skip_everything {
                DisplayOutcome {
                    shown: 0,
                    skipped: paths
                        .into_iter()
                        .map(|path| SkippedDisplay {
                            path,
                            reason: "the file has fewer lines".to_string(),
                        })
                        .collect(),
                }
            } else {
                DisplayOutcome {
                    shown: paths.len(),
                    skipped: Vec::new(),
                }
            };
            Task::ready(Ok(outcome))
        }
    }

    impl CodeDisplay for FakeCodeDisplay {
        fn show_highlights(
            &self,
            _project: &Entity<Project>,
            source: AssistSource,
            highlights: Vec<CodeHighlight>,
            _cx: &mut App,
        ) -> Task<anyhow::Result<DisplayOutcome>> {
            let paths = highlights
                .iter()
                .map(|highlight| highlight.path.clone())
                .collect();
            self.calls
                .lock()
                .push(DisplayCall::Highlights(source, highlights));
            self.outcome(paths)
        }

        fn show_annotations(
            &self,
            _project: &Entity<Project>,
            source: AssistSource,
            annotations: Vec<CodeAnnotation>,
            _cx: &mut App,
        ) -> Task<anyhow::Result<DisplayOutcome>> {
            let paths = annotations
                .iter()
                .map(|annotation| annotation.path.clone())
                .collect();
            self.calls
                .lock()
                .push(DisplayCall::Annotations(source, annotations));
            self.outcome(paths)
        }

        fn start_tour(
            &self,
            _project: &Entity<Project>,
            source: AssistSource,
            tour: CodeTour,
            _cx: &mut App,
        ) -> Task<anyhow::Result<DisplayOutcome>> {
            let paths = tour.steps.iter().map(|step| step.path.clone()).collect();
            self.calls.lock().push(DisplayCall::Tour(source, tour));
            self.outcome(paths)
        }

        fn clear(&self, _project: &Entity<Project>, source: &AssistSource, _cx: &mut App) {
            self.calls.lock().push(DisplayCall::Clear(source.clone()));
        }
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    async fn test_project(cx: &mut TestAppContext) -> Entity<Project> {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                ".env": "SECRET=1\n",
                "src": {
                    "main.rs": "fn main() {\n    run();\n}\n",
                    "lib.rs": "pub fn run() {}\n",
                },
            }),
        )
        .await;
        fs.insert_tree(path!("/outside"), json!({"other.rs": "fn other() {}\n"}))
            .await;
        fs.insert_symlink(path!("/root/link"), path!("/outside").into())
            .await;
        let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();
        project
    }

    fn source() -> AssistSource {
        AssistSource {
            id: "thread-1".into(),
            label: ZED_AGENT_ASSIST_LABEL.into(),
        }
    }

    fn install_display(display: FakeCodeDisplay, cx: &mut TestAppContext) -> Arc<FakeCodeDisplay> {
        let display = Arc::new(display);
        cx.update(|cx| cx.set_global(GlobalCodeDisplay(display.clone())));
        display
    }

    async fn run_tool<T: AgentTool<Output = String>>(
        tool: T,
        input: T::Input,
        cx: &mut TestAppContext,
    ) -> Result<String, String> {
        let tool = Arc::new(tool);
        cx.update(|cx| {
            tool.run(
                ToolInput::resolved(input),
                ToolCallEventStream::test().0,
                cx,
            )
        })
        .await
    }

    fn highlight(path: &str, start_line: u32, end_line: Option<u32>) -> HighlightInput {
        HighlightInput {
            path: path.to_string(),
            start_line,
            end_line,
            note: None,
        }
    }

    fn project_path(project: &Entity<Project>, path: &str, cx: &TestAppContext) -> ProjectPath {
        project.read_with(cx, |project, cx| {
            project
                .find_project_path(path, cx)
                .expect("the path is in the project")
        })
    }

    /// Read-only and display-only, like `read_file` and `grep`, so every built-in profile that has
    /// those has these; `minimal` has no tools at all.
    #[gpui::test]
    fn test_ide_tools_are_in_the_built_in_profiles(cx: &mut App) {
        use agent_settings::{AgentProfileId, AgentSettings, builtin_profiles};
        use settings::Settings as _;

        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        project::DisableAiSettings::register(cx);
        AgentSettings::register(cx);

        let ide_tools = [
            HoverTool::NAME,
            SearchSymbolsTool::NAME,
            HighlightCodeTool::NAME,
            AnnotateCodeTool::NAME,
            CodeTourTool::NAME,
            ClearAnnotationsTool::NAME,
        ];
        let profiles = &AgentSettings::get_global(cx).profiles;
        for profile_id in [
            builtin_profiles::WRITE,
            builtin_profiles::ASK,
            builtin_profiles::EDIT,
            builtin_profiles::MINIMAL,
        ] {
            let profile = profiles
                .get(&AgentProfileId(profile_id.into()))
                .expect("the profile ships in default.json");
            let expected = profile_id != builtin_profiles::MINIMAL;
            for tool in ide_tools {
                assert_eq!(
                    profile.is_tool_enabled(tool),
                    expected,
                    "{tool} in the {profile_id} profile"
                );
            }
        }
    }

    #[gpui::test]
    async fn test_display_tools_hand_validated_input_to_the_display(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;
        let display = install_display(FakeCodeDisplay::default(), cx);
        let main_rs = project_path(&project, "root/src/main.rs", cx);
        let lib_rs = project_path(&project, "root/src/lib.rs", cx);

        let output = run_tool(
            HighlightCodeTool::new(project.clone(), source()),
            HighlightCodeToolInput {
                highlights: vec![
                    HighlightInput {
                        note: Some("入口".to_string()),
                        ..highlight("root/src/main.rs", 1, Some(3))
                    },
                    HighlightInput {
                        note: Some("  ".to_string()),
                        ..highlight("root/src/lib.rs", 1, None)
                    },
                ],
            },
            cx,
        )
        .await
        .expect("the highlights are shown");
        assert_eq!(output, "Showed 2 highlights in the editor.");

        run_tool(
            AnnotateCodeTool::new(project.clone(), source()),
            AnnotateCodeToolInput {
                annotations: vec![AnnotationInput {
                    path: "root/src/lib.rs".to_string(),
                    start_line: 1,
                    end_line: None,
                    markdown: "**呼ばれる側**".to_string(),
                }],
            },
            cx,
        )
        .await
        .expect("the annotation is shown");

        run_tool(
            CodeTourTool::new(project.clone(), source()),
            CodeTourToolInput {
                title: "起動の流れ".to_string(),
                steps: vec![
                    TourStepInput {
                        path: "root/src/main.rs".to_string(),
                        start_line: 2,
                        end_line: None,
                        title: "呼び出し".to_string(),
                        markdown: "`run` を呼ぶ".to_string(),
                    },
                    TourStepInput {
                        path: "root/src/lib.rs".to_string(),
                        start_line: 1,
                        end_line: Some(1),
                        title: "本体".to_string(),
                        markdown: "何もしない".to_string(),
                    },
                ],
            },
            cx,
        )
        .await
        .expect("the tour starts");

        run_tool(
            ClearAnnotationsTool::new(project.clone(), source()),
            ClearAnnotationsToolInput {},
            cx,
        )
        .await
        .expect("the display is cleared");

        let lines = |start_line, end_line| LineRange {
            start_line,
            end_line,
        };
        assert_eq!(
            *display.calls.lock(),
            [
                DisplayCall::Highlights(
                    source(),
                    vec![
                        CodeHighlight {
                            path: main_rs.clone(),
                            lines: lines(1, 3),
                            note: Some("入口".to_string()),
                        },
                        CodeHighlight {
                            path: lib_rs.clone(),
                            lines: lines(1, 1),
                            note: None,
                        },
                    ]
                ),
                DisplayCall::Annotations(
                    source(),
                    vec![CodeAnnotation {
                        path: lib_rs.clone(),
                        lines: lines(1, 1),
                        markdown: "**呼ばれる側**".to_string(),
                    }]
                ),
                DisplayCall::Tour(
                    source(),
                    CodeTour {
                        title: "起動の流れ".to_string(),
                        steps: vec![
                            TourStep {
                                path: main_rs,
                                lines: lines(2, 2),
                                title: "呼び出し".to_string(),
                                markdown: "`run` を呼ぶ".to_string(),
                            },
                            TourStep {
                                path: lib_rs,
                                lines: lines(1, 1),
                                title: "本体".to_string(),
                                markdown: "何もしない".to_string(),
                            },
                        ],
                    }
                ),
                DisplayCall::Clear(source()),
            ]
        );
    }

    #[gpui::test]
    async fn test_display_tools_refuse_what_they_must_not_show(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;
        let display = install_display(FakeCodeDisplay::default(), cx);

        let cases = [
            (
                vec![highlight(path!("/outside/other.rs"), 1, None)],
                "プロジェクト内のパスではありません",
            ),
            (
                vec![highlight("root/../outside/other.rs", 1, None)],
                "プロジェクト内のパスではありません",
            ),
            (
                vec![highlight("root/link/other.rs", 1, None)],
                "プロジェクトの外を指すリンクを通る",
            ),
            (vec![highlight("root/.env", 1, None)], "`private_files`"),
            (vec![highlight("root/src", 1, None)], "フォルダーです"),
            (
                vec![highlight("root/src/main.rs", 0, None)],
                "行番号は 1 から",
            ),
            (
                vec![highlight("root/src/main.rs", 3, Some(2))],
                "end_line (2) が start_line (3) より前です",
            ),
            (Vec::new(), "highlights が空です"),
            (
                (0..=MAX_HIGHLIGHTS)
                    .map(|_| highlight("root/src/main.rs", 1, None))
                    .collect(),
                "highlights が多すぎます (51 件、上限 50 件)",
            ),
            (
                vec![HighlightInput {
                    note: Some("あ".repeat(MAX_DISPLAY_MARKDOWN_CHARS + 1)),
                    ..highlight("root/src/main.rs", 1, None)
                }],
                "note が長すぎます (4001 文字",
            ),
        ];
        for (highlights, expected) in cases {
            let error = run_tool(
                HighlightCodeTool::new(project.clone(), source()),
                HighlightCodeToolInput {
                    highlights: highlights.clone(),
                },
                cx,
            )
            .await
            .expect_err("the input is refused");
            assert!(error.contains(expected), "{highlights:?}: {error}");
        }

        let error = run_tool(
            AnnotateCodeTool::new(project.clone(), source()),
            AnnotateCodeToolInput {
                annotations: vec![AnnotationInput {
                    path: "root/src/main.rs".to_string(),
                    start_line: 1,
                    end_line: None,
                    markdown: " ".to_string(),
                }],
            },
            cx,
        )
        .await
        .expect_err("an empty annotation is refused");
        assert!(error.contains("markdown が空です"), "{error}");

        let error = run_tool(
            CodeTourTool::new(project.clone(), source()),
            CodeTourToolInput {
                title: "ツアー".to_string(),
                steps: (0..=MAX_TOUR_STEPS)
                    .map(|_| TourStepInput {
                        path: "root/src/main.rs".to_string(),
                        start_line: 1,
                        end_line: None,
                        title: "手順".to_string(),
                        markdown: "説明".to_string(),
                    })
                    .collect(),
            },
            cx,
        )
        .await
        .expect_err("too many steps are refused");
        assert!(error.contains("steps が多すぎます"), "{error}");

        assert!(
            display.calls.lock().is_empty(),
            "nothing refused reaches the display"
        );
    }

    #[gpui::test]
    async fn test_display_tools_report_what_was_not_shown(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;
        install_display(
            FakeCodeDisplay {
                skip_everything: true,
                ..FakeCodeDisplay::default()
            },
            cx,
        );

        let output = run_tool(
            HighlightCodeTool::new(project.clone(), source()),
            HighlightCodeToolInput {
                highlights: vec![highlight("root/src/lib.rs", 40, None)],
            },
            cx,
        )
        .await
        .expect("the display answered");
        let expected_path = util::rel_path::rel_path("root/src/lib.rs")
            .display(project.read_with(cx, |project, cx| project.path_style(cx)))
            .into_owned();
        assert_eq!(
            output,
            format!(
                "Showed 0 highlights in the editor.\nNot shown:\n- {expected_path}: the file has fewer lines"
            )
        );
    }

    /// Small local models often send nested arrays as a JSON string.
    #[test]
    fn test_display_tool_lists_may_arrive_as_json_strings() {
        let input: CodeTourToolInput = serde_json::from_value(json!({
            "title": "流れ",
            "steps": "[{\"path\": \"root/src/main.rs\", \"start_line\": 2, \"title\": \"呼び出し\", \"markdown\": \"説明\"}]",
        }))
        .expect("the stringified steps are parsed");
        assert_eq!(input.steps.len(), 1);
        assert_eq!(input.steps[0].start_line, 2);

        let input: HighlightCodeToolInput = serde_json::from_value(json!({
            "highlights": "[{\"path\": \"root/src/main.rs\", \"start_line\": 1}]",
        }))
        .expect("the stringified highlights are parsed");
        assert_eq!(input.highlights[0].path, "root/src/main.rs");

        let input: AnnotateCodeToolInput = serde_json::from_value(json!({
            "annotations": [{"path": "root/src/main.rs", "start_line": 1, "markdown": "説明"}],
        }))
        .expect("the plain annotations are parsed");
        assert_eq!(input.annotations[0].markdown, "説明");
    }

    #[gpui::test]
    async fn test_display_tools_need_a_display(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;

        let error = run_tool(
            HighlightCodeTool::new(project.clone(), source()),
            HighlightCodeToolInput {
                highlights: vec![highlight("root/src/main.rs", 1, None)],
            },
            cx,
        )
        .await
        .expect_err("there is nowhere to show it");
        assert_eq!(error, DISPLAY_UNAVAILABLE);
        let error = run_tool(
            ClearAnnotationsTool::new(project.clone(), source()),
            ClearAnnotationsToolInput {},
            cx,
        )
        .await
        .expect_err("there is nothing to clear");
        assert_eq!(error, DISPLAY_UNAVAILABLE);
    }

    #[gpui::test]
    async fn test_hover_explains_a_missing_language_server(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;
        project.read_with(cx, |project, _| {
            project.languages().add(language::rust_lang())
        });

        let tool = Arc::new(HoverTool::new(project.clone()));
        let (event_stream, mut receiver) = ToolCallEventStream::test();
        let output = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(HoverToolInput {
                        path: "root/src/lib.rs".to_string(),
                        line: 1,
                        symbol: Some("run".to_string()),
                    }),
                    event_stream,
                    cx,
                )
            })
            .await
            .expect("the missing server is explained, not failed");
        assert!(
            output.contains("言語サーバーが起動していません"),
            "{output}"
        );
        let title = receiver.expect_update_fields().await.title;
        assert!(
            title
                .as_deref()
                .is_some_and(|title| title.contains("言語サーバーなし")),
            "{title:?}"
        );

        let error = run_tool(
            HoverTool::new(project.clone()),
            HoverToolInput {
                path: "root/.env".to_string(),
                line: 1,
                symbol: None,
            },
            cx,
        )
        .await
        .expect_err("private files are refused");
        assert!(error.contains("`private_files`"), "{error}");
    }

    #[gpui::test]
    async fn test_search_symbols_lists_paths_lines_kinds_and_signatures(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;
        project.read_with(cx, |project, _| {
            project.languages().add(language::rust_lang())
        });

        let output = run_tool(
            SearchSymbolsTool::new(project.clone()),
            SearchSymbolsToolInput {
                query: "run".to_string(),
                path_prefix: None,
                limit: None,
            },
            cx,
        )
        .await
        .expect("the search runs");
        let expected_path = util::rel_path::rel_path("root/src/lib.rs")
            .display(project.read_with(cx, |project, cx| project.path_style(cx)))
            .into_owned();
        assert_eq!(
            output,
            format!(
                "Found 1 symbols matching `run`:\n{expected_path}:1 function run — pub fn run\n"
            )
        );

        let output = run_tool(
            SearchSymbolsTool::new(project.clone()),
            SearchSymbolsToolInput {
                query: "nothing_like_this".to_string(),
                path_prefix: None,
                limit: None,
            },
            cx,
        )
        .await
        .expect("the search runs");
        assert_eq!(output, "No symbols match `nothing_like_this`.");
    }
}
