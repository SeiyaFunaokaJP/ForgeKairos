use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol::schema::v1 as acp;
use gpui::{App, AsyncApp, Entity, SharedString, Task};
use ide_query::{
    AssistSource, CallCandidates, CallDirection, CallExpansion, CallHierarchy, CallHierarchyAnswer,
    CallHierarchyNotSupported, CallHierarchyOptions, CallHierarchyTarget, CallNode, CallResolution,
    CallSource, CallWalkStop, CodeAnnotation, CodeDisplay, CodeHighlight, CodeTour, DisplayOutcome,
    IndexBuilding, LanguageServerNotRunning, LinePosition, LineRange, MAX_ANNOTATIONS,
    MAX_CALL_HIERARCHY_REQUESTS, MAX_DISPLAY_MARKDOWN_CHARS, MAX_HIGHLIGHTS, MAX_QUERY_RESULTS,
    MAX_REMOTE_CALL_HIERARCHY_FILES, MAX_TOUR_STEPS, NameIndexUnavailable, SymbolHit,
    SymbolSearchOptions, TourStep,
};
use project::{Project, ProjectPath};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use util::markdown::MarkdownInlineCode;

use super::deserialize_maybe_stringified;
use super::symbol_locator::{mark_card_missing_language_server, no_language_server_message};
use super::tool_permissions::{
    ResolvedProjectPath, canonicalize_worktree_roots, resolve_project_path,
};
use crate::{AgentTool, ToolCallEventStream, ToolInput};

/// What the frame around this agent's annotations in the editor is labelled with.
pub const ZED_AGENT_ASSIST_LABEL: &str = crate::NATIVE_AGENT_DISPLAY_NAME;

const DEFAULT_SYMBOL_SEARCH_LIMIT: usize = 30;
/// A hover can carry a whole documentation page; the model rarely needs more than its start.
const MAX_HOVER_OUTPUT_CHARS: usize = 8_000;
const MAX_TITLE_CHARS: usize = 200;
const DISPLAY_UNAVAILABLE: &str = "Code can't be shown in the editor in this environment";

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
            Err(_) => i18n::t!("Get hover information").into(),
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
                .ok_or_else(|| format!("Couldn't resolve `{}`", input.path))?;
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
                    Ok(no_language_server_message(
                        "get hover information",
                        &input.path,
                    ))
                }
                Err(error) => Err(error.to_string()),
            }
        })
    }
}

fn hover_title(input: &HoverToolInput) -> String {
    match &input.symbol {
        Some(symbol) => i18n::t!("Hover information for {}", MarkdownInlineCode(symbol)),
        None => i18n::t!(
            "Hover information for line {1} of {0}",
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
            Ok(input) => i18n::t!("Search symbols: {}", MarkdownInlineCode(&input.query)).into(),
            Err(_) => i18n::t!("Search symbols").into(),
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

/// Shows which functions call a function (`incoming`) or which functions it calls (`outgoing`), as a tree.
///
/// Use it before `grep` whenever you need the callers or the callees of a function: it follows the real calls instead of every place the name occurs, and it can follow them up to 3 levels. It asks the language server; in C and C++ it also asks a name index, which answers even when no language server runs (its results are marked as name matches).
///
/// Point it at the function: the line where the function is declared or called, with the name as `symbol`. In C and C++ the name alone (`symbol` without `path` and `line`) works too.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct CallHierarchyToolInput {
    /// The path of the file, starting with a project root directory name (e.g. "my-app/src/main.c"). Omit it with `line` to look the function up by its name alone (C and C++ only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The 1-based line where the function's name appears: its declaration or a call to it. Give it with `path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// The function's name. With `path` and `line`, its first occurrence on that line is used (omit it to use the line's first non-blank character). Without `line`, the function is looked up by this name (C and C++ only); if several functions have it, you get them listed to call again with one's `path` and `line`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// `incoming` lists the functions that call it; `outgoing` lists the functions it calls.
    pub direction: CallHierarchyDirection,
    /// How many levels to follow, from 1 to 3. Defaults to 1: only the direct callers or callees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<u32>,
    /// The most calls to list, over all levels. Defaults to 50; at most 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_results: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CallHierarchyDirection {
    Incoming,
    Outgoing,
}

impl From<CallHierarchyDirection> for CallDirection {
    fn from(direction: CallHierarchyDirection) -> Self {
        match direction {
            CallHierarchyDirection::Incoming => CallDirection::Incoming,
            CallHierarchyDirection::Outgoing => CallDirection::Outgoing,
        }
    }
}

/// A tree of 100 calls with their previews stays well under this; it bounds pathological
/// names and paths.
const MAX_CALL_HIERARCHY_OUTPUT_CHARS: usize = 16_000;
/// Call lines listed per function before the rest are only counted.
const MAX_LISTED_CALL_LINES: usize = 10;
/// `ide_query` leaves the name empty for a function in a private or excluded file.
const WITHHELD_CALL_NAME: &str = "(name withheld)";
const NAME_MATCH_MARK: &str = "[name match]";
const CANDIDATE_MARK: &str = "[candidate]";
const MACRO_MARK: &str = "[macro]";

pub struct CallHierarchyTool {
    project: Entity<Project>,
}

impl CallHierarchyTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for CallHierarchyTool {
    type Input = CallHierarchyToolInput;
    type Output = String;

    const NAME: &'static str = "call_hierarchy";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Search
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => call_hierarchy_title(&input).into(),
            Err(_) => i18n::t!("Call hierarchy").into(),
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
            let symbol = input
                .symbol
                .as_deref()
                .map(str::trim)
                .filter(|symbol| !symbol.is_empty());
            // Looking the name up instead would answer about a function the line may not hold.
            if input.line.is_some() && input.path.is_none() {
                return Err(
                    "`line` needs `path`: give the file the line is in, or omit `line` to look the function up by its name as `symbol`."
                        .to_string(),
                );
            }
            let path = match &input.path {
                Some(path) => Some(
                    resolve_shown_paths(&project, [path.as_str()], cx)
                        .await?
                        .pop()
                        .ok_or_else(|| format!("Couldn't resolve `{path}`"))?,
                ),
                None => None,
            };
            let target = match (path, input.line, symbol) {
                (Some(path), Some(line), _) => CallHierarchyTarget::Position {
                    path,
                    position: LinePosition {
                        line,
                        symbol: input.symbol.clone(),
                        column: None,
                    },
                },
                (near, _, Some(symbol)) => CallHierarchyTarget::Name {
                    name: symbol.to_string(),
                    near,
                },
                _ => {
                    return Err(
                        "Give `path` and `line` (with the function's name as `symbol`), or the function's name alone as `symbol`."
                            .to_string(),
                    );
                }
            };
            let by_name = matches!(target, CallHierarchyTarget::Name { .. });
            let options = CallHierarchyOptions {
                direction: input.direction.into(),
                depth: input.depth.map_or(1, |depth| depth as usize),
                max_results: input.max_results.map_or(0, |limit| limit.max(1) as usize),
            };
            let answer = cx
                .update(|cx| ide_query::call_hierarchy(&project, target, options, cx))
                .await;
            match answer {
                Ok(CallHierarchyAnswer::Found(hierarchy)) => {
                    let locations = project.read_with(cx, |project, cx| {
                        call_hierarchy_locations(project, [&hierarchy.root], cx)
                    });
                    event_stream
                        .update_fields(acp::ToolCallUpdateFields::new().locations(locations));
                    Ok(project.read_with(cx, |project, cx| {
                        format_call_hierarchy(project, &hierarchy, cx)
                    }))
                }
                Ok(CallHierarchyAnswer::Candidates(candidates)) => {
                    let locations = project.read_with(cx, |project, cx| {
                        call_hierarchy_locations(project, &candidates.candidates, cx)
                    });
                    event_stream
                        .update_fields(acp::ToolCallUpdateFields::new().locations(locations));
                    Ok(project.read_with(cx, |project, cx| {
                        format_call_candidates(project, &candidates, cx)
                    }))
                }
                Ok(CallHierarchyAnswer::NoFunction { index_building, .. }) => {
                    let mut output = index_notes(index_building, false);
                    if by_name {
                        write!(
                            output,
                            "The name index has no function named `{}`. Check the spelling, or give the `path` and `line` where it is declared or called.",
                            symbol.unwrap_or_default()
                        )
                        .ok();
                    } else {
                        write!(
                            output,
                            "There is no function at {} line {}{}. Give the line where the function is declared or called, and its name as `symbol`.",
                            input.path.as_deref().unwrap_or_default(),
                            input.line.unwrap_or_default(),
                            symbol
                                .map(|symbol| format!(" (looking for `{symbol}`)"))
                                .unwrap_or_default()
                        )
                        .ok();
                    }
                    Ok(output)
                }
                Err(error) if error.downcast_ref::<NameIndexUnavailable>().is_some() => Ok(format!(
                    "`{}` can't be looked up by name alone: this project has no name index (it covers C and C++ files when it is turned on). Give the `path` and `line` where the function is declared or called, with its name as `symbol`.",
                    symbol.unwrap_or_default()
                )),
                Err(error) if error.downcast_ref::<LanguageServerNotRunning>().is_some() => {
                    mark_card_missing_language_server(&event_stream, &call_hierarchy_title(&input));
                    Ok(no_language_server_message(
                        "get the call hierarchy",
                        input.path.as_deref().unwrap_or_default(),
                    ))
                }
                Err(error) if error.downcast_ref::<CallHierarchyNotSupported>().is_some() => {
                    Ok(call_hierarchy_not_supported_message(&input))
                }
                Err(error) => Err(error.to_string()),
            }
        })
    }
}

fn call_hierarchy_title(input: &CallHierarchyToolInput) -> String {
    let subject = match (&input.symbol, &input.path, input.line) {
        (Some(symbol), _, _) => MarkdownInlineCode(symbol).to_string(),
        (None, Some(path), Some(line)) => {
            i18n::t!("line {1} of {0}", MarkdownInlineCode(path), line)
        }
        _ => return i18n::t!("Call hierarchy").to_string(),
    };
    match input.direction {
        CallHierarchyDirection::Incoming => i18n::t!("Callers of {}", subject),
        CallHierarchyDirection::Outgoing => i18n::t!("Calls made by {}", subject),
    }
}

/// Without this the model tends to answer from general knowledge; the next step it can take
/// is a text search.
fn call_hierarchy_not_supported_message(input: &CallHierarchyToolInput) -> String {
    let name = input.symbol.as_deref().unwrap_or("the function");
    format!(
        "Couldn't get the call hierarchy: the language server running for `{}` doesn't support \
         call hierarchy, so this project can't answer it. Changing settings won't fix this. \
         Instead, to find the callers, use `grep` for `{name}(`; to find what it calls, read \
         its body with `read_file`. Tell the user that these are text matches, not resolved calls.",
        input.path.as_deref().unwrap_or_default()
    )
}

fn call_hierarchy_locations<'a>(
    project: &Project,
    roots: impl IntoIterator<Item = &'a CallNode>,
    cx: &App,
) -> Vec<acp::ToolCallLocation> {
    let mut locations = Vec::new();
    let mut pending = roots.into_iter().collect::<Vec<_>>();
    pending.reverse();
    while let Some(node) = pending.pop() {
        if node.in_project
            && node.contents_allowed
            && let Some(abs_path) = project.absolute_path(&node.path, cx)
        {
            locations.push(
                acp::ToolCallLocation::new(abs_path)
                    .line(Some(node.lines.start_line.saturating_sub(1))),
            );
        }
        pending.extend(node.children.iter().rev());
    }
    locations
}

/// The notes that lead the output: how complete the name index is, and whether a language
/// server answered at all.
fn index_notes(index_building: Option<IndexBuilding>, without_language_server: bool) -> String {
    let mut notes = String::new();
    if let Some(building) = index_building {
        writeln!(
            notes,
            "note: the name index is still being built ({}/{} files); results may be incomplete",
            building.done, building.total
        )
        .ok();
    }
    if without_language_server {
        notes.push_str("note: no language server; results come from the name index\n");
    }
    notes
}

fn source_mark(node: &CallNode) -> Option<&'static str> {
    if node.resolution != CallResolution::Definition {
        return None;
    }
    match node.source {
        CallSource::LanguageServer => None,
        CallSource::NameIndex => Some(NAME_MATCH_MARK),
        CallSource::Candidate => Some(CANDIDATE_MARK),
        CallSource::Macro => Some(MACRO_MARK),
    }
}

/// What the marks that appear mean, a line or two; nothing when no mark appears.
fn mark_legend<'a>(nodes: impl IntoIterator<Item = &'a CallNode>) -> String {
    let mut marks = Vec::new();
    let mut pending = nodes.into_iter().collect::<Vec<_>>();
    while let Some(node) = pending.pop() {
        if let Some(mark) = source_mark(node)
            && !marks.contains(&mark)
        {
            marks.push(mark);
        }
        pending.extend(node.children.iter());
    }
    let mut legend = String::new();
    let matched = [NAME_MATCH_MARK, MACRO_MARK]
        .into_iter()
        .filter(|mark| marks.contains(mark))
        .map(|mark| format!("`{mark}`"))
        .collect::<Vec<_>>();
    if !matched.is_empty() {
        writeln!(
            legend,
            "Lines marked {} were found by matching names, not resolved by a language server, so they can be another function of the same name; read the definition to confirm.",
            matched.join(" or ")
        )
        .ok();
    }
    if marks.contains(&CANDIDATE_MARK) {
        writeln!(
            legend,
            "Lines marked `{CANDIDATE_MARK}` are functions stored into the member that an indirect call goes through; any of them may be the one called."
        )
        .ok();
    }
    legend
}

/// The model-facing text: the calls of each level are grouped under their file, so a path is
/// written once per group rather than once per call.
fn format_call_hierarchy(project: &Project, hierarchy: &CallHierarchy, cx: &App) -> String {
    let root = &hierarchy.root;
    let incoming = hierarchy.direction == CallDirection::Incoming;
    let mut output = index_notes(hierarchy.index_building, hierarchy.without_language_server);
    write!(
        output,
        "{} {}{} ({}, {}:{}), {} level{} deep",
        if incoming {
            "Callers of"
        } else {
            "Calls made by"
        },
        if root.name.is_empty() {
            WITHHELD_CALL_NAME.to_string()
        } else {
            format!("`{}`", root.name)
        },
        source_mark(root)
            .map(|mark| format!(" {mark}"))
            .unwrap_or_default(),
        root.kind,
        display_path(project, &root.path, root.in_project, cx),
        root.lines.start_line,
        hierarchy.depth,
        if hierarchy.depth == 1 { "" } else { "s" },
    )
    .ok();
    output.push_str(if incoming {
        ". Each caller is listed under its file as `line name (kind)`; \"calls it on line N\" is where the call is in that file.\n"
    } else {
        ". Each callee is listed under its file as `line name (kind)`; \"called on line N\" is where the call is in the caller's file.\n"
    });
    output.push_str(&mark_legend([root]));
    if !root.contents_allowed {
        output.push_str(
            "Its file is private, excluded or outside the project, so its calls were not followed.\n",
        );
        return output;
    }
    if root.children.is_empty() {
        let sources = if !hierarchy.used_name_index {
            "The language server"
        } else if hierarchy.without_language_server {
            "The name index"
        } else {
            "The language server and the name index"
        };
        writeln!(
            output,
            "{sources} found {}.",
            if incoming {
                "no callers"
            } else {
                "no calls in it"
            }
        )
        .ok();
    }

    let mut lines = Vec::new();
    push_call_lines(project, &root.children, 0, incoming, &mut lines, cx);
    push_capped_lines(&mut output, lines);
    if hierarchy.omitted > 0 {
        writeln!(
            output,
            "{} more calls were not listed (the limit is {} calls; ask for fewer levels or a larger max_results).",
            hierarchy.omitted, hierarchy.max_results
        )
        .ok();
    }
    if hierarchy.index_truncated {
        output.push_str(
            "The name index found more calls of a name than one answer carries, so some callers may be missing.\n",
        );
    }
    if let Some(stop) = hierarchy.stopped {
        let reason = match stop {
            CallWalkStop::RequestLimit if hierarchy.used_name_index => format!(
                "it reached the limit of {MAX_CALL_HIERARCHY_REQUESTS} requests to the language server and the name index"
            ),
            CallWalkStop::RequestLimit => format!(
                "it reached the limit of {MAX_CALL_HIERARCHY_REQUESTS} language server requests"
            ),
            CallWalkStop::ResultLimit => {
                format!("it reached the limit of {} calls", hierarchy.max_results)
            }
            CallWalkStop::TimeLimit => {
                "the language server didn't answer in time (it may still be indexing)".to_string()
            }
            CallWalkStop::RemoteFileLimit => format!(
                "the results reached {MAX_REMOTE_CALL_HIERARCHY_FILES} files on the remote host"
            ),
        };
        writeln!(
            output,
            "Stopped early because {reason}; functions marked (not followed) were not expanded. Call again on one of them to go further."
        )
        .ok();
    }
    output
}

/// A name alone matched several functions: the model picks one and calls again with its place.
fn format_call_candidates(project: &Project, candidates: &CallCandidates, cx: &App) -> String {
    let mut output = index_notes(candidates.index_building, false);
    // The one function of the name lies where its calls can't be followed (`ide_query` lists it
    // instead of walking it); calling again with its place would only be refused.
    if candidates.counts.total() == 1
        && candidates
            .candidates
            .iter()
            .all(|candidate| !candidate.contents_allowed)
    {
        writeln!(
            output,
            "The one function named `{}` is in a file that is private, excluded or outside the project, so its calls can't be followed.",
            candidates.name
        )
        .ok();
    } else {
        let withheld = match candidates.counts.withheld {
            0 => String::new(),
            count => format!(
                " (and {count} more in files that are private, excluded or outside the project, counted only)"
            ),
        };
        writeln!(
            output,
            "{} functions are named `{}`{withheld}; call again with the `path` and `line` of the one you mean. Each is listed under its file as `line name (kind)`.",
            candidates.counts.shown, candidates.name
        )
        .ok();
    }
    output.push_str(&mark_legend(&candidates.candidates));
    let mut lines = Vec::new();
    push_call_lines(project, &candidates.candidates, 0, false, &mut lines, cx);
    push_capped_lines(&mut output, lines);
    let unlisted = candidates
        .counts
        .total()
        .saturating_sub(candidates.candidates.len());
    if unlisted > 0 {
        writeln!(output, "{unlisted} more are not listed.").ok();
    }
    output
}

fn push_capped_lines(output: &mut String, lines: Vec<String>) {
    let mut unwritten = 0;
    for line in lines {
        if unwritten > 0 || output.len() + line.len() + 1 > MAX_CALL_HIERARCHY_OUTPUT_CHARS {
            unwritten += 1;
            continue;
        }
        output.push_str(&line);
        output.push('\n');
    }
    if unwritten > 0 {
        writeln!(
            output,
            "… {unwritten} more lines cut to keep the output under {MAX_CALL_HIERARCHY_OUTPUT_CHARS} characters."
        )
        .ok();
    }
}

fn push_call_lines(
    project: &Project,
    nodes: &[CallNode],
    level: usize,
    incoming: bool,
    lines: &mut Vec<String>,
    cx: &App,
) {
    let indent = "    ".repeat(level);
    let mut current_path = None;
    for node in nodes {
        if current_path != Some(&node.path) {
            lines.push(format!(
                "{indent}{}",
                display_path(project, &node.path, node.in_project, cx)
            ));
            current_path = Some(&node.path);
        }
        let mut line = format!(
            "{indent}  {} {} ({})",
            node.lines.start_line,
            if node.name.is_empty() {
                WITHHELD_CALL_NAME
            } else {
                &node.name
            },
            node.kind
        );
        if !node.call_lines.is_empty() {
            let listed = node
                .call_lines
                .iter()
                .take(MAX_LISTED_CALL_LINES)
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            let plural = if node.call_lines.len() == 1 { "" } else { "s" };
            if incoming {
                write!(line, " calls it on line{plural} {listed}").ok();
            } else {
                write!(line, " called on line{plural} {listed}").ok();
            }
            let rest = node.call_lines.len().saturating_sub(MAX_LISTED_CALL_LINES);
            if rest > 0 {
                write!(line, " and {rest} more").ok();
            }
        }
        if !node.preview.is_empty() {
            write!(line, ": {}", node.preview).ok();
        }
        if let Some(mark) = source_mark(node) {
            write!(line, " {mark}").ok();
        }
        if let Some(counts) = &node.ambiguous {
            write!(line, " (ambiguous: {} definitions", counts.shown).ok();
            if counts.withheld > 0 {
                write!(line, ", and {} in withheld files", counts.withheld).ok();
            }
            line.push(')');
        }
        match &node.resolution {
            CallResolution::Definition => {}
            CallResolution::NoDefinition => line.push_str(" (no definition in project)"),
            CallResolution::Indirect { member } if member.is_empty() => {
                line.push_str(" (indirect via a member)")
            }
            CallResolution::Indirect { member } => {
                write!(line, " (indirect via .{member})").ok();
            }
        }
        if !node.contents_allowed {
            line.push_str(" (private, excluded or outside the project)");
        }
        match node.expansion {
            CallExpansion::Expanded if node.children.is_empty() => {
                line.push_str(if incoming {
                    " (no callers)"
                } else {
                    " (calls nothing)"
                });
            }
            CallExpansion::Expanded | CallExpansion::DepthLimit => {}
            CallExpansion::Recursive => line.push_str(" (recursive)"),
            CallExpansion::Repeated => line.push_str(" (expanded above)"),
            CallExpansion::Withheld | CallExpansion::Stopped => line.push_str(" (not followed)"),
            CallExpansion::Failed => {
                line.push_str(" (not followed: the language server failed to answer)")
            }
        }
        lines.push(line);
        push_call_lines(project, &node.children, level + 1, incoming, lines, cx);
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
            Ok(input) => i18n::tn!(
                input.highlights.len(),
                "Highlight code ({count} place)",
                "Highlight code ({count} places)"
            )
            .into(),
            Err(_) => i18n::t!("Highlight code").into(),
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
            Ok(input) => i18n::tn!(
                input.annotations.len(),
                "Annotate code ({count} note)",
                "Annotate code ({count} notes)"
            )
            .into(),
            Err(_) => i18n::t!("Annotate code").into(),
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
            Ok(input) => {
                let title = truncate_chars(&input.title, 60);
                i18n::tn!(
                    input.steps.len(),
                    "Code tour “{title}” ({count} step)",
                    "Code tour “{title}” ({count} steps)"
                )
                .into()
            }
            Err(_) => i18n::t!("Code tour").into(),
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
        i18n::t!("Clear the editor annotations").into()
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
        return Err(format!("{field} is empty. Give at least one"));
    }
    if count > max {
        return Err(format!("Too many {field} ({count}, at most {max})"));
    }
    Ok(())
}

fn line_range(start_line: u32, end_line: Option<u32>, label: &str) -> Result<LineRange, String> {
    if start_line == 0 {
        return Err(format!("{label}: line numbers start at 1"));
    }
    let end_line = end_line.unwrap_or(start_line);
    if end_line < start_line {
        return Err(format!(
            "{label}: end_line ({end_line}) is before start_line ({start_line})"
        ));
    }
    Ok(LineRange {
        start_line,
        end_line,
    })
}

fn check_text(text: &str, field: &str, label: &str, required: bool) -> Result<(), String> {
    if required && text.trim().is_empty() {
        return Err(format!("{label}: {field} is empty"));
    }
    let count = text.chars().count();
    if count > MAX_DISPLAY_MARKDOWN_CHARS {
        return Err(format!(
            "{label}: {field} is too long ({count} characters, at most {MAX_DISPLAY_MARKDOWN_CHARS})"
        ));
    }
    Ok(())
}

fn check_title(title: &str, label: &str) -> Result<(), String> {
    if title.trim().is_empty() {
        return Err(format!("{label} is empty"));
    }
    let count = title.chars().count();
    if count > MAX_TITLE_CHARS {
        return Err(format!(
            "{label} is too long ({count} characters, at most {MAX_TITLE_CHARS})"
        ));
    }
    Ok(())
}

/// Resolves paths the model gave to files inside the project whose contents may be shown, with the
/// same checks as `read_file`: inside a visible worktree, not through a link that leaves the
/// project, and neither excluded from scanning nor private.
pub(super) async fn resolve_shown_paths<'a>(
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
                "`{path}` can't be shown, because it goes through a link that leads out of the project"
            ));
        }
        Err(_) => {
            return Err(format!(
                "`{path}` isn't a path in the project (start it with the name of a project root folder)"
            ));
        }
    };
    let in_visible_worktree = project
        .worktree_for_id(project_path.worktree_id, cx)
        .is_some_and(|worktree| worktree.read(cx).is_visible());
    if !in_visible_worktree {
        return Err(format!("`{path}` isn't a path in the project"));
    }
    let entry = project.entry_for_path(&project_path, cx);
    if entry.is_some_and(|entry| entry.is_dir()) {
        return Err(format!("`{path}` is a folder. Give a file"));
    }
    if ide_query::matches_file_scan_exclusions(project, &project_path, cx) {
        return Err(format!(
            "`{path}` can't be used, because it matches `file_scan_exclusions`"
        ));
    }
    if ide_query::matches_private_files(project, &project_path, cx) {
        return Err(format!(
            "`{path}` can't be used, because it matches `private_files`"
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
            CallHierarchyTool::NAME,
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
                "isn't a path in the project",
            ),
            (
                vec![highlight("root/../outside/other.rs", 1, None)],
                "isn't a path in the project",
            ),
            (
                vec![highlight("root/link/other.rs", 1, None)],
                "goes through a link that leads out of the project",
            ),
            (vec![highlight("root/.env", 1, None)], "`private_files`"),
            (vec![highlight("root/src", 1, None)], "is a folder"),
            (
                vec![highlight("root/src/main.rs", 0, None)],
                "line numbers start at 1",
            ),
            (
                vec![highlight("root/src/main.rs", 3, Some(2))],
                "end_line (2) is before start_line (3)",
            ),
            (Vec::new(), "highlights is empty"),
            (
                (0..=MAX_HIGHLIGHTS)
                    .map(|_| highlight("root/src/main.rs", 1, None))
                    .collect(),
                "Too many highlights (51, at most 50)",
            ),
            (
                vec![HighlightInput {
                    note: Some("あ".repeat(MAX_DISPLAY_MARKDOWN_CHARS + 1)),
                    ..highlight("root/src/main.rs", 1, None)
                }],
                "note is too long (4001 characters",
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
        assert!(error.contains("markdown is empty"), "{error}");

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
        assert!(error.contains("Too many steps"), "{error}");

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
        assert!(output.contains("no language server is running"), "{output}");
        let title = receiver.expect_update_fields().await.title;
        assert!(
            title
                .as_deref()
                .is_some_and(|title| title.contains("(no language server)")),
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

    fn call_node(
        name: &str,
        path: &ProjectPath,
        lines: (u32, u32),
        call_lines: Vec<u32>,
        preview: &str,
        expansion: CallExpansion,
        children: Vec<CallNode>,
    ) -> CallNode {
        CallNode {
            name: name.to_string(),
            kind: "function".to_string(),
            path: path.clone(),
            lines: LineRange {
                start_line: lines.0,
                end_line: lines.1,
            },
            call_lines,
            in_project: true,
            contents_allowed: true,
            preview: preview.to_string(),
            source: CallSource::LanguageServer,
            ambiguous: None,
            resolution: CallResolution::Definition,
            expansion,
            children,
        }
    }

    /// The text the model reads: stable, grouped by file, with every way a call can stop
    /// being followed spelled out.
    #[gpui::test]
    async fn test_call_hierarchy_output_shape(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;
        let main_rs = project_path(&project, "root/src/main.rs", cx);
        let lib_rs = project_path(&project, "root/src/lib.rs", cx);
        let env = project_path(&project, "root/.env", cx);
        let hierarchy = CallHierarchy {
            direction: CallDirection::Incoming,
            depth: 2,
            max_results: 50,
            root: call_node(
                "run",
                &lib_rs,
                (1, 1),
                Vec::new(),
                "pub fn run() {}",
                CallExpansion::Expanded,
                vec![
                    CallNode {
                        contents_allowed: false,
                        ..call_node(
                            "load",
                            &env,
                            (1, 1),
                            vec![1],
                            "",
                            CallExpansion::Withheld,
                            Vec::new(),
                        )
                    },
                    call_node(
                        "main",
                        &main_rs,
                        (1, 3),
                        vec![2],
                        "run();",
                        CallExpansion::Expanded,
                        vec![
                            call_node(
                                "main",
                                &main_rs,
                                (1, 3),
                                vec![3],
                                "main();",
                                CallExpansion::Recursive,
                                Vec::new(),
                            ),
                            CallNode {
                                source: CallSource::NameIndex,
                                ..call_node(
                                    "start",
                                    &lib_rs,
                                    (5, 9),
                                    (1..=12).collect(),
                                    "run();",
                                    CallExpansion::Stopped,
                                    Vec::new(),
                                )
                            },
                        ],
                    ),
                    call_node(
                        "helper",
                        &main_rs,
                        (5, 6),
                        vec![6],
                        "run();",
                        CallExpansion::Expanded,
                        Vec::new(),
                    ),
                ],
            ),
            omitted: 3,
            stopped: Some(CallWalkStop::RequestLimit),
            used_name_index: false,
            without_language_server: false,
            index_truncated: false,
            index_building: None,
        };

        let output = project.read_with(cx, |project, cx| {
            format_call_hierarchy(project, &hierarchy, cx)
        });
        let path_style = project.read_with(cx, |project, cx| project.path_style(cx));
        let shown = |path: &str| {
            util::rel_path::rel_path(path)
                .display(path_style)
                .into_owned()
        };
        let expected = [
            format!(
                "Callers of `run` (function, {}:1), 2 levels deep. Each caller is listed under its file as `line name (kind)`; \"calls it on line N\" is where the call is in that file.",
                shown("root/src/lib.rs")
            ),
            "Lines marked `[name match]` were found by matching names, not resolved by a language server, so they can be another function of the same name; read the definition to confirm.".to_string(),
            shown("root/.env"),
            "  1 load (function) calls it on line 1 (private, excluded or outside the project) (not followed)".to_string(),
            shown("root/src/main.rs"),
            "  1 main (function) calls it on line 2: run();".to_string(),
            format!("    {}", shown("root/src/main.rs")),
            "      1 main (function) calls it on line 3: main(); (recursive)".to_string(),
            format!("    {}", shown("root/src/lib.rs")),
            "      5 start (function) calls it on lines 1, 2, 3, 4, 5, 6, 7, 8, 9, 10 and 2 more: run(); [name match] (not followed)".to_string(),
            "  5 helper (function) calls it on line 6: run(); (no callers)".to_string(),
            "3 more calls were not listed (the limit is 50 calls; ask for fewer levels or a larger max_results).".to_string(),
            "Stopped early because it reached the limit of 60 language server requests; functions marked (not followed) were not expanded. Call again on one of them to go further.".to_string(),
        ];
        assert_eq!(output, expected.join("\n") + "\n");

        let outgoing = CallHierarchy {
            direction: CallDirection::Outgoing,
            depth: 1,
            omitted: 0,
            stopped: None,
            root: CallNode {
                children: Vec::new(),
                ..hierarchy.root.clone()
            },
            ..hierarchy
        };
        let output = project.read_with(cx, |project, cx| {
            format_call_hierarchy(project, &outgoing, cx)
        });
        assert_eq!(
            output,
            format!(
                "Calls made by `run` (function, {}:1), 1 level deep. Each callee is listed under its file as `line name (kind)`; \"called on line N\" is where the call is in the caller's file.\nThe language server found no calls in it.\n",
                shown("root/src/lib.rs")
            )
        );
    }

    /// The whole text of a long tree stays under the cap, and says that it was cut.
    #[gpui::test]
    async fn test_call_hierarchy_output_is_capped(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;
        let main_rs = project_path(&project, "root/src/main.rs", cx);
        let long_preview = "x".repeat(120);
        let hierarchy = CallHierarchy {
            direction: CallDirection::Incoming,
            depth: 1,
            max_results: 100,
            root: call_node(
                "run",
                &main_rs,
                (1, 1),
                Vec::new(),
                "",
                CallExpansion::Expanded,
                (0..400)
                    .map(|index| {
                        call_node(
                            &format!("caller_{index}"),
                            &main_rs,
                            (index + 1, index + 1),
                            vec![index + 1],
                            &long_preview,
                            CallExpansion::DepthLimit,
                            Vec::new(),
                        )
                    })
                    .collect(),
            ),
            omitted: 0,
            stopped: None,
            used_name_index: false,
            without_language_server: false,
            index_truncated: false,
            index_building: None,
        };
        let output = project.read_with(cx, |project, cx| {
            format_call_hierarchy(project, &hierarchy, cx)
        });
        assert!(
            output.len() < MAX_CALL_HIERARCHY_OUTPUT_CHARS + 200,
            "{}",
            output.len()
        );
        assert!(
            output.contains("more lines cut to keep the output under 16000 characters"),
            "{output}"
        );
    }

    fn lsp_function(name: &str, abs_path: &str, row: u32, end_row: u32) -> lsp::CallHierarchyItem {
        lsp::CallHierarchyItem {
            name: name.to_string(),
            kind: lsp::SymbolKind::FUNCTION,
            tags: None,
            detail: None,
            uri: lsp::Uri::from_file_path(abs_path).expect("an absolute path"),
            range: lsp::Range::new(lsp::Position::new(row, 0), lsp::Position::new(end_row, 1)),
            selection_range: lsp::Range::new(
                lsp::Position::new(row, 0),
                lsp::Position::new(row, 1),
            ),
            data: None,
        }
    }

    async fn start_call_hierarchy_server(
        project: &Entity<Project>,
        supports_call_hierarchy: bool,
        cx: &mut TestAppContext,
    ) -> (
        lsp::FakeLanguageServer,
        project::lsp_store::OpenLspBufferHandle,
    ) {
        use futures::StreamExt as _;

        let language_registry = project.read_with(cx, |project, _| project.languages().clone());
        language_registry.add(language::rust_lang());
        let mut fake_servers = language_registry.register_fake_lsp(
            "Rust",
            language::FakeLspAdapter {
                capabilities: lsp::ServerCapabilities {
                    call_hierarchy_provider: supports_call_hierarchy
                        .then_some(lsp::CallHierarchyServerCapability::Simple(true)),
                    ..lsp::ServerCapabilities::default()
                },
                ..language::FakeLspAdapter::default()
            },
        );
        let (_buffer, registration) = project
            .update(cx, |project, cx| {
                project.open_local_buffer_with_lsp(path!("/root/src/lib.rs"), cx)
            })
            .await
            .expect("the file opens");
        let server = fake_servers.next().await.expect("the server starts");
        cx.executor().run_until_parked();
        (server, registration)
    }

    fn call_hierarchy_input(direction: CallHierarchyDirection) -> CallHierarchyToolInput {
        CallHierarchyToolInput {
            path: Some("root/src/lib.rs".to_string()),
            line: Some(1),
            symbol: Some("run".to_string()),
            direction,
            depth: None,
            max_results: None,
        }
    }

    /// A private file the language server leads to is placed but neither named nor quoted, a
    /// file behind a link out of the project is named but not quoted, and the card only links
    /// to what may be shown.
    #[gpui::test]
    async fn test_call_hierarchy_tool_lists_the_callers(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;
        let (server, _registration) = start_call_hierarchy_server(&project, true, cx).await;
        server.set_request_handler::<lsp::request::CallHierarchyPrepare, _, _>(|_, _| async {
            Ok(Some(vec![lsp_function(
                "run",
                path!("/root/src/lib.rs"),
                0,
                0,
            )]))
        });
        server.set_request_handler::<lsp::request::CallHierarchyIncomingCalls, _, _>(
            |_, _| async {
                Ok(Some(vec![
                    lsp::CallHierarchyIncomingCall {
                        from: lsp_function("main", path!("/root/src/main.rs"), 0, 2),
                        from_ranges: vec![lsp::Range::new(
                            lsp::Position::new(1, 4),
                            lsp::Position::new(1, 7),
                        )],
                    },
                    lsp::CallHierarchyIncomingCall {
                        from: lsp_function("load_secret", path!("/root/.env"), 0, 0),
                        from_ranges: vec![lsp::Range::new(
                            lsp::Position::new(0, 0),
                            lsp::Position::new(0, 6),
                        )],
                    },
                    lsp::CallHierarchyIncomingCall {
                        from: lsp_function("other", path!("/root/link/other.rs"), 0, 0),
                        from_ranges: vec![lsp::Range::new(
                            lsp::Position::new(0, 3),
                            lsp::Position::new(0, 8),
                        )],
                    },
                ]))
            },
        );

        let tool = Arc::new(CallHierarchyTool::new(project.clone()));
        let (event_stream, mut receiver) = ToolCallEventStream::test();
        let output = cx
            .update(|cx| {
                tool.clone().run(
                    ToolInput::resolved(call_hierarchy_input(CallHierarchyDirection::Incoming)),
                    event_stream,
                    cx,
                )
            })
            .await
            .expect("the callers are listed");
        let path_style = project.read_with(cx, |project, cx| project.path_style(cx));
        let shown = |path: &str| {
            util::rel_path::rel_path(path)
                .display(path_style)
                .into_owned()
        };
        assert!(
            output.contains(&format!(
                "{}\n  1 (name withheld) (function) calls it on line 1 (private, excluded or outside the project)\n",
                shown("root/.env"),
            )),
            "{output}"
        );
        assert!(
            output.contains(&format!(
                "{}\n  1 main (function) calls it on line 2: run();\n",
                shown("root/src/main.rs")
            )),
            "{output}"
        );
        assert!(
            output.contains(
                "  1 other (function) calls it on line 1 (private, excluded or outside the project)\n"
            ),
            "{output}"
        );
        assert!(!output.contains("SECRET"), "{output}");
        assert!(!output.contains("load_secret"), "{output}");
        assert!(!output.contains("fn other"), "{output}");

        let locations = receiver
            .expect_update_fields()
            .await
            .locations
            .expect("the card links to the results");
        let linked = locations
            .iter()
            .map(|location| (location.path.clone(), location.line))
            .collect::<Vec<_>>();
        assert_eq!(
            linked,
            [
                (PathBuf::from(path!("/root/src/lib.rs")), Some(0)),
                (PathBuf::from(path!("/root/src/main.rs")), Some(0)),
            ]
        );

        let title = cx.update(|cx| {
            tool.initial_title(
                Ok(call_hierarchy_input(CallHierarchyDirection::Outgoing)),
                cx,
            )
        });
        assert_eq!(title.as_ref(), "Calls made by `run`");
    }

    #[gpui::test]
    async fn test_call_hierarchy_tool_gives_a_next_step_when_it_cannot_answer(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let project = test_project(cx).await;

        let tool = Arc::new(CallHierarchyTool::new(project.clone()));
        let (event_stream, mut receiver) = ToolCallEventStream::test();
        let output = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(call_hierarchy_input(CallHierarchyDirection::Incoming)),
                    event_stream,
                    cx,
                )
            })
            .await
            .expect("the missing server is explained, not failed");
        assert!(output.contains("no language server is running"), "{output}");
        let title = receiver.expect_update_fields().await.title;
        assert_eq!(
            title.as_deref(),
            Some("Callers of `run` (no language server)")
        );

        let project = test_project(cx).await;
        let (_server, _registration) = start_call_hierarchy_server(&project, false, cx).await;
        let output = run_tool(
            CallHierarchyTool::new(project.clone()),
            call_hierarchy_input(CallHierarchyDirection::Incoming),
            cx,
        )
        .await
        .expect("the unsupported request is explained, not failed");
        assert!(
            output.contains("doesn't support call hierarchy")
                && output.contains("use `grep` for `run(`"),
            "{output}"
        );

        let error = run_tool(
            CallHierarchyTool::new(project.clone()),
            CallHierarchyToolInput {
                path: Some("root/.env".to_string()),
                ..call_hierarchy_input(CallHierarchyDirection::Incoming)
            },
            cx,
        )
        .await
        .expect_err("private files are refused");
        assert!(error.contains("`private_files`"), "{error}");
    }

    /// The marks of what the name index found, what they mean, and the notes on the index, all
    /// in the text the model reads.
    #[gpui::test]
    async fn test_call_hierarchy_output_marks_what_the_name_index_found(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;
        let main_rs = project_path(&project, "root/src/main.rs", cx);
        let lib_rs = project_path(&project, "root/src/lib.rs", cx);
        let hierarchy = CallHierarchy {
            direction: CallDirection::Outgoing,
            depth: 1,
            max_results: 50,
            root: CallNode {
                source: CallSource::NameIndex,
                ..call_node(
                    "top",
                    &main_rs,
                    (8, 14),
                    Vec::new(),
                    "int top(void) {",
                    CallExpansion::Expanded,
                    vec![
                        CallNode {
                            kind: "macro".to_string(),
                            source: CallSource::Macro,
                            ..call_node(
                                "LOG",
                                &main_rs,
                                (1, 1),
                                vec![11],
                                "#define LOG(text) log_line(text)",
                                CallExpansion::DepthLimit,
                                Vec::new(),
                            )
                        },
                        CallNode {
                            source: CallSource::NameIndex,
                            ..call_node(
                                "middle",
                                &main_rs,
                                (5, 7),
                                vec![9],
                                "int middle(void) {",
                                CallExpansion::DepthLimit,
                                Vec::new(),
                            )
                        },
                        CallNode {
                            kind: "member call".to_string(),
                            source: CallSource::NameIndex,
                            resolution: CallResolution::Indirect {
                                member: "open".to_string(),
                            },
                            ..call_node(
                                "open",
                                &main_rs,
                                (10, 10),
                                vec![10],
                                "ops->open(1);",
                                CallExpansion::DepthLimit,
                                vec![CallNode {
                                    source: CallSource::Candidate,
                                    ..call_node(
                                        "real_open",
                                        &lib_rs,
                                        (4, 4),
                                        Vec::new(),
                                        ".open = real_open,",
                                        CallExpansion::DepthLimit,
                                        Vec::new(),
                                    )
                                }],
                            )
                        },
                        CallNode {
                            source: CallSource::NameIndex,
                            resolution: CallResolution::NoDefinition,
                            ..call_node(
                                "printf",
                                &main_rs,
                                (12, 12),
                                vec![12],
                                "printf(\"done\");",
                                CallExpansion::DepthLimit,
                                Vec::new(),
                            )
                        },
                        CallNode {
                            source: CallSource::NameIndex,
                            ambiguous: Some(ide_query::DefinitionCounts {
                                shown: 2,
                                withheld: 1,
                                shown_files: Vec::new(),
                            }),
                            ..call_node(
                                "dup",
                                &lib_rs,
                                (7, 7),
                                vec![13],
                                "int dup(void) { return 1; }",
                                CallExpansion::DepthLimit,
                                Vec::new(),
                            )
                        },
                    ],
                )
            },
            omitted: 2,
            stopped: Some(CallWalkStop::RequestLimit),
            used_name_index: true,
            without_language_server: true,
            index_truncated: true,
            index_building: Some(IndexBuilding {
                done: 12_345,
                total: 60_000,
            }),
        };

        let output = project.read_with(cx, |project, cx| {
            format_call_hierarchy(project, &hierarchy, cx)
        });
        let path_style = project.read_with(cx, |project, cx| project.path_style(cx));
        let shown = |path: &str| {
            util::rel_path::rel_path(path)
                .display(path_style)
                .into_owned()
        };
        let expected = [
            "note: the name index is still being built (12345/60000 files); results may be incomplete".to_string(),
            "note: no language server; results come from the name index".to_string(),
            format!(
                "Calls made by `top` [name match] (function, {}:8), 1 level deep. Each callee is listed under its file as `line name (kind)`; \"called on line N\" is where the call is in the caller's file.",
                shown("root/src/main.rs")
            ),
            "Lines marked `[name match]` or `[macro]` were found by matching names, not resolved by a language server, so they can be another function of the same name; read the definition to confirm.".to_string(),
            "Lines marked `[candidate]` are functions stored into the member that an indirect call goes through; any of them may be the one called.".to_string(),
            shown("root/src/main.rs"),
            "  1 LOG (macro) called on line 11: #define LOG(text) log_line(text) [macro]".to_string(),
            "  5 middle (function) called on line 9: int middle(void) { [name match]".to_string(),
            "  10 open (member call) called on line 10: ops->open(1); (indirect via .open)".to_string(),
            format!("    {}", shown("root/src/lib.rs")),
            "      4 real_open (function): .open = real_open, [candidate]".to_string(),
            "  12 printf (function) called on line 12: printf(\"done\"); (no definition in project)".to_string(),
            shown("root/src/lib.rs"),
            "  7 dup (function) called on line 13: int dup(void) { return 1; } [name match] (ambiguous: 2 definitions, and 1 in withheld files)".to_string(),
            "2 more calls were not listed (the limit is 50 calls; ask for fewer levels or a larger max_results).".to_string(),
            "The name index found more calls of a name than one answer carries, so some callers may be missing.".to_string(),
            "Stopped early because it reached the limit of 60 requests to the language server and the name index; functions marked (not followed) were not expanded. Call again on one of them to go further.".to_string(),
        ];
        assert_eq!(output, expected.join("\n") + "\n");

        // Without a mark, no legend; with both sources and no calls, both are named.
        let empty = CallHierarchy {
            root: call_node(
                "top",
                &main_rs,
                (8, 14),
                Vec::new(),
                "int top(void) {",
                CallExpansion::Expanded,
                Vec::new(),
            ),
            omitted: 0,
            stopped: None,
            without_language_server: false,
            index_truncated: false,
            index_building: None,
            ..hierarchy
        };
        let output =
            project.read_with(cx, |project, cx| format_call_hierarchy(project, &empty, cx));
        assert_eq!(
            output,
            format!(
                "Calls made by `top` (function, {}:8), 1 level deep. Each callee is listed under its file as `line name (kind)`; \"called on line N\" is where the call is in the caller's file.\nThe language server and the name index found no calls in it.\n",
                shown("root/src/main.rs")
            )
        );
    }

    #[gpui::test]
    async fn test_call_hierarchy_candidates_output(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;
        let lib_rs = project_path(&project, "root/src/lib.rs", cx);
        let env = project_path(&project, "root/.env", cx);
        let candidates = CallCandidates {
            name: "dup".to_string(),
            candidates: vec![
                CallNode {
                    source: CallSource::NameIndex,
                    ..call_node(
                        "dup",
                        &lib_rs,
                        (7, 7),
                        Vec::new(),
                        "int dup(void) { return 1; }",
                        CallExpansion::DepthLimit,
                        Vec::new(),
                    )
                },
                CallNode {
                    source: CallSource::NameIndex,
                    contents_allowed: false,
                    ..call_node(
                        "",
                        &env,
                        (1, 1),
                        Vec::new(),
                        "",
                        CallExpansion::DepthLimit,
                        Vec::new(),
                    )
                },
            ],
            counts: ide_query::DefinitionCounts {
                shown: 2,
                withheld: 1,
                shown_files: Vec::new(),
            },
            index_building: None,
        };
        let output = project.read_with(cx, |project, cx| {
            format_call_candidates(project, &candidates, cx)
        });
        let path_style = project.read_with(cx, |project, cx| project.path_style(cx));
        let shown = |path: &str| {
            util::rel_path::rel_path(path)
                .display(path_style)
                .into_owned()
        };
        let expected = [
            "2 functions are named `dup` (and 1 more in files that are private, excluded or outside the project, counted only); call again with the `path` and `line` of the one you mean. Each is listed under its file as `line name (kind)`.".to_string(),
            "Lines marked `[name match]` were found by matching names, not resolved by a language server, so they can be another function of the same name; read the definition to confirm.".to_string(),
            shown("root/src/lib.rs"),
            "  7 dup (function): int dup(void) { return 1; } [name match]".to_string(),
            shown("root/.env"),
            "  1 (name withheld) (function) [name match] (private, excluded or outside the project)".to_string(),
            "1 more are not listed.".to_string(),
        ];
        assert_eq!(output, expected.join("\n") + "\n");

        // The only function of the name can't be followed: calling again with its place would
        // only be refused, so the model is not asked to.
        let only_withheld = CallCandidates {
            name: "load".to_string(),
            candidates: candidates.candidates[1..].to_vec(),
            counts: ide_query::DefinitionCounts {
                shown: 0,
                withheld: 1,
                shown_files: Vec::new(),
            },
            index_building: None,
        };
        let output = project.read_with(cx, |project, cx| {
            format_call_candidates(project, &only_withheld, cx)
        });
        let expected = [
            "The one function named `load` is in a file that is private, excluded or outside the project, so its calls can't be followed.".to_string(),
            "Lines marked `[name match]` were found by matching names, not resolved by a language server, so they can be another function of the same name; read the definition to confirm.".to_string(),
            shown("root/.env"),
            "  1 (name withheld) (function) [name match] (private, excluded or outside the project)".to_string(),
        ];
        assert_eq!(output, expected.join("\n") + "\n");
    }

    /// A C function by its name alone, with no language server: the name index answers, and
    /// the card links to what it found.
    #[gpui::test]
    async fn test_call_hierarchy_tool_answers_a_name_from_the_name_index(cx: &mut TestAppContext) {
        const MAIN_C: &str =
            "int middle(void) { return 0; }\nint top(void) {\n    return middle();\n}\n";
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ "src": { "main.c": MAIN_C } }))
            .await;
        let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();
        let worktree_id = project_path(&project, "root/src/main.c", cx).worktree_id;
        let index_directory = tempfile::tempdir().expect("a temporary directory");
        let index = project::symbol_index_store::build_index_for_tests(
            index_directory.path(),
            &[("src/main.c", MAIN_C)],
        )
        .expect("building the index");
        project.update(cx, |project, cx| {
            project.symbol_index_store().update(cx, |store, cx| {
                store.insert_index_for_tests(worktree_id, index, cx)
            })
        });
        cx.executor().run_until_parked();

        let input = CallHierarchyToolInput {
            path: None,
            line: None,
            symbol: Some("middle".to_string()),
            direction: CallHierarchyDirection::Incoming,
            depth: None,
            max_results: None,
        };
        let tool = Arc::new(CallHierarchyTool::new(project.clone()));
        let (event_stream, mut receiver) = ToolCallEventStream::test();
        let output = cx
            .update(|cx| tool.run(ToolInput::resolved(input.clone()), event_stream, cx))
            .await
            .expect("the name index answers");
        let path_style = project.read_with(cx, |project, cx| project.path_style(cx));
        let shown = |path: &str| {
            util::rel_path::rel_path(path)
                .display(path_style)
                .into_owned()
        };
        let expected = [
            "note: no language server; results come from the name index".to_string(),
            format!(
                "Callers of `middle` [name match] (function, {}:1), 1 level deep. Each caller is listed under its file as `line name (kind)`; \"calls it on line N\" is where the call is in that file.",
                shown("root/src/main.c")
            ),
            "Lines marked `[name match]` were found by matching names, not resolved by a language server, so they can be another function of the same name; read the definition to confirm.".to_string(),
            shown("root/src/main.c"),
            "  2 top (function) calls it on line 3: return middle(); [name match]".to_string(),
        ];
        assert_eq!(output, expected.join("\n") + "\n");
        let locations = receiver
            .expect_update_fields()
            .await
            .locations
            .expect("the card links to the results");
        assert_eq!(
            locations
                .iter()
                .map(|location| (location.path.clone(), location.line))
                .collect::<Vec<_>>(),
            [
                (PathBuf::from(path!("/root/src/main.c")), Some(0)),
                (PathBuf::from(path!("/root/src/main.c")), Some(1)),
            ]
        );

        let output = run_tool(
            CallHierarchyTool::new(project.clone()),
            CallHierarchyToolInput {
                symbol: Some("missing".to_string()),
                ..input.clone()
            },
            cx,
        )
        .await
        .expect("an unknown name is explained");
        assert!(
            output.starts_with("The name index has no function named `missing`."),
            "{output}"
        );

        let error = run_tool(
            CallHierarchyTool::new(project.clone()),
            CallHierarchyToolInput {
                symbol: None,
                ..input.clone()
            },
            cx,
        )
        .await
        .expect_err("neither a place nor a name");
        assert!(error.contains("Give `path` and `line`"), "{error}");

        let error = run_tool(
            CallHierarchyTool::new(project.clone()),
            CallHierarchyToolInput {
                line: Some(2),
                ..input.clone()
            },
            cx,
        )
        .await
        .expect_err("a line without its file is not ignored");
        assert!(error.contains("`line` needs `path`"), "{error}");

        let rust_project = test_project(cx).await;
        let output = run_tool(
            CallHierarchyTool::new(rust_project),
            CallHierarchyToolInput {
                symbol: Some("run".to_string()),
                ..input
            },
            cx,
        )
        .await
        .expect("a missing index is explained");
        assert!(
            output.contains("can't be looked up by name alone")
                && output.contains("`path` and `line`"),
            "{output}"
        );
    }

    #[test]
    fn test_call_hierarchy_input_schema() {
        let schema = <CallHierarchyTool as AgentTool>::input_schema().to_value();
        let schema_text = schema.to_string();
        assert!(
            schema_text.contains("\"incoming\"") && schema_text.contains("\"outgoing\""),
            "{schema_text}"
        );
        let required = schema["required"]
            .as_array()
            .expect("the schema lists the required fields");
        assert_eq!(required, &[json!("direction")]);

        let input: CallHierarchyToolInput = serde_json::from_value(json!({
            "path": "root/src/lib.rs",
            "line": 1,
            "direction": "outgoing",
            "depth": 2,
        }))
        .expect("the input parses");
        assert_eq!(input.direction, CallHierarchyDirection::Outgoing);
        assert_eq!(input.depth, Some(2));
        let input: CallHierarchyToolInput = serde_json::from_value(json!({
            "symbol": "vfs_open",
            "direction": "incoming",
        }))
        .expect("a name alone parses");
        assert_eq!(input.path, None);
        assert_eq!(input.line, None);
        assert!(
            serde_json::from_value::<CallHierarchyToolInput>(json!({
                "path": "root/src/lib.rs",
                "line": 1,
                "direction": "sideways",
            }))
            .is_err()
        );
    }
}
