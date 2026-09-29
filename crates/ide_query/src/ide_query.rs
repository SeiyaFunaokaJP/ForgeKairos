//! UI-free IDE queries (hover, definitions, references, symbols) and the display contract
//! shared by the IDE MCP server (`ide_server`) and Zed's own agent (`agent`). The UI side of the
//! display contract lives in `ide_assist`, which registers itself through [`GlobalCodeDisplay`]
//! so that neither caller has to depend on `editor` or `workspace`.

mod symbol_search;

use std::{fmt, pin::pin, sync::Arc, time::Duration};

use anyhow::{Result, anyhow};
use futures::future::{self, Either};
use gpui::{App, AppContext as _, AsyncApp, Entity, Global, SharedString, Task};
use language::{Buffer, BufferSnapshot, Location, OutlineItem, Point, ToPoint as _};
use project::{DocumentSymbol, Hover, HoverBlockKind, Project, ProjectPath, WorktreeSettings};
use settings::Settings as _;
use util::markdown::MarkdownCodeBlock;

pub const MAX_QUERY_RESULTS: usize = 100;
pub const MAX_HIGHLIGHTS: usize = 50;
pub const MAX_ANNOTATIONS: usize = 20;
pub const MAX_TOUR_STEPS: usize = 30;
pub const MAX_DISPLAY_MARKDOWN_CHARS: usize = 4000;

/// A server that is still indexing can take minutes to answer, and every caller is an agent
/// waiting on a tool call, so a slow answer is reported rather than waited for.
const LANGUAGE_SERVER_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_PREVIEW_CHARS: usize = 300;
const MAX_SIGNATURE_CHARS: usize = 200;
/// Bounds what one call can hand to an agent; a generated file can declare tens of thousands.
const MAX_DOCUMENT_SYMBOLS: usize = 1000;

/// 1-based, inclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LineRange {
    pub start_line: u32,
    pub end_line: u32,
}

/// 1-based line. `symbol` picks the column by finding that text on the line; `column`
/// (1-based, in characters) wins when both are given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinePosition {
    pub line: u32,
    pub symbol: Option<String>,
    pub column: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct QueryLocation {
    pub path: ProjectPath,
    pub lines: LineRange,
    /// False for files outside the visible worktrees (dependency sources the language server
    /// pointed at) or reached through a link that leaves one. Callers must not return their
    /// contents to an agent.
    pub in_project: bool,
    pub preview: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SymbolHit {
    pub name: String,
    pub kind: String,
    pub container: Option<String>,
    pub path: ProjectPath,
    pub lines: LineRange,
    pub signature: String,
    pub in_project: bool,
    pub score: f64,
}

#[derive(Clone, Debug, Default)]
pub struct SymbolSearchOptions {
    /// Zero means [`MAX_QUERY_RESULTS`].
    pub limit: usize,
    /// Project-relative, `/`-separated.
    pub path_prefix: Option<String>,
}

/// What a language query fails with when no language server that could answer is running for
/// the file. Callers find it with `error.downcast_ref::<LanguageServerNotRunning>()`: "there is
/// no server" needs a different next step from "the server found nothing".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LanguageServerNotRunning {
    pub path: String,
}

impl fmt::Display for LanguageServerNotRunning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Language server not running: no language server is running for `{}` \
             (it is not installed, is still starting, or is waiting for approval to start)",
            self.path
        )
    }
}

impl std::error::Error for LanguageServerNotRunning {}

/// Matches the query against the symbols the running language servers know about
/// (`workspace/symbol`) and against the tree-sitter outlines of the project's files, so that
/// languages without a server are searchable too.
pub fn search_symbols(
    project: &Entity<Project>,
    query: &str,
    options: SymbolSearchOptions,
    cx: &mut App,
) -> Task<Result<Vec<SymbolHit>>> {
    symbol_search::search_symbols(project, query, options, cx)
}

pub fn document_symbols(
    project: &Entity<Project>,
    path: ProjectPath,
    cx: &mut App,
) -> Task<Result<Vec<SymbolHit>>> {
    let project = project.clone();
    cx.spawn(async move |cx| {
        let target = open_target(&project, &path, cx).await?;
        let _language_server_registration = project.update(cx, |project, cx| {
            project.register_buffer_with_language_servers(&target.buffer, cx)
        });
        let has_language_server = project.read_with(cx, |project, cx| {
            project.has_language_server_for_buffer(&target.buffer, cx)
        });

        let language_server_symbols = if has_language_server {
            let request = project.update(cx, |project, cx| {
                project.document_symbols(&target.buffer, cx)
            });
            match with_timeout(request, cx).await {
                Ok(Ok(symbols)) => symbols,
                Ok(Err(error)) => {
                    log::warn!(
                        "document symbols for {} failed, using the outline: {error:#}",
                        target.display_path
                    );
                    Vec::new()
                }
                Err(error) => {
                    log::warn!("{error:#}; using the outline of {}", target.display_path);
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };

        target
            .buffer
            .read_with(cx, |buffer, _| buffer.parsing_idle())
            .await;
        let snapshot = target.buffer.read_with(cx, |buffer, _| buffer.snapshot());
        let in_project = target.in_project;
        let symbols = cx
            .background_spawn(async move {
                let outline =
                    snapshot.outline_items_as_points_containing(0..snapshot.len(), true, None);
                let mut symbols = if language_server_symbols.is_empty() {
                    outline_symbol_hits(&outline, &path, in_project)
                } else {
                    language_server_symbol_hits(
                        &language_server_symbols,
                        &snapshot,
                        &outline,
                        &path,
                        in_project,
                    )
                };
                symbols.truncate(MAX_DOCUMENT_SYMBOLS);
                symbols
            })
            .await;
        Ok(symbols)
    })
}

pub fn hover(
    project: &Entity<Project>,
    path: ProjectPath,
    position: LinePosition,
    cx: &mut App,
) -> Task<Result<Option<String>>> {
    let project = project.clone();
    cx.spawn(async move |cx| {
        let target = open_target(&project, &path, cx).await?;
        let point = target.buffer.read_with(cx, |buffer, _| {
            resolve_position(&buffer.snapshot(), &position, &target.display_path)
        })?;
        let _language_server_registration = require_language_server(&project, &target, cx)?;
        let request = project.update(cx, |project, cx| project.hover(&target.buffer, point, cx));
        let hovers = with_timeout(request, cx).await?;
        Ok(hover_markdown(&hovers.unwrap_or_default()))
    })
}

pub fn definitions(
    project: &Entity<Project>,
    path: ProjectPath,
    position: LinePosition,
    cx: &mut App,
) -> Task<Result<Vec<QueryLocation>>> {
    let project = project.clone();
    cx.spawn(async move |cx| {
        let target = open_target(&project, &path, cx).await?;
        let point = target.buffer.read_with(cx, |buffer, _| {
            resolve_position(&buffer.snapshot(), &position, &target.display_path)
        })?;
        let _language_server_registration = require_language_server(&project, &target, cx)?;
        let request = project.update(cx, |project, cx| {
            project.definitions(&target.buffer, point, cx)
        });
        let links = with_timeout(request, cx).await??.unwrap_or_default();
        let locations = links.into_iter().map(|link| link.target).collect();
        Ok(project.read_with(cx, |project, cx| query_locations(project, locations, cx)))
    })
}

pub fn references(
    project: &Entity<Project>,
    path: ProjectPath,
    position: LinePosition,
    cx: &mut App,
) -> Task<Result<Vec<QueryLocation>>> {
    let project = project.clone();
    cx.spawn(async move |cx| {
        let target = open_target(&project, &path, cx).await?;
        let point = target.buffer.read_with(cx, |buffer, _| {
            resolve_position(&buffer.snapshot(), &position, &target.display_path)
        })?;
        let _language_server_registration = require_language_server(&project, &target, cx)?;
        let request = project.update(cx, |project, cx| {
            project.references(&target.buffer, point, cx)
        });
        let locations = with_timeout(request, cx).await??.unwrap_or_default();
        Ok(project.read_with(cx, |project, cx| query_locations(project, locations, cx)))
    })
}

struct QueryTarget {
    buffer: Entity<Buffer>,
    display_path: String,
    in_project: bool,
}

async fn open_target(
    project: &Entity<Project>,
    path: &ProjectPath,
    cx: &mut AsyncApp,
) -> Result<QueryTarget> {
    let (display_path, in_project, withheld) = project.read_with(cx, |project, cx| {
        (
            display_path(project, path, cx),
            is_in_project(project, path, cx),
            withheld_reason(project, path, cx),
        )
    });
    if let Some(reason) = withheld {
        return Err(anyhow!(
            "`{display_path}` can't be used because it {reason}"
        ));
    }
    let buffer = project
        .update(cx, |project, cx| project.open_buffer(path.clone(), cx))
        .await
        .map_err(|error| anyhow!("Couldn't open `{display_path}`: {error:#}"))?;
    Ok(QueryTarget {
        buffer,
        display_path,
        in_project,
    })
}

/// Registering is what opening the file in an editor does, so a server that is not running yet
/// starts through the usual consent and launch approval. The request is not held back until it
/// is up: it reports [`LanguageServerNotRunning`] and the caller can ask again.
fn require_language_server(
    project: &Entity<Project>,
    target: &QueryTarget,
    cx: &mut AsyncApp,
) -> Result<project::lsp_store::OpenLspBufferHandle> {
    let registration = project.update(cx, |project, cx| {
        project.register_buffer_with_language_servers(&target.buffer, cx)
    });
    let has_language_server = project.read_with(cx, |project, cx| {
        project.has_language_server_for_buffer(&target.buffer, cx)
    });
    if has_language_server {
        Ok(registration)
    } else {
        Err(anyhow!(LanguageServerNotRunning {
            path: target.display_path.clone(),
        }))
    }
}

async fn with_timeout<T>(request: impl Future<Output = T>, cx: &AsyncApp) -> Result<T> {
    let timer = cx.background_executor().timer(LANGUAGE_SERVER_TIMEOUT);
    match future::select(pin!(request), pin!(timer)).await {
        Either::Left((value, _)) => Ok(value),
        Either::Right(_) => Err(anyhow!(
            "The language server didn't respond within {} seconds",
            LANGUAGE_SERVER_TIMEOUT.as_secs()
        )),
    }
}

fn display_path(project: &Project, path: &ProjectPath, cx: &App) -> String {
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

/// A file reached through a link that leaves its worktree lies outside the project as much as a
/// dependency does (`read_file` refuses it for the same reason), even though its path is inside.
/// What lies behind such a link is not scanned until something opens it, so the file itself
/// may have no entry yet: the link above it is what says so.
pub(crate) fn is_in_project(project: &Project, path: &ProjectPath, cx: &App) -> bool {
    project
        .worktree_for_id(path.worktree_id, cx)
        .is_some_and(|worktree| worktree.read(cx).is_visible())
        && !path.path.ancestors().any(|ancestor| {
            let ancestor = ProjectPath {
                worktree_id: path.worktree_id,
                path: ancestor.into(),
            };
            project
                .entry_for_path(&ancestor, cx)
                .is_some_and(|entry| entry.is_external)
        })
}

/// Why the contents of `path` must not reach an agent, if they must not. The settings are
/// checked as well as the worktree entry because a file the language server pointed at need
/// not have been scanned.
fn withheld_reason(project: &Project, path: &ProjectPath, cx: &App) -> Option<&'static str> {
    let global_settings = WorktreeSettings::get_global(cx);
    let worktree_settings = WorktreeSettings::get(Some(path.into()), cx);
    let entry_is_private = project
        .entry_for_path(path, cx)
        .is_some_and(|entry| entry.is_private);
    if entry_is_private
        || global_settings.is_path_private(&path.path)
        || worktree_settings.is_path_private(&path.path)
    {
        return Some("matches `private_files`");
    }
    if global_settings.is_path_excluded(&path.path)
        || worktree_settings.is_path_excluded(&path.path)
    {
        return Some("matches `file_scan_exclusions`");
    }
    None
}

/// Why the contents of `path` must not reach an agent, if they must not: [`withheld_reason`]
/// and [`is_in_project`] in one answer, for callers that report locations themselves. The
/// reason reads after "it", as in "it matches `private_files`".
pub fn agent_withheld_reason(
    project: &Project,
    path: &ProjectPath,
    cx: &App,
) -> Option<&'static str> {
    if !is_in_project(project, path, cx) {
        return Some("lies outside the project");
    }
    withheld_reason(project, path, cx)
}

/// [`agent_withheld_reason`] for an open buffer. A buffer without a file has no path to judge
/// and nothing on disk behind it.
pub fn agent_withheld_reason_for_buffer(
    project: &Project,
    buffer: &Buffer,
    cx: &App,
) -> Option<&'static str> {
    let file = buffer.file()?;
    if file.is_private() {
        return Some("matches `private_files`");
    }
    agent_withheld_reason(project, &ProjectPath::from_file(file.as_ref(), cx), cx)
}

fn resolve_position(
    snapshot: &BufferSnapshot,
    position: &LinePosition,
    display_path: &str,
) -> Result<Point> {
    let line = position.line;
    if line == 0 {
        return Err(anyhow!("Line numbers start at 1 (got 0)"));
    }
    let row = line - 1;
    let max_row = snapshot.max_point().row;
    if row > max_row {
        return Err(anyhow!(
            "`{display_path}` has no line {line} (it has {} lines)",
            max_row + 1
        ));
    }
    let line_text = snapshot
        .text_for_range(Point::new(row, 0)..Point::new(row, snapshot.line_len(row)))
        .collect::<String>();

    let byte_column = if let Some(column) = position.column {
        if column == 0 {
            return Err(anyhow!("Column numbers start at 1 (got 0)"));
        }
        let character_count = line_text.chars().count();
        let character_index = (column - 1) as usize;
        if character_index > character_count {
            return Err(anyhow!(
                "Line {line} of `{display_path}` has {character_count} characters, so it has no column {column}"
            ));
        }
        line_text
            .char_indices()
            .nth(character_index)
            .map_or(line_text.len(), |(offset, _)| offset)
    } else if let Some(symbol) = position
        .symbol
        .as_deref()
        .map(str::trim)
        .filter(|symbol| !symbol.is_empty())
    {
        line_text.find(symbol).ok_or_else(|| {
            anyhow!(
                "`{symbol}` not found on line {line} of `{display_path}` (line text: `{}`)",
                truncate_chars(line_text.trim(), MAX_SIGNATURE_CHARS)
            )
        })?
    } else {
        line_text
            .char_indices()
            .find(|(_, character)| !character.is_whitespace())
            .map_or(0, |(offset, _)| offset)
    };
    Ok(Point::new(row, byte_column as u32))
}

fn hover_markdown(hovers: &[Hover]) -> Option<String> {
    let sections = hovers
        .iter()
        .filter_map(|hover| {
            let blocks = hover
                .contents
                .iter()
                .filter(|block| !block.text.trim().is_empty())
                .map(|block| match &block.kind {
                    HoverBlockKind::Markdown | HoverBlockKind::PlainText => {
                        block.text.trim().to_string()
                    }
                    HoverBlockKind::Code { language } => MarkdownCodeBlock {
                        tag: language,
                        text: block.text.trim_end(),
                    }
                    .to_string(),
                })
                .collect::<Vec<_>>();
            (!blocks.is_empty()).then(|| blocks.join("\n\n"))
        })
        .collect::<Vec<_>>();
    (!sections.is_empty()).then(|| sections.join("\n\n---\n\n"))
}

fn query_locations(project: &Project, locations: Vec<Location>, cx: &App) -> Vec<QueryLocation> {
    let mut results: Vec<QueryLocation> = Vec::new();
    for location in locations {
        if results.len() >= MAX_QUERY_RESULTS {
            break;
        }
        let buffer = location.buffer.read(cx);
        let Some(file) = buffer.file() else {
            continue;
        };
        let path = ProjectPath::from_file(file.as_ref(), cx);
        let snapshot = buffer.snapshot();
        let range =
            location.range.start.to_point(&snapshot)..location.range.end.to_point(&snapshot);
        let lines = LineRange {
            start_line: range.start.row + 1,
            end_line: range.end.row + 1,
        };
        if results
            .iter()
            .any(|existing| existing.path == path && existing.lines == lines)
        {
            continue;
        }
        let in_project = is_in_project(project, &path, cx);
        let preview =
            if in_project && !file.is_private() && withheld_reason(project, &path, cx).is_none() {
                preview_lines(&snapshot, range.start.row)
            } else {
                String::new()
            };
        results.push(QueryLocation {
            path,
            lines,
            in_project,
            preview,
        });
    }
    results
}

/// The line itself with one line of context on each side.
fn preview_lines(snapshot: &BufferSnapshot, row: u32) -> String {
    let first_row = row.saturating_sub(1);
    let last_row = (row + 1).min(snapshot.max_point().row);
    let text = snapshot
        .text_for_range(Point::new(first_row, 0)..Point::new(last_row, snapshot.line_len(last_row)))
        .collect::<String>();
    truncate_chars(&text, MAX_PREVIEW_CHARS)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((offset, _)) => format!("{}…", &text[..offset]),
        None => text.to_string(),
    }
}

fn outline_symbol_hits(
    outline: &[OutlineItem<Point>],
    path: &ProjectPath,
    in_project: bool,
) -> Vec<SymbolHit> {
    symbol_search::outline_symbols(outline)
        .into_iter()
        .map(|symbol| SymbolHit {
            name: symbol.name,
            kind: symbol.kind.to_string(),
            container: symbol.container,
            path: path.clone(),
            lines: symbol.lines,
            signature: if in_project {
                symbol.signature
            } else {
                String::new()
            },
            in_project,
            score: 0.0,
        })
        .collect()
}

fn language_server_symbol_hits(
    symbols: &[DocumentSymbol],
    snapshot: &BufferSnapshot,
    outline: &[OutlineItem<Point>],
    path: &ProjectPath,
    in_project: bool,
) -> Vec<SymbolHit> {
    fn visit(
        symbol: &DocumentSymbol,
        container: Option<&str>,
        snapshot: &BufferSnapshot,
        outline: &[OutlineItem<Point>],
        path: &ProjectPath,
        in_project: bool,
        hits: &mut Vec<SymbolHit>,
    ) {
        let start_row = symbol.range.start.0.row;
        let end_row = symbol.range.end.0.row.max(start_row);
        let name_row = symbol.selection_range.start.0.row;
        let signature = if in_project {
            outline
                .iter()
                .find(|item| item.selection_range.start.row == name_row)
                .map(|item| truncate_chars(&item.text, MAX_SIGNATURE_CHARS))
                .unwrap_or_else(|| source_line_signature(snapshot, name_row))
        } else {
            String::new()
        };
        hits.push(SymbolHit {
            name: symbol.name.clone(),
            kind: symbol_search::symbol_kind_name(symbol.kind).to_string(),
            container: container.map(str::to_string),
            path: path.clone(),
            lines: LineRange {
                start_line: start_row + 1,
                end_line: end_row + 1,
            },
            signature,
            in_project,
            score: 0.0,
        });
        for child in &symbol.children {
            visit(
                child,
                Some(&symbol.name),
                snapshot,
                outline,
                path,
                in_project,
                hits,
            );
        }
    }

    let mut hits = Vec::new();
    for symbol in symbols {
        visit(symbol, None, snapshot, outline, path, in_project, &mut hits);
    }
    hits
}

fn source_line_signature(snapshot: &BufferSnapshot, row: u32) -> String {
    if row > snapshot.max_point().row {
        return String::new();
    }
    let line = snapshot
        .text_for_range(Point::new(row, 0)..Point::new(row, snapshot.line_len(row)))
        .collect::<String>();
    let line = line.trim();
    let line = line.strip_suffix('{').map_or(line, str::trim_end);
    truncate_chars(line, MAX_SIGNATURE_CHARS)
}

/// Who asked for a display. Each source's highlights, annotations and tour replace only that
/// source's previous ones, and [`CodeDisplay::clear`] removes only that source's.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AssistSource {
    pub id: SharedString,
    /// Shown on the frame around the agent's text, e.g. "Claude Code".
    pub label: SharedString,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CodeHighlight {
    pub path: ProjectPath,
    pub lines: LineRange,
    pub note: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CodeAnnotation {
    pub path: ProjectPath,
    pub lines: LineRange,
    pub markdown: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TourStep {
    pub path: ProjectPath,
    pub lines: LineRange,
    pub title: String,
    pub markdown: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CodeTour {
    pub title: String,
    pub steps: Vec<TourStep>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DisplayOutcome {
    pub shown: usize,
    pub skipped: Vec<SkippedDisplay>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SkippedDisplay {
    pub path: ProjectPath,
    pub reason: String,
}

/// Implemented by `ide_assist`. Every method finds the workspace showing `project`; path
/// validation (session roots, private files) is the caller's job.
pub trait CodeDisplay: 'static + Send + Sync {
    fn show_highlights(
        &self,
        project: &Entity<Project>,
        source: AssistSource,
        highlights: Vec<CodeHighlight>,
        cx: &mut App,
    ) -> Task<Result<DisplayOutcome>>;

    fn show_annotations(
        &self,
        project: &Entity<Project>,
        source: AssistSource,
        annotations: Vec<CodeAnnotation>,
        cx: &mut App,
    ) -> Task<Result<DisplayOutcome>>;

    fn start_tour(
        &self,
        project: &Entity<Project>,
        source: AssistSource,
        tour: CodeTour,
        cx: &mut App,
    ) -> Task<Result<DisplayOutcome>>;

    fn clear(&self, project: &Entity<Project>, source: &AssistSource, cx: &mut App);
}

pub struct GlobalCodeDisplay(pub Arc<dyn CodeDisplay>);

impl Global for GlobalCodeDisplay {}

pub fn code_display(cx: &App) -> Option<Arc<dyn CodeDisplay>> {
    cx.try_global::<GlobalCodeDisplay>()
        .map(|display| display.0.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{FutureExt as _, StreamExt as _};
    use gpui::{TestAppContext, UpdateGlobal as _};
    use language::{FakeLspAdapter, rust_lang};
    use project::{FakeFs, lsp_store::OpenLspBufferHandle};
    use serde_json::json;
    use settings::{SettingsStore, SplicingVec};
    use util::{path, paths::PathStyle, rel_path::rel_path};

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project.worktree.file_scan_exclusions =
                        Some(SplicingVec::from(vec!["**/excluded".to_string()]));
                    settings.project.worktree.private_files =
                        Some(vec!["**/secret.rs".to_string()].into());
                });
            });
        });
    }

    async fn test_project(
        root: serde_json::Value,
        dependencies: serde_json::Value,
        cx: &mut TestAppContext,
    ) -> Entity<Project> {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), root).await;
        fs.insert_tree(path!("/deps"), dependencies).await;
        let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
        project.read_with(cx, |project, _| project.languages().add(rust_lang()));
        cx.executor().run_until_parked();
        project
    }

    async fn start_language_server(
        project: &Entity<Project>,
        abs_path: &str,
        cx: &mut TestAppContext,
    ) -> (lsp::FakeLanguageServer, OpenLspBufferHandle) {
        let language_registry = project.read_with(cx, |project, _| project.languages().clone());
        let mut fake_servers = language_registry.register_fake_lsp(
            "Rust",
            FakeLspAdapter {
                capabilities: lsp::ServerCapabilities {
                    hover_provider: Some(lsp::HoverProviderCapability::Simple(true)),
                    definition_provider: Some(lsp::OneOf::Left(true)),
                    references_provider: Some(lsp::OneOf::Left(true)),
                    document_symbol_provider: Some(lsp::OneOf::Left(true)),
                    workspace_symbol_provider: Some(lsp::OneOf::Left(true)),
                    ..lsp::ServerCapabilities::default()
                },
                ..FakeLspAdapter::default()
            },
        );
        let (_buffer, registration) = project
            .update(cx, |project, cx| {
                project.open_local_buffer_with_lsp(abs_path, cx)
            })
            .await
            .expect("the file opens");
        let server = fake_servers.next().await.expect("the server starts");
        cx.executor().run_until_parked();
        (server, registration)
    }

    fn project_path(project: &Entity<Project>, path: &str, cx: &TestAppContext) -> ProjectPath {
        project.read_with(cx, |project, cx| {
            let worktree = project
                .visible_worktrees(cx)
                .next()
                .expect("the project has a worktree");
            ProjectPath {
                worktree_id: worktree.read(cx).id(),
                path: rel_path(path).into(),
            }
        })
    }

    fn position(line: u32, symbol: Option<&str>, column: Option<u32>) -> LinePosition {
        LinePosition {
            line,
            symbol: symbol.map(str::to_string),
            column,
        }
    }

    fn lsp_location(abs_path: &str, row: u32) -> lsp::Location {
        lsp::Location::new(
            lsp::Uri::from_file_path(abs_path).expect("an absolute path"),
            lsp::Range::new(lsp::Position::new(row, 0), lsp::Position::new(row, 2)),
        )
    }

    const JAPANESE_SOURCE: &str = "fn 日本語() {}\nfn main() {\n    let 変数 = 日本語();\n}\n";

    #[gpui::test]
    async fn test_hover_positions_count_characters_not_bytes(cx: &mut TestAppContext) {
        init_test(cx);
        let project =
            test_project(json!({"src": {"lib.rs": JAPANESE_SOURCE}}), json!({}), cx).await;
        let (server, _registration) =
            start_language_server(&project, path!("/root/src/lib.rs"), cx).await;
        server.set_request_handler::<lsp::request::HoverRequest, _, _>(|params, _| async move {
            let position = params.text_document_position_params.position;
            Ok(Some(lsp::Hover {
                contents: lsp::HoverContents::Array(vec![
                    lsp::MarkedString::String(format!("{}:{}", position.line, position.character)),
                    lsp::MarkedString::LanguageString(lsp::LanguageString {
                        language: "rust".to_string(),
                        value: "fn 日本語()".to_string(),
                    }),
                ]),
                range: None,
            }))
        });
        let path = project_path(&project, "src/lib.rs", cx);

        let cases = [
            // `日本語` starts at character 13 but at byte 17 of `    let 変数 = 日本語();`.
            (position(3, Some("日本語"), None), "2:13"),
            (position(3, Some("変数"), Some(14)), "2:13"),
            (position(3, None, None), "2:4"),
            (position(3, None, Some(20)), "2:19"),
        ];
        for (position, expected) in cases {
            let markdown = cx
                .update(|cx| hover(&project, path.clone(), position.clone(), cx))
                .await
                .expect("the hover succeeds")
                .expect("the server answered");
            assert!(markdown.starts_with(expected), "{position:?}: {markdown}");
            assert!(
                markdown.contains("```rust\nfn 日本語()\n```"),
                "{position:?}: {markdown}"
            );
        }
    }

    #[gpui::test]
    async fn test_positions_that_do_not_exist_are_refused(cx: &mut TestAppContext) {
        init_test(cx);
        let project =
            test_project(json!({"src": {"lib.rs": JAPANESE_SOURCE}}), json!({}), cx).await;
        let path = project_path(&project, "src/lib.rs", cx);

        for (position, expected) in [
            (position(0, None, None), "Line numbers start at 1"),
            (position(99, None, None), "has no line 99 (it has 5 lines)"),
            (
                position(3, Some("存在しない"), None),
                "`存在しない` not found on line 3",
            ),
            (position(3, None, Some(0)), "Column numbers start at 1"),
            (
                position(3, None, Some(21)),
                "has 19 characters, so it has no column 21",
            ),
        ] {
            let error = cx
                .update(|cx| hover(&project, path.clone(), position.clone(), cx))
                .await
                .expect_err("the position does not exist");
            assert!(
                error.to_string().contains(expected),
                "{position:?}: {error}"
            );
        }
    }

    #[gpui::test]
    async fn test_a_missing_language_server_is_reported_as_such(cx: &mut TestAppContext) {
        init_test(cx);
        let project =
            test_project(json!({"src": {"lib.rs": JAPANESE_SOURCE}}), json!({}), cx).await;
        let path = project_path(&project, "src/lib.rs", cx);

        let hover_error = cx
            .update(|cx| {
                hover(
                    &project,
                    path.clone(),
                    position(1, Some("日本語"), None),
                    cx,
                )
            })
            .await
            .expect_err("no server can answer");
        let definitions_error = cx
            .update(|cx| definitions(&project, path.clone(), position(1, None, None), cx))
            .await
            .expect_err("no server can answer");
        let references_error = cx
            .update(|cx| references(&project, path.clone(), position(1, None, None), cx))
            .await
            .expect_err("no server can answer");
        let expected = LanguageServerNotRunning {
            path: rel_path("root/src/lib.rs")
                .display(PathStyle::local())
                .into_owned(),
        };
        for error in [hover_error, definitions_error, references_error] {
            assert_eq!(
                error.downcast_ref::<LanguageServerNotRunning>(),
                Some(&expected),
                "{error}"
            );
            assert!(error.to_string().contains("Language server not running"));
        }
    }

    #[gpui::test]
    async fn test_private_and_excluded_files_are_not_queried(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(
            json!({
                "secret.rs": "fn hidden() {}\n",
                "excluded": {"generated.rs": "fn generated() {}\n"},
            }),
            json!({}),
            cx,
        )
        .await;

        let secret = project_path(&project, "secret.rs", cx);
        let error = cx
            .update(|cx| document_symbols(&project, secret, cx))
            .await
            .expect_err("private files are refused");
        assert!(error.to_string().contains("`private_files`"), "{error}");

        let excluded = project_path(&project, "excluded/generated.rs", cx);
        let error = cx
            .update(|cx| hover(&project, excluded, position(1, None, None), cx))
            .await
            .expect_err("excluded files are refused");
        assert!(
            error.to_string().contains("`file_scan_exclusions`"),
            "{error}"
        );
    }

    #[gpui::test]
    async fn test_a_language_server_that_does_not_answer_times_out(cx: &mut TestAppContext) {
        init_test(cx);
        let project =
            test_project(json!({"src": {"lib.rs": JAPANESE_SOURCE}}), json!({}), cx).await;
        let (server, _registration) =
            start_language_server(&project, path!("/root/src/lib.rs"), cx).await;
        server.set_request_handler::<lsp::request::HoverRequest, _, _>(|_, _| {
            futures::future::pending::<Result<Option<lsp::Hover>>>()
        });
        let path = project_path(&project, "src/lib.rs", cx);

        let mut task = cx.update(|cx| hover(&project, path, position(1, Some("日本語"), None), cx));
        cx.executor().run_until_parked();
        assert!(
            (&mut task).now_or_never().is_none(),
            "the query waits for the server at first"
        );
        cx.executor()
            .advance_clock(LANGUAGE_SERVER_TIMEOUT + Duration::from_secs(1));
        cx.executor().run_until_parked();
        let error = task
            .now_or_never()
            .expect("the query gives up once the timeout passes")
            .expect_err("the server never answers");
        assert!(
            error.to_string().contains("didn't respond within"),
            "{error}"
        );
    }

    #[gpui::test]
    async fn test_definitions_withhold_what_lies_outside_the_project(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(
            json!({
                "src": {
                    "lib.rs": "fn main() {\n    helper();\n}\n",
                    "other.rs": "// before\nfn helper() {}\n// after\n",
                },
                "secret.rs": "fn helper() {}\n",
            }),
            json!({"dep.rs": "pub fn helper() {}\n"}),
            cx,
        )
        .await;
        let (server, _registration) =
            start_language_server(&project, path!("/root/src/lib.rs"), cx).await;
        server.set_request_handler::<lsp::request::GotoDefinition, _, _>(|_, _| async move {
            Ok(Some(lsp::GotoDefinitionResponse::Array(vec![
                lsp_location(path!("/root/src/other.rs"), 1),
                lsp_location(path!("/deps/dep.rs"), 0),
                lsp_location(path!("/root/secret.rs"), 0),
            ])))
        });
        let path = project_path(&project, "src/lib.rs", cx);

        let locations = cx
            .update(|cx| definitions(&project, path, position(2, Some("helper"), None), cx))
            .await
            .expect("the definitions are found");
        assert_eq!(locations.len(), 3, "{locations:?}");

        let in_project = &locations[0];
        assert_eq!(in_project.path, project_path(&project, "src/other.rs", cx));
        assert_eq!(
            in_project.lines,
            LineRange {
                start_line: 2,
                end_line: 2
            }
        );
        assert!(in_project.in_project);
        assert_eq!(in_project.preview, "// before\nfn helper() {}\n// after");

        let dependency = &locations[1];
        assert!(!dependency.in_project, "{dependency:?}");
        assert_eq!(
            dependency.preview, "",
            "a dependency's source is not handed out"
        );

        let private = &locations[2];
        assert!(private.in_project, "{private:?}");
        assert_eq!(
            private.preview, "",
            "a private file's source is not handed out"
        );
    }

    #[gpui::test]
    async fn test_definitions_withhold_what_a_link_leads_out_to(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({"src": {"lib.rs": "fn main() {\n    helper();\n}\n"}}),
        )
        .await;
        fs.insert_tree(
            path!("/outside"),
            json!({"linked.rs": "// before\nfn helper() {}\n// after\n"}),
        )
        .await;
        fs.insert_symlink(path!("/root/link"), path!("/outside").into())
            .await;
        let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
        project.read_with(cx, |project, _| project.languages().add(rust_lang()));
        cx.executor().run_until_parked();
        // Nothing has opened the file yet, so only the link itself has an entry.
        let linked = project_path(&project, "link/linked.rs", cx);
        let inside = project_path(&project, "src/lib.rs", cx);
        project.read_with(cx, |project, cx| {
            assert!(project.entry_for_path(&linked, cx).is_none());
            assert!(!is_in_project(project, &linked, cx));
            assert_eq!(
                agent_withheld_reason(project, &linked, cx),
                Some("lies outside the project")
            );
            assert!(is_in_project(project, &inside, cx));
            assert_eq!(agent_withheld_reason(project, &inside, cx), None);
        });

        let (server, _registration) =
            start_language_server(&project, path!("/root/src/lib.rs"), cx).await;
        server.set_request_handler::<lsp::request::GotoDefinition, _, _>(|_, _| async move {
            Ok(Some(lsp::GotoDefinitionResponse::Array(vec![
                lsp_location(path!("/root/link/linked.rs"), 1),
            ])))
        });
        let path = project_path(&project, "src/lib.rs", cx);

        let locations = cx
            .update(|cx| definitions(&project, path, position(2, Some("helper"), None), cx))
            .await
            .expect("the definition is found");
        assert_eq!(locations.len(), 1, "{locations:?}");
        let linked = &locations[0];
        assert_eq!(linked.path, project_path(&project, "link/linked.rs", cx));
        assert!(!linked.in_project, "{linked:?}");
        assert_eq!(
            linked.preview, "",
            "what a link leads out to is not handed out"
        );
    }

    /// The server that answers was started for another file: the query has to register the
    /// file it asks about, or the server is never told about it.
    #[gpui::test]
    async fn test_a_file_no_editor_has_open_is_registered_for_the_query(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(
            json!({"src": {"lib.rs": JAPANESE_SOURCE, "other.rs": "fn other() {}\n"}}),
            json!({}),
            cx,
        )
        .await;
        let (server, _registration) =
            start_language_server(&project, path!("/root/src/lib.rs"), cx).await;
        server.set_request_handler::<lsp::request::HoverRequest, _, _>(|params, _| async move {
            Ok(Some(lsp::Hover {
                contents: lsp::HoverContents::Scalar(lsp::MarkedString::String(
                    params
                        .text_document_position_params
                        .text_document
                        .uri
                        .to_string(),
                )),
                range: None,
            }))
        });
        let path = project_path(&project, "src/other.rs", cx);

        let markdown = cx
            .update(|cx| hover(&project, path, position(1, Some("other"), None), cx))
            .await
            .expect("the running server answers for the other file")
            .expect("the server answered");
        assert!(markdown.ends_with("other.rs"), "{markdown}");
    }

    /// Each language server answers separately and the project does not merge their answers,
    /// so two servers for one file report the same place twice.
    #[gpui::test]
    async fn test_the_same_location_is_reported_once(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(
            json!({"src": {"lib.rs": "fn first() {}\nfn second() {}\n"}}),
            json!({}),
            cx,
        )
        .await;
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/root/src/lib.rs"), cx)
            })
            .await
            .expect("the file opens");
        let locations = buffer.read_with(cx, |text, _| {
            [0, 1, 0].map(|row| Location {
                buffer: buffer.clone(),
                range: text.anchor_before(Point::new(row, 0))
                    ..text.anchor_after(Point::new(row, 2)),
            })
        });

        let results = project.read_with(cx, |project, cx| {
            query_locations(project, Vec::from(locations), cx)
        });
        let lines = results
            .iter()
            .map(|location| location.lines.start_line)
            .collect::<Vec<_>>();
        assert_eq!(lines, [1, 2]);
    }

    #[gpui::test]
    async fn test_references_are_capped(cx: &mut TestAppContext) {
        init_test(cx);
        let many_lines = (0..150)
            .map(|index| format!("fn function_{index}() {{}}\n"))
            .collect::<String>();
        let project = test_project(json!({"src": {"lib.rs": many_lines}}), json!({}), cx).await;
        let (server, _registration) =
            start_language_server(&project, path!("/root/src/lib.rs"), cx).await;
        server.set_request_handler::<lsp::request::References, _, _>(|_, _| async move {
            let mut locations = vec![lsp_location(path!("/root/src/lib.rs"), 0)];
            locations.extend((0..150).map(|row| lsp_location(path!("/root/src/lib.rs"), row)));
            Ok(Some(locations))
        });
        let path = project_path(&project, "src/lib.rs", cx);

        let locations = cx
            .update(|cx| references(&project, path, position(1, Some("function_0"), None), cx))
            .await
            .expect("the references are found");
        assert_eq!(locations.len(), MAX_QUERY_RESULTS);
        let first_lines = locations
            .iter()
            .take(3)
            .map(|location| location.lines.start_line)
            .collect::<Vec<_>>();
        assert_eq!(first_lines, [1, 2, 3]);
        assert_eq!(
            locations[1].preview,
            "fn function_0() {}\nfn function_1() {}\nfn function_2() {}"
        );
    }

    const CONFIG_SOURCE: &str = "pub struct Config {\n    pub name: String,\n}\n\nimpl Config {\n    pub fn new() -> Self {\n        todo!()\n    }\n}\n";

    fn document_symbol(
        name: &str,
        kind: lsp::SymbolKind,
        range: lsp::Range,
        selection_range: lsp::Range,
        children: Option<Vec<lsp::DocumentSymbol>>,
    ) -> lsp::DocumentSymbol {
        #[allow(deprecated)]
        lsp::DocumentSymbol {
            name: name.to_string(),
            detail: None,
            kind,
            tags: None,
            deprecated: None,
            range,
            selection_range,
            children,
        }
    }

    fn lsp_range(start_row: u32, start_column: u32, end_row: u32, end_column: u32) -> lsp::Range {
        lsp::Range::new(
            lsp::Position::new(start_row, start_column),
            lsp::Position::new(end_row, end_column),
        )
    }

    #[gpui::test]
    async fn test_document_symbols_come_from_the_language_server(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(json!({"src": {"lib.rs": CONFIG_SOURCE}}), json!({}), cx).await;
        let (server, _registration) =
            start_language_server(&project, path!("/root/src/lib.rs"), cx).await;
        server.set_request_handler::<lsp::request::DocumentSymbolRequest, _, _>(
            |_, _| async move {
                let field = document_symbol(
                    "name",
                    lsp::SymbolKind::FIELD,
                    lsp_range(1, 4, 1, 20),
                    lsp_range(1, 8, 1, 12),
                    None,
                );
                let structure = document_symbol(
                    "Config",
                    lsp::SymbolKind::STRUCT,
                    lsp_range(0, 0, 2, 1),
                    lsp_range(0, 11, 0, 17),
                    Some(vec![field]),
                );
                Ok(Some(lsp::DocumentSymbolResponse::Nested(vec![structure])))
            },
        );
        let path = project_path(&project, "src/lib.rs", cx);

        let symbols = cx
            .update(|cx| document_symbols(&project, path.clone(), cx))
            .await
            .expect("the symbols are listed");
        let summary = symbols
            .iter()
            .map(|symbol| {
                (
                    symbol.name.as_str(),
                    symbol.kind.as_str(),
                    symbol.container.as_deref(),
                    symbol.lines.start_line,
                    symbol.lines.end_line,
                    symbol.signature.as_str(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            summary,
            [
                ("Config", "struct", None, 1, 3, "pub struct Config"),
                ("name", "field", Some("Config"), 2, 2, "pub name"),
            ]
        );
        assert!(
            symbols
                .iter()
                .all(|symbol| symbol.path == path && symbol.in_project)
        );
    }

    #[gpui::test]
    async fn test_document_symbols_fall_back_to_the_outline(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(json!({"src": {"lib.rs": CONFIG_SOURCE}}), json!({}), cx).await;
        let path = project_path(&project, "src/lib.rs", cx);

        let symbols = cx
            .update(|cx| document_symbols(&project, path, cx))
            .await
            .expect("the outline stands in for the missing server");
        let summary = symbols
            .iter()
            .map(|symbol| {
                (
                    symbol.name.as_str(),
                    symbol.kind.as_str(),
                    symbol.container.as_deref(),
                    symbol.lines.start_line,
                    symbol.lines.end_line,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            summary,
            [
                ("Config", "struct", None, 1, 3),
                ("name", "field", Some("Config"), 2, 2),
                ("Config", "impl", None, 5, 9),
                ("new", "method", Some("Config"), 6, 8),
            ]
        );
        assert_eq!(symbols[3].signature, "pub fn new");
    }

    fn search(
        project: &Entity<Project>,
        query: &str,
        limit: usize,
        path_prefix: Option<&str>,
        cx: &mut TestAppContext,
    ) -> Task<Result<Vec<SymbolHit>>> {
        cx.update(|cx| {
            search_symbols(
                project,
                query,
                SymbolSearchOptions {
                    limit,
                    path_prefix: path_prefix.map(str::to_string),
                },
                cx,
            )
        })
    }

    fn names(hits: &[SymbolHit]) -> Vec<&str> {
        hits.iter().map(|hit| hit.name.as_str()).collect()
    }

    #[gpui::test]
    async fn test_symbol_search_ranks_outline_names_by_how_well_they_match(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let project = test_project(
            json!({
                "src": {
                    "a.rs": "pub fn token_for_user() {}\n",
                    "b.rs": "pub fn validate_user_token() {}\n",
                    "c.rs": "pub struct UserTokenStore;\n",
                    "d.rs": "pub struct UserToken;\n",
                    "user_token_cache.rs": "fn load() {}\n",
                    "f.rs": "fn unrelated() {}\n",
                },
            }),
            json!({}),
            cx,
        )
        .await;

        let hits = search(&project, "user token", 0, None, cx)
            .await
            .expect("the search runs");
        assert_eq!(
            names(&hits),
            [
                "UserToken",
                "UserTokenStore",
                "validate_user_token",
                "token_for_user",
                "load"
            ]
        );
        let tiers = hits
            .iter()
            .map(|hit| (hit.score / 100.0).floor() as u32)
            .collect::<Vec<_>>();
        assert_eq!(
            tiers,
            [5, 4, 3, 2, 1],
            "exact, prefix, substring, words, context"
        );
        let store = &hits[1];
        assert_eq!(store.kind, "struct");
        assert_eq!(store.path, project_path(&project, "src/c.rs", cx));
        assert_eq!(store.signature, "pub struct UserTokenStore");
        assert_eq!(
            hits[4].path,
            project_path(&project, "src/user_token_cache.rs", cx),
            "the path holds the words"
        );
        assert!(hits.iter().all(|hit| hit.in_project));

        let hits = search(&project, "UserToken", 0, None, cx)
            .await
            .expect("the search runs");
        assert_eq!(names(&hits)[..2], ["UserToken", "UserTokenStore"]);
    }

    #[gpui::test]
    async fn test_symbol_search_honors_the_path_prefix_and_the_limit(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(
            json!({
                "src": {
                    "sub": {"inner.rs": "fn parse_config() {}\n"},
                    "subtle.rs": "fn parse_config_file() {}\n",
                    "top.rs": "fn parse_configuration() {}\n",
                },
            }),
            json!({}),
            cx,
        )
        .await;

        for prefix in ["root/src/sub", "src/sub", "./src/sub/", "src\\sub"] {
            let hits = search(&project, "parse config", 0, Some(prefix), cx)
                .await
                .expect("the search runs");
            assert_eq!(names(&hits), ["parse_config"], "prefix {prefix}");
        }
        let hits = search(&project, "parse config", 0, Some("."), cx)
            .await
            .expect("the search runs");
        assert_eq!(hits.len(), 3);
        let hits = search(&project, "parse config", 2, None, cx)
            .await
            .expect("the search runs");
        assert_eq!(names(&hits), ["parse_config", "parse_config_file"]);

        let error = search(&project, " :: ", 0, None, cx)
            .await
            .expect_err("nothing to search for");
        assert!(
            error.to_string().contains("Specify a search query"),
            "{error}"
        );
    }

    #[gpui::test]
    async fn test_symbol_search_merges_the_language_server_and_hides_private_files(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let project = test_project(
            json!({
                "src": {"lib.rs": "pub fn validate_user_token() {}\n"},
                "secret.rs": "pub fn user_token_secret() {}\n",
                "excluded": {"generated.rs": "pub fn user_token_generated() {}\n"},
            }),
            json!({"dep.rs": "pub fn user_token_dependency() {}\n"}),
            cx,
        )
        .await;
        let (server, _registration) =
            start_language_server(&project, path!("/root/src/lib.rs"), cx).await;
        server.set_request_handler::<lsp::WorkspaceSymbolRequest, _, _>(|params, _| async move {
            assert!(
                ["usertoken", "vut"].contains(&params.query.as_str()),
                "{}",
                params.query
            );
            #[allow(deprecated)]
            let symbol = |name: &str, abs_path: &str| lsp::SymbolInformation {
                name: name.to_string(),
                kind: lsp::SymbolKind::FUNCTION,
                tags: None,
                deprecated: None,
                container_name: Some("auth".to_string()),
                location: lsp_location(abs_path, 0),
            };
            Ok(Some(lsp::WorkspaceSymbolResponse::Flat(vec![
                symbol("validate_user_token", path!("/root/src/lib.rs")),
                symbol("user_token_secret", path!("/root/secret.rs")),
                symbol("user_token_generated", path!("/root/excluded/generated.rs")),
                symbol("user_token_dependency", path!("/deps/dep.rs")),
            ])))
        });

        let hits = search(&project, "user token", 0, None, cx)
            .await
            .expect("the search runs");
        assert_eq!(names(&hits), ["validate_user_token"], "{hits:?}");
        let hit = &hits[0];
        assert_eq!(hit.kind, "function", "the server's kind is kept");
        assert_eq!(hit.container.as_deref(), Some("auth"));
        assert_eq!(
            hit.signature, "pub fn validate_user_token",
            "the outline's source text replaces the server's label"
        );

        // Only the server's answer holds the letters in order, so the fuzzy match ranks it
        // below every tier and the server's label stands in for the signature.
        let hits = search(&project, "vut", 0, None, cx)
            .await
            .expect("the search runs");
        assert_eq!(names(&hits), ["validate_user_token"], "{hits:?}");
        assert!(hits[0].score < 100.0, "{hits:?}");
    }

    /// Sources outside the visible worktrees are hidden worktrees of the project; their symbols
    /// may be named, but not quoted. (A dependency file the language server led to is a
    /// single-file worktree, whose workspace symbols the project drops before they get here, so
    /// a hidden folder stands in for it.)
    #[gpui::test]
    async fn test_symbols_outside_the_project_come_without_their_source(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(
            json!({"src": {"lib.rs": "pub fn validate_user_token() {}\n"}}),
            json!({"dep.rs": "pub fn dependency_helper(password: &str) {}\n"}),
            cx,
        )
        .await;
        let (worktree, _) = project
            .update(cx, |project, cx| {
                project.find_or_create_worktree(path!("/deps"), false, cx)
            })
            .await
            .expect("the dependencies open as a hidden worktree");
        cx.executor().run_until_parked();
        let dependency = ProjectPath {
            worktree_id: worktree.read_with(cx, |worktree, _| worktree.id()),
            path: rel_path("dep.rs").into(),
        };

        let symbols = cx
            .update(|cx| document_symbols(&project, dependency.clone(), cx))
            .await
            .expect("the outline lists the dependency's symbols");
        assert_eq!(names(&symbols), ["dependency_helper"], "{symbols:?}");
        assert!(!symbols[0].in_project, "{symbols:?}");
        assert_eq!(symbols[0].signature, "", "the source is not quoted");

        let (server, _registration) =
            start_language_server(&project, path!("/root/src/lib.rs"), cx).await;
        server.set_request_handler::<lsp::WorkspaceSymbolRequest, _, _>(|_, _| async move {
            #[allow(deprecated)]
            let symbol = lsp::SymbolInformation {
                name: "dependency_helper".to_string(),
                kind: lsp::SymbolKind::FUNCTION,
                tags: None,
                deprecated: None,
                container_name: None,
                location: lsp_location(path!("/deps/dep.rs"), 0),
            };
            Ok(Some(lsp::WorkspaceSymbolResponse::Flat(vec![symbol])))
        });
        let hits = search(&project, "dependency", 0, None, cx)
            .await
            .expect("the search runs");
        assert_eq!(names(&hits), ["dependency_helper"], "{hits:?}");
        assert!(!hits[0].in_project, "{hits:?}");
        assert_eq!(hits[0].path, dependency);
        assert_eq!(hits[0].signature, "", "the server's label is not quoted");
    }

    /// rust-analyzer reports a workspace symbol's whole range, which begins at its doc comment,
    /// while the outline gives the row of the name.
    #[gpui::test]
    async fn test_symbol_search_merges_a_server_range_that_starts_above_the_name(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let project = test_project(
            json!({
                "src": {
                    "lib.rs": "/// Checks the token.\n#[inline]\npub fn validate_user_token() {\n}\n",
                },
            }),
            json!({}),
            cx,
        )
        .await;
        let (server, _registration) =
            start_language_server(&project, path!("/root/src/lib.rs"), cx).await;
        server.set_request_handler::<lsp::WorkspaceSymbolRequest, _, _>(|_, _| async move {
            #[allow(deprecated)]
            let symbol = lsp::SymbolInformation {
                name: "validate_user_token".to_string(),
                kind: lsp::SymbolKind::FUNCTION,
                tags: None,
                deprecated: None,
                container_name: Some("auth".to_string()),
                location: lsp::Location::new(
                    lsp::Uri::from_file_path(path!("/root/src/lib.rs")).expect("an absolute path"),
                    lsp_range(0, 0, 3, 1),
                ),
            };
            Ok(Some(lsp::WorkspaceSymbolResponse::Flat(vec![symbol])))
        });

        let hits = search(&project, "user token", 0, None, cx)
            .await
            .expect("the search runs");
        assert_eq!(names(&hits), ["validate_user_token"], "{hits:?}");
        let hit = &hits[0];
        assert_eq!(hit.container.as_deref(), Some("auth"), "{hit:?}");
        assert_eq!(hit.lines.start_line, 3, "the outline's lines are kept");
        assert_eq!(hit.signature, "pub fn validate_user_token");
    }
}
