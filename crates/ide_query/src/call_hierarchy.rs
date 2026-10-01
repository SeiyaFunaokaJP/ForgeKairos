use std::{
    collections::VecDeque,
    fmt,
    ops::Range,
    pin::pin,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use collections::{HashMap, HashSet};
use futures::future::{self, Either};
use gpui::{App, AsyncApp, Entity, EntityId, Task};
use language::{Anchor, Buffer, BufferSnapshot, Location, Point, ToPoint as _, lsp_to_symbol_kind};
use project::{
    CallHierarchyItem, Project, ProjectPath,
    symbol_index_store::{
        CallKind, CalleeTarget, DefinitionKind, IndexedAssignment, IndexedCallSite,
        IndexedCalleeSite, IndexedDefinition, MAX_CALLERS_LIMIT, SymbolIndexStatus,
        SymbolIndexStore,
    },
};
use symbol_index::is_indexable_path;

use crate::{
    LANGUAGE_SERVER_TIMEOUT, LinePosition, LineRange, MAX_QUERY_RESULTS, agent_withheld_reason,
    is_in_project, open_target, require_language_server, resolve_position,
    symbol_search::symbol_kind_name, truncate_chars, withheld_reason,
};

pub const MAX_CALL_HIERARCHY_DEPTH: usize = 3;
pub const DEFAULT_CALL_HIERARCHY_RESULTS: usize = 50;
/// Each level multiplies the requests by the width of the level below, and every request is a
/// round trip to a language server that may still be indexing. Questions to the name index
/// count too: over a remote connection each is a round trip as well.
pub const MAX_CALL_HIERARCHY_REQUESTS: usize = 60;
/// Bounds the whole walk; [`LANGUAGE_SERVER_TIMEOUT`] still bounds each request.
pub const CALL_HIERARCHY_TIME_BUDGET: Duration = Duration::from_secs(45);
/// Over a remote connection every file a result lies in comes over as a buffer.
pub const MAX_REMOTE_CALL_HIERARCHY_FILES: usize = 40;
/// Functions listed when a name alone matches several.
pub const MAX_CALL_CANDIDATES: usize = 20;
/// Definitions listed for a called name that the name index finds defined more than once.
pub const MAX_AMBIGUOUS_DEFINITIONS: usize = 5;
/// Functions listed as stored into the member an indirect call goes through.
pub const MAX_INDIRECT_CANDIDATES: usize = 10;
/// The name index answers without opening files; quoting a line of its results means opening
/// the file, which is what this bounds.
pub const MAX_PREVIEW_FILES: usize = 20;
const CALL_PREVIEW_CHARS: usize = 120;
/// Names come from the language server as they are; a broken or hostile one can send a name of
/// any length, with line breaks that would pass for lines of the tree.
const MAX_CALL_NAME_CHARS: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CallDirection {
    /// The functions that call the root.
    Incoming,
    /// The functions the root calls.
    Outgoing,
}

/// Where a call came from. A language server resolves the call; the name index only matches
/// the name, so a function of the same name elsewhere can stand in for the real one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CallSource {
    LanguageServer,
    NameIndex,
    /// A function stored into the member an indirect call goes through (name index).
    Candidate,
    /// A macro the name index found by name.
    Macro,
}

/// What a node stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallResolution {
    /// A function (or macro): where it is defined.
    Definition,
    /// A called name the name index knows no definition of in the project (a library function or
    /// generated code). The node is placed at the call.
    NoDefinition,
    /// A call through a struct member (`ops->open(...)`), placed at the call. Its children are the
    /// functions stored into that member.
    Indirect { member: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallHierarchyOptions {
    pub direction: CallDirection,
    /// Levels to follow; clamped to 1..=[`MAX_CALL_HIERARCHY_DEPTH`].
    pub depth: usize,
    /// Calls listed across all levels. Zero means [`DEFAULT_CALL_HIERARCHY_RESULTS`]; at most
    /// [`MAX_QUERY_RESULTS`].
    pub max_results: usize,
}

impl CallHierarchyOptions {
    fn normalized(self) -> Self {
        Self {
            direction: self.direction,
            depth: self.depth.clamp(1, MAX_CALL_HIERARCHY_DEPTH),
            max_results: if self.max_results == 0 {
                DEFAULT_CALL_HIERARCHY_RESULTS
            } else {
                self.max_results.min(MAX_QUERY_RESULTS)
            },
        }
    }
}

/// Which function to start from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallHierarchyTarget {
    /// The function declared or called at a position, or the one whose body holds it.
    Position {
        path: ProjectPath,
        position: LinePosition,
    },
    /// A function the name index knows by this name. `near` prefers its file, and leaves out
    /// `static` functions of other source files.
    Name {
        name: String,
        near: Option<ProjectPath>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallExpansion {
    /// `children` are all its calls (possibly none).
    Expanded,
    /// The requested depth ends here, or there is nothing to follow (a call placed at the call
    /// site, or a candidate of an indirect call).
    DepthLimit,
    /// It is also one of its ancestors in the tree.
    Recursive,
    /// It is expanded at another place in the tree.
    Repeated,
    /// Its contents must not reach an agent, so its calls are not followed either.
    Withheld,
    /// The walk stopped (see [`CallHierarchy::stopped`]) before it got here.
    Stopped,
    /// The language server failed to answer for it.
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallWalkStop {
    RequestLimit,
    ResultLimit,
    TimeLimit,
    RemoteFileLimit,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CallNode {
    /// Empty when the function lies in a file that is private or excluded from scanning: its
    /// name is part of that file's contents (`search_symbols` leaves such files out entirely).
    pub name: String,
    pub kind: String,
    pub path: ProjectPath,
    /// From the line of the name through the last line of the body, 1-based.
    pub lines: LineRange,
    /// 1-based, ascending. Incoming: the lines of this node's file where it calls its parent.
    /// Outgoing: the lines of the parent's file where the parent calls it. Empty for the root.
    pub call_lines: Vec<u32>,
    pub in_project: bool,
    /// False for files outside the visible worktrees, reached through a link that leaves one,
    /// private or excluded from scanning. Their `preview` is empty and their calls are not
    /// followed.
    pub contents_allowed: bool,
    /// One trimmed line: the first call for an incoming call, the declaration otherwise, the
    /// call for a node placed at the call. Empty when the file was not read (see
    /// [`MAX_PREVIEW_FILES`]).
    pub preview: String,
    pub source: CallSource,
    /// The number of definitions of this name the name index found, when it found more than
    /// one; this node is one of them.
    pub ambiguous: Option<usize>,
    pub resolution: CallResolution,
    pub expansion: CallExpansion,
    pub children: Vec<CallNode>,
}

/// How far the name index has got through its first build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexBuilding {
    pub done: usize,
    pub total: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CallHierarchy {
    pub direction: CallDirection,
    pub depth: usize,
    pub max_results: usize,
    pub root: CallNode,
    /// Calls the sources reported that were left out because `max_results` was reached (or,
    /// for the name index, because the requests ran out before they were looked up).
    pub omitted: usize,
    pub stopped: Option<CallWalkStop>,
    /// The name index was asked (a C or C++ file, with an index that can answer).
    pub used_name_index: bool,
    /// No language server answered for the root's file, so every call comes from the name
    /// index.
    pub without_language_server: bool,
    /// The name index had more call sites of a name than one answer carries.
    pub index_truncated: bool,
    pub index_building: Option<IndexBuilding>,
}

impl CallHierarchy {
    pub fn listed_calls(&self) -> usize {
        fn count(node: &CallNode) -> usize {
            node.children.len() + node.children.iter().map(count).sum::<usize>()
        }
        count(&self.root)
    }
}

/// Several functions have the name that was asked about.
#[derive(Clone, Debug, PartialEq)]
pub struct CallCandidates {
    pub name: String,
    /// At most [`MAX_CALL_CANDIDATES`], with the same checks as the calls of a tree: one in a file
    /// that must not reach an agent is placed without its name or preview.
    pub candidates: Vec<CallNode>,
    pub total: usize,
    pub index_building: Option<IndexBuilding>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CallHierarchyAnswer {
    Found(CallHierarchy),
    /// No function at the position, or none of that name.
    NoFunction {
        /// The name index was asked too, so "no function" covers what it knows.
        used_name_index: bool,
        index_building: Option<IndexBuilding>,
    },
    /// Only for a name that matches several functions: ask again with one's position.
    Candidates(CallCandidates),
}

/// What [`call_hierarchy`] fails with when the running language servers can't answer call
/// hierarchy requests at all, which differs from "the function has no callers".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallHierarchyNotSupported {
    pub path: String,
}

impl fmt::Display for CallHierarchyNotSupported {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Call hierarchy not supported: the language server running for `{}` doesn't \
             answer call hierarchy requests",
            self.path
        )
    }
}

impl std::error::Error for CallHierarchyNotSupported {}

/// What [`call_hierarchy`] fails with for a name alone when there is no name index to look it
/// up in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameIndexUnavailable {
    pub name: String,
}

impl fmt::Display for NameIndexUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "`{}` can't be looked up by name alone: this project has no name index (it covers C \
             and C++ files when it is turned on)",
            self.name
        )
    }
}

impl std::error::Error for NameIndexUnavailable {}

/// The calls into or out of a function, as a tree.
///
/// In C and C++ files the name index answers next to the language server, and alone when no
/// language server can. Its answers only match names, and every one of them passes the same
/// checks on what may reach an agent as the language server's.
pub fn call_hierarchy(
    project: &Entity<Project>,
    target: CallHierarchyTarget,
    options: CallHierarchyOptions,
    cx: &mut App,
) -> Task<Result<CallHierarchyAnswer>> {
    let options = options.normalized();
    let project = project.clone();
    cx.spawn(async move |cx| {
        let deadline = cx.background_executor().now() + CALL_HIERARCHY_TIME_BUDGET;
        match target {
            CallHierarchyTarget::Position { path, position } => {
                position_answer(&project, path, position, options, deadline, cx).await
            }
            CallHierarchyTarget::Name { name, near } => {
                name_answer(&project, name, near, options, deadline, cx).await
            }
        }
    })
}

async fn position_answer(
    project: &Entity<Project>,
    path: ProjectPath,
    position: LinePosition,
    options: CallHierarchyOptions,
    deadline: Instant,
    cx: &mut AsyncApp,
) -> Result<CallHierarchyAnswer> {
    let target = open_target(project, &path, cx).await?;
    let (point, snapshot) = target.buffer.read_with(cx, |buffer, _| {
        let snapshot = buffer.snapshot();
        resolve_position(&snapshot, &position, &target.display_path).map(|point| (point, snapshot))
    })?;
    let index = if is_indexable_path(path.path.as_unix_str()) {
        IndexAccess::new(project, cx)
    } else {
        None
    };
    let index_building = index.as_ref().and_then(|index| index.building(cx));
    let has_index = index.is_some();

    // With the name index to fall back on, a language server that can't answer is only logged.
    let mut language_server_failure = None;
    let mut language_server_roots = None;
    let language_server_registration = match require_language_server(project, &target, cx) {
        Ok(registration) => Some(registration),
        Err(error) if has_index => {
            language_server_failure = Some(error);
            None
        }
        Err(error) => return Err(error),
    };
    if language_server_registration.is_some() {
        let supported = project.read_with(cx, |project, cx| {
            supports_call_hierarchy(project, &target.buffer, cx)
        });
        let roots = if supported {
            collect_roots(project, &target.buffer, point, deadline, cx)
                .await
                .and_then(|roots| {
                    roots.ok_or_else(|| {
                        anyhow!(
                            "The language server didn't respond within {} seconds",
                            LANGUAGE_SERVER_TIMEOUT.as_secs()
                        )
                    })
                })
        } else {
            Err(anyhow!(CallHierarchyNotSupported {
                path: target.display_path.clone(),
            }))
        };
        match roots {
            Ok(roots) => language_server_roots = Some(roots),
            Err(error) if has_index => language_server_failure = Some(error),
            Err(error) => return Err(error),
        }
    }
    if let Some(error) = &language_server_failure {
        log::info!(
            "call hierarchy for {} answers from the name index only: {error:#}",
            target.display_path
        );
    }

    let language_server_answers = language_server_roots.is_some();
    let mut walk = Walk::new(project.clone(), options, deadline, index, cx);
    walk.language_server_answers = language_server_answers;
    let root = match language_server_roots.and_then(|roots| roots.into_iter().next()) {
        Some(root) => Some(root),
        None if walk.index.is_some() => match walk.index_root(&path, point, &snapshot, cx).await? {
            IndexRoot::Found(root) => Some(root),
            IndexRoot::Ambiguous { name, definitions } => {
                return Ok(CallHierarchyAnswer::Candidates(
                    walk.candidates(name, definitions, index_building, cx).await,
                ));
            }
            IndexRoot::NotFound => None,
        },
        None => None,
    };
    let Some(root) = root else {
        return Ok(CallHierarchyAnswer::NoFunction {
            used_name_index: walk.index.is_some(),
            index_building,
        });
    };
    let mut hierarchy = walk.run(root, cx).await?;
    drop(language_server_registration);
    hierarchy.without_language_server = hierarchy.used_name_index && !language_server_answers;
    hierarchy.index_building = index_building;
    Ok(CallHierarchyAnswer::Found(hierarchy))
}

async fn name_answer(
    project: &Entity<Project>,
    name: String,
    near: Option<ProjectPath>,
    options: CallHierarchyOptions,
    deadline: Instant,
    cx: &mut AsyncApp,
) -> Result<CallHierarchyAnswer> {
    let name = function_name(&name)?;
    let Some(index) = IndexAccess::new(project, cx) else {
        return Err(anyhow!(NameIndexUnavailable { name }));
    };
    let index_building = index.building(cx);
    let mut walk = Walk::new(project.clone(), options, deadline, Some(index), cx);
    let definitions = walk
        .definitions(&name, near, cx)
        .await?
        .ok_or_else(|| anyhow!("The name index didn't answer in time"))?;
    let mut definitions = callable_definitions(definitions);
    if definitions.len() > 1 {
        return Ok(CallHierarchyAnswer::Candidates(
            walk.candidates(name, definitions, index_building, cx).await,
        ));
    }
    let Some(definition) = definitions.pop() else {
        return Ok(CallHierarchyAnswer::NoFunction {
            used_name_index: true,
            index_building,
        });
    };
    let shown = project.read_with(cx, |project, cx| {
        agent_withheld_reason(project, &definition.path, cx).is_none()
    });
    if !shown {
        return Ok(CallHierarchyAnswer::Candidates(
            walk.candidates(name, vec![definition], index_building, cx)
                .await,
        ));
    }
    let position = LinePosition {
        line: definition.name_range.start.row + 1,
        symbol: Some(definition.name.clone()),
        column: None,
    };
    position_answer(project, definition.path, position, options, deadline, cx).await
}

/// A function a source can be asked about again, for the next level. A C or C++ function can
/// carry both handles, and then both sources are asked.
#[derive(Clone, Debug, Default)]
struct CallHandle {
    language_server: Option<CallHierarchyItem>,
    index: Option<IndexHandle>,
}

impl CallHandle {
    fn has_source(&self) -> bool {
        self.language_server.is_some() || self.index.is_some()
    }
}

/// A function as the name index is asked about it. Definition ids go stale when the index is
/// updated, so the function is named by its place instead.
#[derive(Clone, Debug, PartialEq, Eq)]
struct IndexHandle {
    name: String,
    path: ProjectPath,
    /// Where its name starts, 0-based.
    position: Point,
    is_static: bool,
}

/// Where a source put a function.
enum FoundPlace {
    Buffer {
        buffer: Entity<Buffer>,
        range: Range<Anchor>,
        selection_range: Range<Anchor>,
        call_sites: Vec<Location>,
    },
    /// The name index answers from the files on disk without opening them.
    Index {
        path: ProjectPath,
        name_row: u32,
        end_row: u32,
        /// 0-based, in the file of the calls (see [`CallNode::call_lines`]).
        call_rows: Vec<u32>,
        preview_row: u32,
    },
}

/// One function a source reported, before the checks on what may be shown.
struct FoundCall {
    name: String,
    kind: String,
    place: FoundPlace,
    source: CallSource,
    handle: CallHandle,
    ambiguous: Option<usize>,
    resolution: CallResolution,
    /// Listed under this one without being followed.
    candidates: Vec<FoundCall>,
}

impl FoundCall {
    fn from_language_server(item: CallHierarchyItem, call_sites: Vec<Location>) -> Self {
        Self {
            name: item.name.clone(),
            kind: symbol_kind_name(lsp_to_symbol_kind(item.kind)).to_string(),
            place: FoundPlace::Buffer {
                buffer: item.buffer.clone(),
                range: item.range.clone(),
                selection_range: item.selection_range.clone(),
                call_sites,
            },
            source: CallSource::LanguageServer,
            handle: CallHandle {
                language_server: Some(item),
                index: None,
            },
            ambiguous: None,
            resolution: CallResolution::Definition,
            candidates: Vec::new(),
        }
    }

    fn from_definition(
        definition: IndexedDefinition,
        call_rows: Vec<u32>,
        preview_row: u32,
        ambiguous: Option<usize>,
    ) -> Self {
        let name_row = definition.name_range.start.row;
        // A `#define` ends with its line break, at the start of the next line.
        let end = definition.item_range.end;
        let end_row = if end.column == 0 && end.row > name_row {
            end.row - 1
        } else {
            end.row
        };
        Self {
            kind: definition_kind_name(definition.kind).to_string(),
            place: FoundPlace::Index {
                path: definition.path.clone(),
                name_row,
                end_row,
                call_rows,
                preview_row,
            },
            source: if definition.kind.is_macro() {
                CallSource::Macro
            } else {
                CallSource::NameIndex
            },
            handle: CallHandle {
                language_server: None,
                index: Some(IndexHandle {
                    name: definition.name.clone(),
                    path: definition.path,
                    position: definition.name_range.start,
                    is_static: definition.is_static,
                }),
            },
            name: definition.name,
            ambiguous,
            resolution: CallResolution::Definition,
            candidates: Vec::new(),
        }
    }

    /// A call the name index can't lead to a definition, placed at the call itself.
    fn at_call(
        name: String,
        kind: &str,
        path: ProjectPath,
        call_rows: Vec<u32>,
        resolution: CallResolution,
        candidates: Vec<FoundCall>,
    ) -> Self {
        let row = call_rows.iter().min().copied().unwrap_or_default();
        Self {
            name,
            kind: kind.to_string(),
            place: FoundPlace::Index {
                path,
                name_row: row,
                end_row: row,
                call_rows,
                preview_row: row,
            },
            source: CallSource::NameIndex,
            handle: CallHandle::default(),
            ambiguous: None,
            resolution,
            candidates,
        }
    }
}

fn definition_kind_name(kind: DefinitionKind) -> &'static str {
    match kind {
        DefinitionKind::Method => "method",
        DefinitionKind::Macro | DefinitionKind::FunctionMacro => "macro",
        _ => "function",
    }
}

/// The functions and macros among `definitions`: definitions when there are any, otherwise the
/// declarations (a function whose body the index did not see).
fn callable_definitions(definitions: Vec<IndexedDefinition>) -> Vec<IndexedDefinition> {
    let mut callable = definitions
        .into_iter()
        .filter(|definition| definition.kind.is_function_like() || definition.kind.is_macro())
        .collect::<Vec<_>>();
    if callable.iter().any(|definition| !definition.is_declaration) {
        callable.retain(|definition| !definition.is_declaration);
    }
    callable
}

fn is_header_path(path: &ProjectPath) -> bool {
    path.path.extension().is_some_and(|extension| {
        matches!(
            extension,
            "h" | "hh" | "hpp" | "hxx" | "h++" | "H" | "inl" | "ipp" | "tcc"
        )
    })
}

/// Everyone who includes a header can call its `static` functions; a `static` function of a
/// source file can only be called from that file.
fn may_call_from(handle: &IndexHandle, path: &ProjectPath) -> bool {
    !handle.is_static || is_header_path(&handle.path) || handle.path == *path
}

/// The source files other than the handle's that define a `static` function of its name: a call
/// of that name there goes to their own function.
fn files_with_own_static(
    handle: &IndexHandle,
    definitions: Vec<IndexedDefinition>,
) -> HashSet<ProjectPath> {
    definitions
        .into_iter()
        .filter(|definition| {
            definition.is_static
                && definition.kind.is_function_like()
                && !is_header_path(&definition.path)
                && definition.path != handle.path
        })
        .map(|definition| definition.path)
        .collect()
}

/// The name index finds calls by name; the tree lists functions, so the calls are put under the
/// function that contains them.
fn group_callers(
    handle: &IndexHandle,
    calls: Vec<IndexedCallSite>,
    shadowing_files: &HashSet<ProjectPath>,
) -> Vec<FoundCall> {
    let mut groups: Vec<(IndexedDefinition, Vec<u32>)> = Vec::new();
    for call in calls {
        // A member call named like the function goes through a pointer, not to it by name.
        if call.kind != CallKind::Direct
            || !may_call_from(handle, &call.path)
            || shadowing_files.contains(&call.path)
        {
            continue;
        }
        let Some(caller) = call.caller else {
            continue;
        };
        let row = call.range.start.row;
        match groups.iter_mut().find(|(existing, _)| {
            existing.path == caller.path
                && existing.name == caller.name
                && existing.name_range.start.row == caller.name_range.start.row
        }) {
            Some((_, rows)) => rows.push(row),
            None => groups.push((caller, vec![row])),
        }
    }
    groups
        .into_iter()
        .map(|(caller, rows)| {
            let preview_row = rows
                .iter()
                .min()
                .copied()
                .unwrap_or(caller.name_range.start.row);
            FoundCall::from_definition(caller, rows, preview_row, None)
        })
        .collect()
}

/// The calls of one function, by called name, in the order they first appear.
struct CalleeGroup {
    name: String,
    indirect: bool,
    target: CalleeTarget,
    path: ProjectPath,
    rows: Vec<u32>,
}

fn group_callees(sites: Vec<IndexedCalleeSite>) -> Vec<CalleeGroup> {
    let mut groups: Vec<CalleeGroup> = Vec::new();
    for site in sites {
        let indirect = site.kind == CallKind::Member || site.target == CalleeTarget::Indirect;
        let row = site.range.start.row;
        match groups
            .iter_mut()
            .find(|group| group.name == site.callee && group.indirect == indirect)
        {
            Some(group) => group.rows.push(row),
            None => groups.push(CalleeGroup {
                name: site.callee,
                indirect,
                target: site.target,
                path: site.path,
                rows: vec![row],
            }),
        }
    }
    groups
}

/// The index of the status of each visible worktree, summed over those still building.
fn index_building(statuses: &[(project::WorktreeId, SymbolIndexStatus)]) -> Option<IndexBuilding> {
    let mut building = None;
    for (_, status) in statuses {
        if let SymbolIndexStatus::Indexing { done, total } = status {
            let sum = building.get_or_insert(IndexBuilding { done: 0, total: 0 });
            sum.done += done;
            sum.total += total;
        }
    }
    building
}

/// The identifier the position is on, if any.
fn identifier_at(snapshot: &BufferSnapshot, point: Point) -> Option<String> {
    if point.row > snapshot.max_point().row {
        return None;
    }
    let line = snapshot
        .text_for_range(
            Point::new(point.row, 0)..Point::new(point.row, snapshot.line_len(point.row)),
        )
        .collect::<String>();
    let column = (point.column as usize).min(line.len());
    let is_identifier = |character: char| character == '_' || character.is_alphanumeric();
    let before = line.get(..column)?;
    let after = line.get(column..)?;
    let start = column
        - before
            .chars()
            .rev()
            .take_while(|character| is_identifier(*character))
            .map(char::len_utf8)
            .sum::<usize>();
    let end = column
        + after
            .chars()
            .take_while(|character| is_identifier(*character))
            .map(char::len_utf8)
            .sum::<usize>();
    let identifier = line.get(start..end)?;
    (!identifier.is_empty()
        && !identifier.starts_with(|character: char| character.is_ascii_digit()))
    .then(|| identifier.to_string())
}

/// A language server names a C++ method with its class; the name index by its own name.
fn index_name(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

/// The name a name alone is looked up by. The index keeps a C++ method under its own name, so a
/// qualified `A::f` is looked up as `f` (and every class's `f` comes back as a candidate).
fn function_name(text: &str) -> Result<String> {
    let name = index_name(text.trim());
    if name.is_empty()
        || name.chars().count() > MAX_CALL_NAME_CHARS
        || !name
            .chars()
            .all(|character| character == '_' || character.is_alphanumeric())
    {
        return Err(anyhow!(
            "`{}` is not a function's name: give the name alone, like `parse_header` or `Parser::parse`",
            single_line_name(text)
        ));
    }
    Ok(name.to_string())
}

fn same_function(resolved: &CallNode, matched: &CallNode) -> bool {
    resolved.path == matched.path
        && resolved.lines.start_line == matched.lines.start_line
        && index_name(&resolved.name) == matched.name
}

/// `Ok(None)` when the deadline passed first.
async fn collect_roots(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    point: Point,
    deadline: Instant,
    cx: &mut AsyncApp,
) -> Result<Option<Vec<FoundCall>>> {
    let request = project.update(cx, |project, cx| {
        project.prepare_call_hierarchy(buffer, point, cx)
    });
    let Some(items) = within_deadline(request, deadline, cx).await else {
        return Ok(None);
    };
    Ok(Some(
        items?
            .unwrap_or_default()
            .into_iter()
            .map(|item| FoundCall::from_language_server(item, Vec::new()))
            .collect(),
    ))
}

async fn within_deadline<T>(
    request: impl Future<Output = T>,
    deadline: Instant,
    cx: &AsyncApp,
) -> Option<T> {
    let executor = cx.background_executor();
    let remaining = deadline
        .saturating_duration_since(executor.now())
        .min(LANGUAGE_SERVER_TIMEOUT);
    if remaining.is_zero() {
        return None;
    }
    let timer = executor.timer(remaining);
    match future::select(pin!(request), pin!(timer)).await {
        Either::Left((value, _)) => Some(value),
        Either::Right(_) => None,
    }
}

/// A server whose capabilities have not reached this side yet counts as able to answer, so that
/// only a definite "no" is reported as unsupported.
fn supports_call_hierarchy(project: &Project, buffer: &Entity<Buffer>, cx: &App) -> bool {
    let lsp_store = project.lsp_store().read(cx);
    lsp_store
        .relevant_server_ids_for_capability_check(buffer, cx)
        .iter()
        .any(|server_id| {
            lsp_store
                .lsp_server_capabilities
                .get(server_id)
                .is_none_or(|capabilities| match &capabilities.call_hierarchy_provider {
                    Some(lsp::CallHierarchyServerCapability::Simple(supported)) => *supported,
                    Some(lsp::CallHierarchyServerCapability::Options(_)) => true,
                    None => false,
                })
        })
}

/// The name index, with what one call has already looked up in it: the same names come up at
/// every level.
struct IndexAccess {
    store: Entity<SymbolIndexStore>,
    definitions: HashMap<(String, Option<ProjectPath>), Vec<IndexedDefinition>>,
    assignments: HashMap<String, Vec<IndexedAssignment>>,
}

impl IndexAccess {
    /// `None` when the index can't answer: turned off, unavailable, or a remote server that is
    /// not known to have one.
    fn new(project: &Entity<Project>, cx: &AsyncApp) -> Option<Self> {
        let store = project.read_with(cx, |project, _| project.symbol_index_store().clone());
        store
            .read_with(cx, |store, _| store.has_index())
            .then(|| Self {
                store,
                definitions: HashMap::default(),
                assignments: HashMap::default(),
            })
    }

    fn building(&self, cx: &AsyncApp) -> Option<IndexBuilding> {
        self.store
            .read_with(cx, |store, cx| index_building(&store.statuses(cx)))
    }
}

enum IndexRoot {
    Found(FoundCall),
    Ambiguous {
        name: String,
        definitions: Vec<IndexedDefinition>,
    },
    NotFound,
}

type NodeKey = (ProjectPath, u32, String);

struct WalkNode {
    node: CallNode,
    key: NodeKey,
    parent: Option<usize>,
    depth: usize,
    handle: CallHandle,
    /// The row to quote once the file is open; only set when the contents may be shown.
    preview_row: Option<u32>,
}

/// A node before it is placed in the tree.
struct Child {
    node: CallNode,
    handle: CallHandle,
    preview_row: Option<u32>,
    candidates: Vec<Child>,
}

struct Walk {
    project: Entity<Project>,
    options: CallHierarchyOptions,
    deadline: Instant,
    is_remote: bool,
    index: Option<IndexAccess>,
    /// Cleared when the language server stops answering in time and the name index can go on.
    language_server_answers: bool,
    requests: usize,
    /// A question was not asked because the requests ran out.
    out_of_requests: bool,
    /// A question to the name index was not answered before the deadline.
    out_of_time: bool,
    index_truncated: bool,
    /// Called names that were not looked up because the requests ran out.
    skipped_calls: usize,
    files_opened: usize,
    remote_files: HashSet<EntityId>,
}

impl Walk {
    fn new(
        project: Entity<Project>,
        options: CallHierarchyOptions,
        deadline: Instant,
        index: Option<IndexAccess>,
        cx: &AsyncApp,
    ) -> Self {
        let is_remote = project.read_with(cx, |project, _| !project.is_local());
        Self {
            project,
            options: options.normalized(),
            deadline,
            is_remote,
            index,
            language_server_answers: true,
            requests: 0,
            out_of_requests: false,
            out_of_time: false,
            index_truncated: false,
            skipped_calls: 0,
            files_opened: 0,
            remote_files: HashSet::default(),
        }
    }

    fn take_request(&mut self) -> bool {
        if self.requests >= MAX_CALL_HIERARCHY_REQUESTS {
            self.out_of_requests = true;
            false
        } else {
            self.requests += 1;
            true
        }
    }

    /// `Ok(None)` when the requests or the time ran out first.
    async fn definitions(
        &mut self,
        name: &str,
        near: Option<ProjectPath>,
        cx: &mut AsyncApp,
    ) -> Result<Option<Vec<IndexedDefinition>>> {
        let key = (name.to_string(), near);
        let Some(store) = self.index.as_ref().map(|index| index.store.clone()) else {
            return Ok(Some(Vec::new()));
        };
        if let Some(found) = self
            .index
            .as_ref()
            .and_then(|index| index.definitions.get(&key))
        {
            return Ok(Some(found.clone()));
        }
        if !self.take_request() {
            return Ok(None);
        }
        let request = store.update(cx, |store, cx| store.definitions(&key.0, key.1.clone(), cx));
        let Some(found) = within_deadline(request, self.deadline, cx).await else {
            self.out_of_time = true;
            return Ok(None);
        };
        let found = found?;
        if let Some(index) = &mut self.index {
            index.definitions.insert(key, found.clone());
        }
        Ok(Some(found))
    }

    /// `Ok(None)` when the requests or the time ran out first.
    async fn assignments(
        &mut self,
        member: &str,
        cx: &mut AsyncApp,
    ) -> Result<Option<Vec<IndexedAssignment>>> {
        let Some(store) = self.index.as_ref().map(|index| index.store.clone()) else {
            return Ok(Some(Vec::new()));
        };
        if let Some(found) = self
            .index
            .as_ref()
            .and_then(|index| index.assignments.get(member))
        {
            return Ok(Some(found.clone()));
        }
        if !self.take_request() {
            return Ok(None);
        }
        let request = store.update(cx, |store, cx| store.assignments(member, false, cx));
        let Some(found) = within_deadline(request, self.deadline, cx).await else {
            self.out_of_time = true;
            return Ok(None);
        };
        let found = found?;
        if let Some(index) = &mut self.index {
            index.assignments.insert(member.to_string(), found.clone());
        }
        Ok(Some(found))
    }

    /// The root when the language server can't give one: the function named at the position,
    /// or else the one whose body holds it.
    async fn index_root(
        &mut self,
        path: &ProjectPath,
        point: Point,
        snapshot: &BufferSnapshot,
        cx: &mut AsyncApp,
    ) -> Result<IndexRoot> {
        if let Some(name) = identifier_at(snapshot, point) {
            let definitions = self
                .definitions(&name, Some(path.clone()), cx)
                .await?
                .ok_or_else(|| anyhow!("The name index didn't answer in time"))?;
            let mut definitions = callable_definitions(definitions);
            let declared_here = definitions.iter().position(|definition| {
                definition.path == *path && definition.name_range.start.row == point.row
            });
            let chosen = declared_here.or_else(|| {
                if definitions.len() == 1 {
                    Some(0)
                } else {
                    definitions
                        .iter()
                        .position(|definition| definition.path == *path)
                }
            });
            if let Some(chosen) = chosen {
                let definition = definitions.swap_remove(chosen);
                let row = definition.name_range.start.row;
                return Ok(IndexRoot::Found(FoundCall::from_definition(
                    definition,
                    Vec::new(),
                    row,
                    None,
                )));
            }
            if !definitions.is_empty() {
                return Ok(IndexRoot::Ambiguous { name, definitions });
            }
        }
        let Some(store) = self.index.as_ref().map(|index| index.store.clone()) else {
            return Ok(IndexRoot::NotFound);
        };
        if !self.take_request() {
            return Ok(IndexRoot::NotFound);
        }
        let request = store.update(cx, |store, cx| store.callees(path.clone(), point, cx));
        let callees = within_deadline(request, self.deadline, cx)
            .await
            .ok_or_else(|| anyhow!("The name index didn't answer in time"))??;
        Ok(match callees {
            Some(callees) => {
                let row = callees.function.name_range.start.row;
                IndexRoot::Found(FoundCall::from_definition(
                    callees.function,
                    Vec::new(),
                    row,
                    None,
                ))
            }
            None => IndexRoot::NotFound,
        })
    }

    async fn candidates(
        &mut self,
        name: String,
        definitions: Vec<IndexedDefinition>,
        index_building: Option<IndexBuilding>,
        cx: &mut AsyncApp,
    ) -> CallCandidates {
        let total = definitions.len();
        let found = definitions
            .into_iter()
            .take(MAX_CALL_CANDIDATES)
            .map(|definition| {
                let row = definition.name_range.start.row;
                FoundCall::from_definition(definition, Vec::new(), row, None)
            })
            .collect::<Vec<_>>();
        let mut built = self.project.read_with(cx, |project, cx| {
            found
                .iter()
                .filter_map(|found| call_node(project, found, self.options.direction, true, cx))
                .map(|built| (built.node, built.preview_row))
                .collect::<Vec<_>>()
        });
        self.fill_previews(
            built
                .iter_mut()
                .filter_map(|(node, row)| Some((node, (*row)?)))
                .collect(),
            cx,
        )
        .await;
        CallCandidates {
            name,
            candidates: built.into_iter().map(|(node, _)| node).collect(),
            total,
            index_building,
        }
    }

    /// Asks every source the handle has. `Ok(None)` when none could answer before the time or
    /// the requests ran out.
    async fn collect_calls(
        &mut self,
        handle: &CallHandle,
        cx: &mut AsyncApp,
    ) -> Result<Option<Vec<FoundCall>>> {
        let mut found = Vec::new();
        let mut answered = false;
        let mut failure = None;
        if let Some(item) = &handle.language_server
            && self.language_server_answers
        {
            self.requests += 1;
            match self.language_server_calls(item, cx).await {
                Some(Ok(calls)) => {
                    answered = true;
                    found.extend(calls);
                }
                Some(Err(error)) => failure = Some(error),
                None if handle.index.is_some() && self.index.is_some() => {
                    log::info!(
                        "call hierarchy: the language server didn't answer in time; going on with the name index only"
                    );
                    self.language_server_answers = false;
                }
                None => return Ok(None),
            }
        }
        if let Some(index_handle) = &handle.index
            && self.index.is_some()
        {
            match self.index_calls(index_handle, cx).await {
                Ok(Some(calls)) => {
                    answered = true;
                    found.extend(calls);
                }
                Ok(None) => {}
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
        if answered {
            Ok(Some(found))
        } else if let Some(error) = failure {
            Err(error)
        } else {
            Ok(None)
        }
    }

    /// `None` when the deadline passed first.
    async fn language_server_calls(
        &mut self,
        item: &CallHierarchyItem,
        cx: &mut AsyncApp,
    ) -> Option<Result<Vec<FoundCall>>> {
        match self.options.direction {
            CallDirection::Incoming => {
                let request = self
                    .project
                    .update(cx, |project, cx| project.incoming_calls(item.clone(), cx));
                let calls = within_deadline(request, self.deadline, cx).await?;
                Some(calls.map(|calls| {
                    calls
                        .unwrap_or_default()
                        .into_iter()
                        .map(|call| FoundCall::from_language_server(call.from, call.from_ranges))
                        .collect()
                }))
            }
            CallDirection::Outgoing => {
                let request = self
                    .project
                    .update(cx, |project, cx| project.outgoing_calls(item.clone(), cx));
                let calls = within_deadline(request, self.deadline, cx).await?;
                Some(calls.map(|calls| {
                    calls
                        .unwrap_or_default()
                        .into_iter()
                        .map(|call| FoundCall::from_language_server(call.to, call.from_ranges))
                        .collect()
                }))
            }
        }
    }

    /// `Ok(None)` when the requests or the time ran out first.
    async fn index_calls(
        &mut self,
        handle: &IndexHandle,
        cx: &mut AsyncApp,
    ) -> Result<Option<Vec<FoundCall>>> {
        let Some(store) = self.index.as_ref().map(|index| index.store.clone()) else {
            return Ok(Some(Vec::new()));
        };
        if !self.take_request() {
            return Ok(None);
        }
        match self.options.direction {
            CallDirection::Incoming => {
                let request = store.update(cx, |store, cx| {
                    store.callers(&handle.name, MAX_CALLERS_LIMIT, cx)
                });
                let Some(callers) = within_deadline(request, self.deadline, cx).await else {
                    self.out_of_time = true;
                    return Ok(None);
                };
                let callers = callers?;
                self.index_truncated |= callers.truncated;
                let shadowing_files = self.shadowing_files(handle, &callers.calls, cx).await;
                Ok(Some(group_callers(handle, callers.calls, &shadowing_files)))
            }
            CallDirection::Outgoing => {
                let request = store.update(cx, |store, cx| {
                    store.callees(handle.path.clone(), handle.position, cx)
                });
                let Some(callees) = within_deadline(request, self.deadline, cx).await else {
                    self.out_of_time = true;
                    return Ok(None);
                };
                match callees? {
                    // The position lies in another function once the file has changed.
                    Some(callees) if callees.function.name == handle.name => self
                        .callee_calls(handle, callees.callees, cx)
                        .await
                        .map(Some),
                    _ => Ok(Some(Vec::new())),
                }
            }
        }
    }

    /// Callers are looked up by name, so a function that is not `static` would also get the
    /// calls other source files make to their own `static` function of its name.
    async fn shadowing_files(
        &mut self,
        handle: &IndexHandle,
        calls: &[IndexedCallSite],
        cx: &mut AsyncApp,
    ) -> HashSet<ProjectPath> {
        let static_in_own_file = handle.is_static && !is_header_path(&handle.path);
        if static_in_own_file || calls.iter().all(|call| call.path == handle.path) {
            return HashSet::default();
        }
        // With a place to be near, the store would leave out the other files' `static` functions.
        match self.definitions(&handle.name, None, cx).await {
            Ok(Some(definitions)) => files_with_own_static(handle, definitions),
            Ok(None) => HashSet::default(),
            Err(error) => {
                log::warn!(
                    "call hierarchy: looking up the static functions named {}: {error:#}",
                    handle.name
                );
                HashSet::default()
            }
        }
    }

    async fn callee_calls(
        &mut self,
        handle: &IndexHandle,
        sites: Vec<IndexedCalleeSite>,
        cx: &mut AsyncApp,
    ) -> Result<Vec<FoundCall>> {
        let mut found = Vec::new();
        for group in group_callees(sites) {
            if group.indirect {
                let Some(assignments) = self.assignments(&group.name, cx).await? else {
                    self.skipped_calls += 1;
                    continue;
                };
                let mut candidates: Vec<FoundCall> = Vec::new();
                for assignment in assignments {
                    if candidates.len() >= MAX_INDIRECT_CANDIDATES {
                        break;
                    }
                    if candidates
                        .iter()
                        .any(|candidate| candidate.name == assignment.function)
                    {
                        continue;
                    }
                    let row = assignment.range.start.row;
                    candidates.push(FoundCall {
                        name: assignment.function,
                        kind: "function".to_string(),
                        place: FoundPlace::Index {
                            path: assignment.path,
                            name_row: row,
                            end_row: row,
                            call_rows: Vec::new(),
                            preview_row: row,
                        },
                        source: CallSource::Candidate,
                        handle: CallHandle::default(),
                        ambiguous: None,
                        resolution: CallResolution::Definition,
                        candidates: Vec::new(),
                    });
                }
                found.push(FoundCall::at_call(
                    group.name.clone(),
                    "member call",
                    group.path,
                    group.rows,
                    CallResolution::Indirect { member: group.name },
                    candidates,
                ));
                continue;
            }
            let definitions = if group.target == CalleeTarget::Unresolved {
                Vec::new()
            } else {
                let Some(definitions) = self
                    .definitions(&group.name, Some(handle.path.clone()), cx)
                    .await?
                else {
                    self.skipped_calls += 1;
                    continue;
                };
                callable_definitions(definitions)
            };
            if definitions.is_empty() {
                found.push(FoundCall::at_call(
                    group.name,
                    "function",
                    group.path,
                    group.rows,
                    CallResolution::NoDefinition,
                    Vec::new(),
                ));
                continue;
            }
            let total = definitions.len();
            let ambiguous = (total > 1).then_some(total);
            for definition in definitions.into_iter().take(MAX_AMBIGUOUS_DEFINITIONS) {
                let row = definition.name_range.start.row;
                found.push(FoundCall::from_definition(
                    definition,
                    group.rows.clone(),
                    row,
                    ambiguous,
                ));
            }
        }
        Ok(found)
    }

    /// Every node goes through [`call_node`], whatever its source: the name index checks its
    /// results against the settings, but what an agent may read is decided here.
    fn child(
        &self,
        project: &Project,
        found: &FoundCall,
        is_root: bool,
        cx: &App,
    ) -> Option<Child> {
        let built = call_node(project, found, self.options.direction, is_root, cx)?;
        let mut handle = found.handle.clone();
        // The name index is asked about a C or C++ function the language server found, too.
        if handle.index.is_none()
            && found.source == CallSource::LanguageServer
            && self.index.is_some()
            && built.node.contents_allowed
            && !built.node.name.is_empty()
            && is_indexable_path(built.node.path.path.as_unix_str())
        {
            handle.index = Some(IndexHandle {
                name: index_name(&built.node.name).to_string(),
                path: built.node.path.clone(),
                position: built.name_point,
                is_static: false,
            });
        }
        let candidates = found
            .candidates
            .iter()
            .filter_map(|candidate| self.child(project, candidate, false, cx))
            .collect();
        Some(Child {
            node: built.node,
            handle,
            preview_row: built.preview_row,
            candidates,
        })
    }

    /// Whether the root's function is `static` decides whose calls can reach it, and only the
    /// name index knows; the language server's root is matched to the index's definition.
    async fn match_root_to_index(&mut self, handle: &mut CallHandle, cx: &mut AsyncApp) {
        let Some(index_handle) = &handle.index else {
            return;
        };
        let (name, path, row) = (
            index_handle.name.clone(),
            index_handle.path.clone(),
            index_handle.position.row,
        );
        match self.definitions(&name, Some(path.clone()), cx).await {
            Ok(Some(definitions)) => {
                if let Some(definition) = definitions.into_iter().find(|definition| {
                    definition.path == path && definition.name_range.start.row == row
                }) {
                    handle.index = Some(IndexHandle {
                        name: definition.name,
                        path: definition.path,
                        position: definition.name_range.start,
                        is_static: definition.is_static,
                    });
                }
            }
            Ok(None) => {}
            Err(error) => {
                log::warn!("call hierarchy: looking up {name} in the name index: {error:#}")
            }
        }
    }

    async fn run(&mut self, root: FoundCall, cx: &mut AsyncApp) -> Result<CallHierarchy> {
        let root = self
            .project
            .read_with(cx, |project, cx| self.child(project, &root, true, cx))
            .ok_or_else(|| anyhow!("The language server named a function that isn't in a file"))?;
        let Child {
            node: root_node,
            mut handle,
            preview_row,
            candidates: _,
        } = root;
        if handle.language_server.is_some() {
            self.match_root_to_index(&mut handle, cx).await;
        }
        let root_key = node_key(&root_node);
        let mut nodes = vec![WalkNode {
            node: root_node,
            key: root_key.clone(),
            parent: None,
            depth: 0,
            handle,
            preview_row,
        }];
        let mut queue = VecDeque::new();
        let mut queued_keys = HashSet::default();
        if let Some(root) = nodes.first_mut() {
            if !root.node.contents_allowed {
                root.node.expansion = CallExpansion::Withheld;
            } else if !root.handle.has_source() {
                root.node.expansion = CallExpansion::DepthLimit;
            } else {
                root.node.expansion = CallExpansion::Stopped;
                queue.push_back(0);
                queued_keys.insert(root_key);
            }
        }

        let mut listed = 0;
        let mut omitted = 0;
        let mut stopped = None;
        while let Some(index) = queue.pop_front() {
            if stopped.is_none() {
                stopped = walk_stop(
                    self.requests,
                    listed,
                    self.options.max_results,
                    self.is_remote.then_some(self.remote_files.len()),
                );
            }
            let Some((handle, child_depth)) = nodes
                .get(index)
                .map(|walk_node| (walk_node.handle.clone(), walk_node.depth + 1))
            else {
                continue;
            };
            if stopped.is_some() {
                set_expansion(&mut nodes, index, CallExpansion::Stopped);
                continue;
            }
            let found = self.collect_calls(&handle, cx).await;
            if stopped.is_none() {
                if self.out_of_requests {
                    stopped = Some(CallWalkStop::RequestLimit);
                } else if self.out_of_time {
                    stopped = Some(CallWalkStop::TimeLimit);
                }
            }
            let found = match found {
                Ok(Some(found)) => found,
                Ok(None) => {
                    stopped.get_or_insert(CallWalkStop::TimeLimit);
                    set_expansion(&mut nodes, index, CallExpansion::Stopped);
                    continue;
                }
                Err(error) => {
                    log::warn!("call hierarchy request failed: {error:#}");
                    set_expansion(&mut nodes, index, CallExpansion::Failed);
                    continue;
                }
            };
            set_expansion(&mut nodes, index, CallExpansion::Expanded);
            // The remote host has already opened and sent every file of the answer, including the
            // calls left out below, so all of them count.
            if self.is_remote {
                self.remote_files
                    .extend(found.iter().filter_map(|found| match &found.place {
                        FoundPlace::Buffer { buffer, .. } => Some(buffer.entity_id()),
                        FoundPlace::Index { .. } => None,
                    }));
            }

            let children = self.project.read_with(cx, |project, cx| {
                found
                    .iter()
                    .filter_map(|found| self.child(project, found, false, cx))
                    .collect::<Vec<_>>()
            });
            let children = merge_children(children, self.options.direction);

            for child in children {
                if listed >= self.options.max_results {
                    omitted += 1 + child.candidates.len();
                    continue;
                }
                listed += 1;
                let Child {
                    mut node,
                    handle,
                    preview_row,
                    candidates,
                } = child;
                let key = node_key(&node);
                let new_index = nodes.len();
                let expandable = child_depth < self.options.depth;
                node.expansion = if is_ancestor(&nodes, index, &key) {
                    CallExpansion::Recursive
                } else if !expandable {
                    CallExpansion::DepthLimit
                } else if !node.contents_allowed {
                    CallExpansion::Withheld
                } else if !handle.has_source() {
                    CallExpansion::DepthLimit
                } else if queued_keys.insert(key.clone()) {
                    queue.push_back(new_index);
                    CallExpansion::Stopped
                } else {
                    CallExpansion::Repeated
                };
                nodes.push(WalkNode {
                    node,
                    key,
                    parent: Some(index),
                    depth: child_depth,
                    handle,
                    preview_row,
                });
                for candidate in candidates {
                    if listed >= self.options.max_results {
                        omitted += 1;
                        continue;
                    }
                    listed += 1;
                    let mut candidate_node = candidate.node;
                    candidate_node.expansion = if candidate_node.contents_allowed {
                        CallExpansion::DepthLimit
                    } else {
                        CallExpansion::Withheld
                    };
                    nodes.push(WalkNode {
                        key: node_key(&candidate_node),
                        node: candidate_node,
                        parent: Some(new_index),
                        depth: child_depth + 1,
                        handle: CallHandle::default(),
                        preview_row: candidate.preview_row,
                    });
                }
            }
        }
        omitted += self.skipped_calls;

        self.fill_previews(
            nodes
                .iter_mut()
                .filter_map(|walk_node| Some((&mut walk_node.node, walk_node.preview_row?)))
                .collect(),
            cx,
        )
        .await;

        Ok(CallHierarchy {
            direction: self.options.direction,
            depth: self.options.depth,
            max_results: self.options.max_results,
            root: build_tree(nodes)?,
            omitted,
            stopped,
            used_name_index: self.index.is_some(),
            without_language_server: false,
            index_truncated: self.index_truncated,
            index_building: None,
        })
    }

    /// Quotes the lines the name index pointed at. A file is opened only for a node whose
    /// contents may be shown, and at most [`MAX_PREVIEW_FILES`] files are opened.
    async fn fill_previews(&mut self, pending: Vec<(&mut CallNode, u32)>, cx: &mut AsyncApp) {
        let mut buffers = HashMap::<ProjectPath, Option<Entity<Buffer>>>::default();
        for (node, row) in pending {
            if !node.contents_allowed || !node.preview.is_empty() {
                continue;
            }
            let buffer = match buffers.get(&node.path) {
                Some(buffer) => buffer.clone(),
                None => {
                    let buffer = self.open_for_preview(&node.path, cx).await;
                    buffers.insert(node.path.clone(), buffer.clone());
                    buffer
                }
            };
            if let Some(buffer) = buffer {
                node.preview = buffer.read_with(cx, |buffer, _| line_preview(buffer, row));
            }
        }
    }

    async fn open_for_preview(
        &mut self,
        path: &ProjectPath,
        cx: &mut AsyncApp,
    ) -> Option<Entity<Buffer>> {
        if let Some(buffer) = self
            .project
            .read_with(cx, |project, cx| project.get_open_buffer(path, cx))
        {
            return Some(buffer);
        }
        if self.files_opened >= MAX_PREVIEW_FILES
            || (self.is_remote && self.remote_files.len() >= MAX_REMOTE_CALL_HIERARCHY_FILES)
        {
            return None;
        }
        self.files_opened += 1;
        let request = self
            .project
            .update(cx, |project, cx| project.open_buffer(path.clone(), cx));
        match within_deadline(request, self.deadline, cx).await {
            Some(Ok(buffer)) => {
                if self.is_remote {
                    self.remote_files.insert(buffer.entity_id());
                }
                Some(buffer)
            }
            Some(Err(error)) => {
                log::debug!("call hierarchy: couldn't open a file to quote: {error:#}");
                None
            }
            None => None,
        }
    }
}

/// The language server's calls first, in the order of the tree, then what only the name index
/// found. A function both found is the language server's, with the calls of both.
fn merge_children(children: Vec<Child>, direction: CallDirection) -> Vec<Child> {
    let (mut resolved, matched): (Vec<_>, Vec<_>) = children
        .into_iter()
        .partition(|child| child.node.source == CallSource::LanguageServer);
    sort_and_merge_duplicates(&mut resolved);
    let mut index_only = Vec::new();
    for child in matched {
        if child.node.resolution == CallResolution::Definition
            && let Some(same) = resolved
                .iter_mut()
                .find(|resolved| same_function(&resolved.node, &child.node))
        {
            // An incoming call quotes its first call, which may be one only the index knew.
            if direction == CallDirection::Incoming
                && let Some(first) = child.node.call_lines.first()
                && same.node.call_lines.first().is_none_or(|kept| first < kept)
            {
                same.node.preview = child.node.preview;
                same.preview_row = child.preview_row;
            }
            same.node.call_lines.extend(child.node.call_lines);
            same.node.call_lines.sort_unstable();
            same.node.call_lines.dedup();
            if child.handle.index.is_some() {
                same.handle.index = child.handle.index;
            }
            continue;
        }
        // The language server resolved this called name; other functions of the name are not
        // what the call reaches.
        if direction == CallDirection::Outgoing
            && !matches!(child.node.resolution, CallResolution::Indirect { .. })
            && !child.node.name.is_empty()
            && resolved
                .iter()
                .any(|resolved| index_name(&resolved.node.name) == child.node.name)
        {
            continue;
        }
        index_only.push(child);
    }
    sort_and_merge_duplicates(&mut index_only);
    resolved.extend(index_only);
    resolved
}

fn sort_and_merge_duplicates(children: &mut Vec<Child>) {
    children.sort_by(|left, right| {
        (&left.node.path, left.node.lines.start_line, &left.node.name).cmp(&(
            &right.node.path,
            right.node.lines.start_line,
            &right.node.name,
        ))
    });
    // Two servers for one file each report the same function.
    children.dedup_by(|duplicate, kept| {
        let same = node_key(&duplicate.node) == node_key(&kept.node)
            && duplicate.node.resolution == kept.node.resolution;
        if same {
            kept.node.call_lines.append(&mut duplicate.node.call_lines);
            kept.node.call_lines.sort_unstable();
            kept.node.call_lines.dedup();
        }
        same
    });
}

/// `remote_files` is `None` for a local project.
fn walk_stop(
    requests: usize,
    listed: usize,
    max_results: usize,
    remote_files: Option<usize>,
) -> Option<CallWalkStop> {
    if requests >= MAX_CALL_HIERARCHY_REQUESTS {
        Some(CallWalkStop::RequestLimit)
    } else if listed >= max_results {
        Some(CallWalkStop::ResultLimit)
    } else if remote_files.is_some_and(|files| files >= MAX_REMOTE_CALL_HIERARCHY_FILES) {
        Some(CallWalkStop::RemoteFileLimit)
    } else {
        None
    }
}

fn set_expansion(nodes: &mut [WalkNode], index: usize, expansion: CallExpansion) {
    if let Some(walk_node) = nodes.get_mut(index) {
        walk_node.node.expansion = expansion;
    }
}

fn is_ancestor(nodes: &[WalkNode], parent: usize, key: &NodeKey) -> bool {
    let mut current = Some(parent);
    while let Some(index) = current {
        let Some(walk_node) = nodes.get(index) else {
            return false;
        };
        if walk_node.key == *key {
            return true;
        }
        current = walk_node.parent;
    }
    false
}

/// Children always come after their parent, so taking the nodes from the back completes each
/// node's children before the node itself moves into its parent.
fn build_tree(nodes: Vec<WalkNode>) -> Result<CallNode> {
    let parents = nodes
        .iter()
        .map(|walk_node| walk_node.parent)
        .collect::<Vec<_>>();
    let mut built = nodes
        .into_iter()
        .map(|walk_node| Some(walk_node.node))
        .collect::<Vec<_>>();
    for index in (1..built.len()).rev() {
        let Some(mut node) = built.get_mut(index).and_then(Option::take) else {
            continue;
        };
        node.children.reverse();
        if let Some(Some(parent)) = parents
            .get(index)
            .copied()
            .flatten()
            .and_then(|parent| built.get_mut(parent))
        {
            parent.children.push(node);
        }
    }
    let mut root = built
        .into_iter()
        .next()
        .flatten()
        .ok_or_else(|| anyhow!("The call hierarchy has no root"))?;
    root.children.reverse();
    Ok(root)
}

/// The name is keyed as the index names it: a language server can name the same C++ method
/// `A::f` that the index calls `f`, and the walk has to see one function in both.
fn node_key(node: &CallNode) -> NodeKey {
    (
        node.path.clone(),
        node.lines.start_line,
        index_name(&node.name).to_string(),
    )
}

struct BuiltNode {
    node: CallNode,
    /// Set when the preview must be read from a file that is not open yet.
    preview_row: Option<u32>,
    name_point: Point,
}

fn call_node(
    project: &Project,
    found: &FoundCall,
    direction: CallDirection,
    is_root: bool,
    cx: &App,
) -> Option<BuiltNode> {
    let (path, name_point, end_row, call_lines, file_is_private) = match &found.place {
        FoundPlace::Buffer {
            buffer,
            range,
            selection_range,
            call_sites,
        } => {
            let buffer = buffer.read(cx);
            let file = buffer.file()?;
            let name_point = selection_range.start.to_point(buffer);
            let call_lines = call_sites
                .iter()
                .map(|site| site.range.start.to_point(site.buffer.read(cx)).row + 1)
                .collect::<Vec<_>>();
            (
                ProjectPath::from_file(file.as_ref(), cx),
                name_point,
                range.end.to_point(buffer).row,
                call_lines,
                file.is_private(),
            )
        }
        FoundPlace::Index {
            path,
            name_row,
            end_row,
            call_rows,
            ..
        } => {
            let file_is_private = project
                .get_open_buffer(path, cx)
                .and_then(|buffer| buffer.read(cx).file().map(|file| file.is_private()))
                .unwrap_or(false);
            (
                path.clone(),
                Point::new(*name_row, 0),
                *end_row,
                call_rows.iter().map(|row| row + 1).collect(),
                file_is_private,
            )
        }
    };
    let name_row = name_point.row;
    let end_row = end_row.max(name_row);
    let in_project = is_in_project(project, &path, cx);
    let is_withheld = file_is_private || withheld_reason(project, &path, cx).is_some();
    let contents_allowed = in_project && !is_withheld;

    let mut call_lines = call_lines;
    call_lines.sort_unstable();
    call_lines.dedup();

    let mut preview = String::new();
    let mut preview_row = None;
    if contents_allowed {
        match &found.place {
            FoundPlace::Buffer {
                buffer, call_sites, ..
            } => {
                let row = if !is_root && direction == CallDirection::Incoming {
                    call_sites
                        .iter()
                        .filter(|site| site.buffer.entity_id() == buffer.entity_id())
                        .map(|site| site.range.start.to_point(buffer.read(cx)).row)
                        .min()
                        .unwrap_or(name_row)
                } else {
                    name_row
                };
                preview = line_preview(buffer.read(cx), row);
            }
            FoundPlace::Index {
                preview_row: row, ..
            } => match project.get_open_buffer(&path, cx) {
                Some(buffer) => preview = line_preview(buffer.read(cx), *row),
                None => preview_row = Some(*row),
            },
        }
    }

    Some(BuiltNode {
        node: CallNode {
            name: if is_withheld {
                String::new()
            } else {
                single_line_name(&found.name)
            },
            kind: found.kind.clone(),
            path,
            lines: LineRange {
                start_line: name_row + 1,
                end_line: end_row + 1,
            },
            call_lines,
            in_project,
            contents_allowed,
            preview,
            source: found.source,
            ambiguous: found.ambiguous,
            resolution: if is_withheld {
                match &found.resolution {
                    CallResolution::Indirect { .. } => CallResolution::Indirect {
                        member: String::new(),
                    },
                    other => other.clone(),
                }
            } else {
                found.resolution.clone()
            },
            expansion: CallExpansion::DepthLimit,
            children: Vec::new(),
        },
        preview_row,
        name_point,
    })
}

fn single_line_name(name: &str) -> String {
    let name = name
        .split(char::is_control)
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    truncate_chars(&name, MAX_CALL_NAME_CHARS)
}

fn line_preview(buffer: &Buffer, row: u32) -> String {
    if row > buffer.max_point().row {
        return String::new();
    }
    // A minified file can hold megabytes on one line; stop reading once the preview is full.
    let mut line = String::new();
    for chunk in buffer.text_for_range(Point::new(row, 0)..Point::new(row, buffer.line_len(row))) {
        if line.is_empty() {
            line.push_str(chunk.trim_start());
        } else {
            line.push_str(chunk);
        }
        if line.len() > CALL_PREVIEW_CHARS * 4 {
            break;
        }
    }
    truncate_chars(line.trim(), CALL_PREVIEW_CHARS)
}

#[cfg(test)]
#[path = "call_hierarchy_index_tests.rs"]
mod index_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LanguageServerNotRunning;
    use futures::{FutureExt as _, StreamExt as _};
    use gpui::{TestAppContext, UpdateGlobal as _};
    use language::{FakeLspAdapter, rust_lang};
    use project::{FakeFs, lsp_store::OpenLspBufferHandle};
    use serde_json::json;
    use settings::{SettingsStore, SplicingVec};
    use std::sync::{Arc, Mutex};
    use util::{path, rel_path::rel_path};

    const LIB: &str = "fn leaf() {}\nfn middle() {\n    leaf();\n}\nfn top() {\n    middle();\n    top();\n}\nfn other() {\n    leaf();\n    middle();\n}\n";
    const SECRET: &str = "fn hidden() {\n    leaf();\n    middle();\n}\n";
    const DEPENDENCY: &str = "fn dep_caller() {\n    leaf();\n}\n";
    const GENERATED: &str = "fn generated() {\n    leaf();\n}\n";
    const LINKED: &str = "fn linked() {\n    leaf();\n}\n";
    const WIDE_CALLERS: u32 = 90;

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

    async fn test_project(cx: &mut TestAppContext) -> Entity<Project> {
        let wide = (0..WIDE_CALLERS + 2)
            .map(|index| format!("fn wide_{index}() {{}}\n"))
            .collect::<String>();
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "src": {"lib.rs": LIB, "wide.rs": wide},
                "secret.rs": SECRET,
                "excluded": {"gen.rs": GENERATED},
            }),
        )
        .await;
        fs.insert_tree(path!("/deps"), json!({"dep.rs": DEPENDENCY}))
            .await;
        fs.insert_tree(path!("/outside"), json!({"linked.rs": LINKED}))
            .await;
        fs.insert_symlink(path!("/root/link"), path!("/outside").into())
            .await;
        let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
        project.read_with(cx, |project, _| project.languages().add(rust_lang()));
        cx.executor().run_until_parked();
        project
    }

    async fn start_language_server(
        project: &Entity<Project>,
        supports_call_hierarchy: bool,
        cx: &mut TestAppContext,
    ) -> (lsp::FakeLanguageServer, OpenLspBufferHandle) {
        let language_registry = project.read_with(cx, |project, _| project.languages().clone());
        let mut fake_servers = language_registry.register_fake_lsp(
            "Rust",
            FakeLspAdapter {
                capabilities: lsp::ServerCapabilities {
                    call_hierarchy_provider: supports_call_hierarchy
                        .then_some(lsp::CallHierarchyServerCapability::Simple(true)),
                    ..lsp::ServerCapabilities::default()
                },
                ..FakeLspAdapter::default()
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

    fn lsp_range(start_row: u32, start_column: u32, end_row: u32, end_column: u32) -> lsp::Range {
        lsp::Range::new(
            lsp::Position::new(start_row, start_column),
            lsp::Position::new(end_row, end_column),
        )
    }

    fn function(name: &str) -> lsp::CallHierarchyItem {
        let (abs_path, row, end_row) = match name {
            "leaf" => (path!("/root/src/lib.rs"), 0, 0),
            "middle" => (path!("/root/src/lib.rs"), 1, 3),
            "top" => (path!("/root/src/lib.rs"), 4, 7),
            "other" => (path!("/root/src/lib.rs"), 8, 11),
            "hidden" => (path!("/root/secret.rs"), 0, 3),
            "dep_caller" => (path!("/deps/dep.rs"), 0, 2),
            "generated" => (path!("/root/excluded/gen.rs"), 0, 2),
            "linked" => (path!("/root/link/linked.rs"), 0, 2),
            wide => {
                let index = wide
                    .strip_prefix("wide_")
                    .and_then(|index| index.parse::<u32>().ok())
                    .expect("a function of the test");
                (path!("/root/src/wide.rs"), index, index)
            }
        };
        let name_length = name.len() as u32;
        lsp::CallHierarchyItem {
            name: name.to_string(),
            kind: lsp::SymbolKind::FUNCTION,
            tags: None,
            detail: None,
            uri: lsp::Uri::from_file_path(abs_path).expect("an absolute path"),
            range: lsp_range(row, 0, end_row, 1),
            selection_range: lsp_range(row, 3, row, 3 + name_length),
            data: None,
        }
    }

    /// Who calls whom, with the 0-based rows of the calls in the caller's file.
    fn calls_into(name: &str) -> Vec<(String, Vec<u32>)> {
        let calls: &[(&str, &[u32])] = match name {
            "leaf" => &[
                ("middle", &[2]),
                ("other", &[9]),
                ("hidden", &[1]),
                ("dep_caller", &[1]),
                ("generated", &[1]),
                ("linked", &[1]),
            ],
            "middle" => &[("top", &[5]), ("other", &[10]), ("hidden", &[2])],
            "top" => &[("top", &[6])],
            "wide_0" => &[("wide_1", &[1])],
            "wide_1" => {
                return (0..WIDE_CALLERS)
                    .map(|index| (format!("wide_{}", index + 2), vec![index + 2]))
                    .collect();
            }
            _ => &[],
        };
        calls
            .iter()
            .map(|(caller, rows)| (caller.to_string(), rows.to_vec()))
            .collect()
    }

    fn calls_out_of(name: &str) -> Vec<(String, Vec<u32>)> {
        let calls: &[(&str, &[u32])] = match name {
            "top" => &[("middle", &[5]), ("top", &[6])],
            "middle" => &[("leaf", &[2])],
            "other" => &[
                ("leaf", &[9]),
                ("middle", &[10]),
                ("hidden", &[10]),
                ("linked", &[10]),
            ],
            "hidden" => &[("leaf", &[1]), ("middle", &[2])],
            _ => &[],
        };
        calls
            .iter()
            .map(|(callee, rows)| (callee.to_string(), rows.to_vec()))
            .collect()
    }

    fn call_ranges(rows: &[u32]) -> Vec<lsp::Range> {
        rows.iter().map(|row| lsp_range(*row, 4, *row, 8)).collect()
    }

    /// Records the function each incoming or outgoing request asks about. The prepare request
    /// picks the function by the row it is asked at; row 9 of `lib.rs` (a call in `other`)
    /// resolves to `hidden`, a function in a private file.
    fn serve_call_graph(
        server: &lsp::FakeLanguageServer,
        stalled: Option<&'static str>,
    ) -> Arc<Mutex<Vec<String>>> {
        let asked = Arc::new(Mutex::new(Vec::new()));
        server.set_request_handler::<lsp::request::CallHierarchyPrepare, _, _>(
            |params, _| async move {
                let position = params.text_document_position_params;
                let name = if position.text_document.uri.as_str().ends_with("wide.rs") {
                    "wide_0"
                } else {
                    match position.position.line {
                        0 => "leaf",
                        1 => "middle",
                        4 => "top",
                        8 => "other",
                        9 => "hidden",
                        _ => return Ok(None),
                    }
                };
                Ok(Some(vec![function(name)]))
            },
        );
        server.set_request_handler::<lsp::request::CallHierarchyIncomingCalls, _, _>({
            let asked = asked.clone();
            move |params, _| {
                let asked = asked.clone();
                async move {
                    let name = params.item.name;
                    asked.lock().expect("not poisoned").push(name.clone());
                    if stalled == Some(name.as_str()) {
                        futures::future::pending::<()>().await;
                    }
                    Ok(Some(
                        calls_into(&name)
                            .into_iter()
                            .map(|(caller, rows)| lsp::CallHierarchyIncomingCall {
                                from: function(&caller),
                                from_ranges: call_ranges(&rows),
                            })
                            .collect(),
                    ))
                }
            }
        });
        server.set_request_handler::<lsp::request::CallHierarchyOutgoingCalls, _, _>({
            let asked = asked.clone();
            move |params, _| {
                let asked = asked.clone();
                async move {
                    let name = params.item.name;
                    asked.lock().expect("not poisoned").push(name.clone());
                    Ok(Some(
                        calls_out_of(&name)
                            .into_iter()
                            .map(|(callee, rows)| lsp::CallHierarchyOutgoingCall {
                                to: function(&callee),
                                from_ranges: call_ranges(&rows),
                            })
                            .collect(),
                    ))
                }
            }
        });
        asked
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

    fn query(
        project: &Entity<Project>,
        path: &str,
        line: u32,
        direction: CallDirection,
        depth: usize,
        max_results: usize,
        cx: &mut TestAppContext,
    ) -> Task<Result<Option<CallHierarchy>>> {
        let path = project_path(project, path, cx);
        let task = cx.update(|cx| {
            call_hierarchy(
                project,
                CallHierarchyTarget::Position {
                    path,
                    position: LinePosition {
                        line,
                        symbol: None,
                        column: None,
                    },
                },
                CallHierarchyOptions {
                    direction,
                    depth,
                    max_results,
                },
                cx,
            )
        });
        cx.executor().spawn(async move {
            match task.await? {
                CallHierarchyAnswer::Found(hierarchy) => {
                    assert!(!hierarchy.used_name_index, "Rust has no name index");
                    Ok(Some(hierarchy))
                }
                CallHierarchyAnswer::NoFunction {
                    used_name_index,
                    index_building,
                } => {
                    assert!(!used_name_index, "Rust has no name index");
                    assert_eq!(index_building, None);
                    Ok(None)
                }
                CallHierarchyAnswer::Candidates(candidates) => {
                    Err(anyhow!("unexpected candidates: {candidates:?}"))
                }
            }
        })
    }

    /// One line per node: name (`-` when withheld), place, call lines, preview and how far it
    /// was followed.
    fn outline(node: &CallNode) -> Vec<String> {
        fn visit(node: &CallNode, indent: usize, lines: &mut Vec<String>) {
            let place = if node.in_project {
                node.path.path.as_unix_str().to_string()
            } else {
                "<outside>".to_string()
            };
            lines.push(format!(
                "{}{} {place}:{}-{} {:?} {:?} {:?}{}",
                "  ".repeat(indent),
                if node.name.is_empty() {
                    "-"
                } else {
                    &node.name
                },
                node.lines.start_line,
                node.lines.end_line,
                node.call_lines,
                node.preview,
                node.expansion,
                if node.contents_allowed {
                    ""
                } else {
                    " withheld"
                },
            ));
            for child in &node.children {
                visit(child, indent + 1, lines);
            }
        }
        let mut lines = Vec::new();
        visit(node, 0, &mut lines);
        lines
    }

    fn asked(asked: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        asked.lock().expect("not poisoned").clone()
    }

    #[gpui::test]
    async fn test_incoming_calls_one_level(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;
        let (server, _registration) = start_language_server(&project, true, cx).await;
        let requests = serve_call_graph(&server, None);

        let hierarchy = query(&project, "src/lib.rs", 1, CallDirection::Incoming, 1, 0, cx)
            .await
            .expect("the query succeeds")
            .expect("there is a function on line 1");
        assert_eq!(
            outline(&hierarchy.root),
            [
                "leaf src/lib.rs:1-1 [] \"fn leaf() {}\" Expanded",
                "  - excluded/gen.rs:1-3 [2] \"\" DepthLimit withheld",
                "  linked <outside>:1-3 [2] \"\" DepthLimit withheld",
                "  - secret.rs:1-4 [2] \"\" DepthLimit withheld",
                "  middle src/lib.rs:2-4 [3] \"leaf();\" DepthLimit",
                "  other src/lib.rs:9-12 [10] \"leaf();\" DepthLimit",
                "  dep_caller <outside>:1-3 [2] \"\" DepthLimit withheld",
            ]
        );
        assert_eq!(hierarchy.root.source, CallSource::LanguageServer);
        assert_eq!(hierarchy.omitted, 0);
        assert_eq!(hierarchy.stopped, None);
        assert_eq!(hierarchy.listed_calls(), 6);
        assert_eq!(asked(&requests), ["leaf"]);
    }

    /// Deeper levels are followed from the functions whose contents may be shown, and from
    /// nowhere else: `hidden` (private) and `dep_caller` (outside the project) are listed but
    /// never asked about. `top` calls itself, and `other` is reached twice.
    #[gpui::test]
    async fn test_incoming_calls_three_levels_skip_what_is_withheld(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;
        let (server, _registration) = start_language_server(&project, true, cx).await;
        let requests = serve_call_graph(&server, None);

        let hierarchy = query(&project, "src/lib.rs", 1, CallDirection::Incoming, 3, 0, cx)
            .await
            .expect("the query succeeds")
            .expect("there is a function on line 1");
        assert_eq!(
            outline(&hierarchy.root),
            [
                "leaf src/lib.rs:1-1 [] \"fn leaf() {}\" Expanded",
                "  - excluded/gen.rs:1-3 [2] \"\" Withheld withheld",
                "  linked <outside>:1-3 [2] \"\" Withheld withheld",
                "  - secret.rs:1-4 [2] \"\" Withheld withheld",
                "  middle src/lib.rs:2-4 [3] \"leaf();\" Expanded",
                "    - secret.rs:1-4 [3] \"\" Withheld withheld",
                "    top src/lib.rs:5-8 [6] \"middle();\" Expanded",
                "      top src/lib.rs:5-8 [7] \"top();\" Recursive",
                "    other src/lib.rs:9-12 [11] \"middle();\" Repeated",
                "  other src/lib.rs:9-12 [10] \"leaf();\" Expanded",
                "  dep_caller <outside>:1-3 [2] \"\" Withheld withheld",
            ]
        );
        assert_eq!(asked(&requests), ["leaf", "middle", "other", "top"]);
        assert_eq!(hierarchy.stopped, None);
    }

    #[gpui::test]
    async fn test_outgoing_calls_preview_the_declaration(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;
        let (server, _registration) = start_language_server(&project, true, cx).await;
        let requests = serve_call_graph(&server, None);

        let hierarchy = query(&project, "src/lib.rs", 5, CallDirection::Outgoing, 2, 0, cx)
            .await
            .expect("the query succeeds")
            .expect("there is a function on line 5");
        assert_eq!(
            outline(&hierarchy.root),
            [
                "top src/lib.rs:5-8 [] \"fn top() {\" Expanded",
                "  middle src/lib.rs:2-4 [6] \"fn middle() {\" Expanded",
                "    leaf src/lib.rs:1-1 [3] \"fn leaf() {}\" DepthLimit",
                "  top src/lib.rs:5-8 [7] \"fn top() {\" Recursive",
            ]
        );
        assert_eq!(asked(&requests), ["top", "middle"]);
    }

    /// A callee's declaration is quoted from the callee's file, so a callee in a private file or
    /// behind a link out of the project is placed but not quoted, named (when private) or
    /// followed, while the lines of the calls, which lie in the caller's file, stay listed.
    #[gpui::test]
    async fn test_outgoing_calls_withhold_the_callees_that_must_not_be_shown(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let project = test_project(cx).await;
        let (server, _registration) = start_language_server(&project, true, cx).await;
        let requests = serve_call_graph(&server, None);

        let hierarchy = query(&project, "src/lib.rs", 9, CallDirection::Outgoing, 2, 0, cx)
            .await
            .expect("the query succeeds")
            .expect("there is a function on line 9");
        assert_eq!(
            outline(&hierarchy.root),
            [
                "other src/lib.rs:9-12 [] \"fn other() {\" Expanded",
                "  linked <outside>:1-3 [11] \"\" Withheld withheld",
                "  - secret.rs:1-4 [11] \"\" Withheld withheld",
                "  leaf src/lib.rs:1-1 [10] \"fn leaf() {}\" Expanded",
                "  middle src/lib.rs:2-4 [11] \"fn middle() {\" Expanded",
                "    leaf src/lib.rs:1-1 [3] \"fn leaf() {}\" DepthLimit",
            ]
        );
        assert_eq!(asked(&requests), ["other", "leaf", "middle"]);
    }

    /// The position resolves to a function in a private file: it is placed, but neither named,
    /// quoted nor followed.
    #[gpui::test]
    async fn test_a_withheld_root_is_not_followed(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;
        let (server, _registration) = start_language_server(&project, true, cx).await;
        let requests = serve_call_graph(&server, None);

        for direction in [CallDirection::Incoming, CallDirection::Outgoing] {
            let hierarchy = query(&project, "src/lib.rs", 10, direction, 3, 0, cx)
                .await
                .expect("the query succeeds")
                .expect("the server named a function");
            assert_eq!(
                outline(&hierarchy.root),
                ["- secret.rs:1-4 [] \"\" Withheld withheld"]
            );
        }
        assert!(asked(&requests).is_empty(), "{:?}", asked(&requests));
    }

    #[gpui::test]
    async fn test_results_beyond_the_limit_are_counted(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;
        let (server, _registration) = start_language_server(&project, true, cx).await;
        let requests = serve_call_graph(&server, None);

        let hierarchy = query(&project, "src/lib.rs", 1, CallDirection::Incoming, 2, 5, cx)
            .await
            .expect("the query succeeds")
            .expect("there is a function on line 1");
        assert_eq!(
            outline(&hierarchy.root),
            [
                "leaf src/lib.rs:1-1 [] \"fn leaf() {}\" Expanded",
                "  - excluded/gen.rs:1-3 [2] \"\" Withheld withheld",
                "  linked <outside>:1-3 [2] \"\" Withheld withheld",
                "  - secret.rs:1-4 [2] \"\" Withheld withheld",
                "  middle src/lib.rs:2-4 [3] \"leaf();\" Stopped",
                "  other src/lib.rs:9-12 [10] \"leaf();\" Stopped",
            ]
        );
        assert_eq!(hierarchy.omitted, 1);
        assert_eq!(hierarchy.stopped, Some(CallWalkStop::ResultLimit));
        assert_eq!(hierarchy.max_results, 5);
        assert_eq!(asked(&requests), ["leaf"]);
    }

    /// One caller with 90 callers of its own: following each of them on the third level would
    /// take 92 requests.
    #[gpui::test]
    async fn test_requests_are_capped(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;
        let (server, _registration) = start_language_server(&project, true, cx).await;
        let requests = serve_call_graph(&server, None);

        let hierarchy = query(
            &project,
            "src/wide.rs",
            1,
            CallDirection::Incoming,
            3,
            MAX_QUERY_RESULTS,
            cx,
        )
        .await
        .expect("the query succeeds")
        .expect("there is a function on line 1");
        assert_eq!(asked(&requests).len(), MAX_CALL_HIERARCHY_REQUESTS);
        assert_eq!(hierarchy.stopped, Some(CallWalkStop::RequestLimit));
        let callers = &hierarchy
            .root
            .children
            .first()
            .expect("wide_1 calls wide_0")
            .children;
        assert_eq!(callers.len(), WIDE_CALLERS as usize);
        let expanded = callers
            .iter()
            .filter(|caller| caller.expansion == CallExpansion::Expanded)
            .count();
        assert_eq!(expanded, MAX_CALL_HIERARCHY_REQUESTS - 2);
        assert!(
            callers
                .iter()
                .skip(expanded)
                .all(|caller| caller.expansion == CallExpansion::Stopped)
        );
    }

    #[gpui::test]
    async fn test_a_server_that_does_not_answer_stops_the_walk(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;
        let (server, _registration) = start_language_server(&project, true, cx).await;
        let requests = serve_call_graph(&server, Some("middle"));

        let mut task = query(&project, "src/lib.rs", 1, CallDirection::Incoming, 2, 0, cx);
        cx.executor().run_until_parked();
        assert!((&mut task).now_or_never().is_none(), "waits for `middle`");
        cx.executor()
            .advance_clock(LANGUAGE_SERVER_TIMEOUT + Duration::from_secs(1));
        cx.executor().run_until_parked();
        let hierarchy = task
            .now_or_never()
            .expect("the walk gives up once the timeout passes")
            .expect("what was found is kept")
            .expect("there is a function on line 1");
        assert_eq!(
            outline(&hierarchy.root),
            [
                "leaf src/lib.rs:1-1 [] \"fn leaf() {}\" Expanded",
                "  - excluded/gen.rs:1-3 [2] \"\" Withheld withheld",
                "  linked <outside>:1-3 [2] \"\" Withheld withheld",
                "  - secret.rs:1-4 [2] \"\" Withheld withheld",
                "  middle src/lib.rs:2-4 [3] \"leaf();\" Stopped",
                "  other src/lib.rs:9-12 [10] \"leaf();\" Stopped",
                "  dep_caller <outside>:1-3 [2] \"\" Withheld withheld",
            ]
        );
        assert_eq!(hierarchy.stopped, Some(CallWalkStop::TimeLimit));
        assert_eq!(asked(&requests), ["leaf", "middle"]);
    }

    #[gpui::test]
    async fn test_what_the_server_cannot_answer_is_reported_as_such(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;

        let error = query(&project, "src/lib.rs", 1, CallDirection::Incoming, 1, 0, cx)
            .await
            .expect_err("no server is running");
        assert!(
            error.downcast_ref::<LanguageServerNotRunning>().is_some(),
            "{error}"
        );

        let error = query(&project, "secret.rs", 1, CallDirection::Incoming, 1, 0, cx)
            .await
            .expect_err("a private file is refused");
        assert!(error.to_string().contains("`private_files`"), "{error}");

        let (_server, _registration) = start_language_server(&project, false, cx).await;
        let error = query(&project, "src/lib.rs", 1, CallDirection::Incoming, 1, 0, cx)
            .await
            .expect_err("the server has no call hierarchy");
        assert!(
            error.downcast_ref::<CallHierarchyNotSupported>().is_some(),
            "{error}"
        );
    }

    #[gpui::test]
    async fn test_no_function_at_the_position(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;
        let (server, _registration) = start_language_server(&project, true, cx).await;
        serve_call_graph(&server, None);

        let hierarchy = query(&project, "src/lib.rs", 4, CallDirection::Incoming, 1, 0, cx)
            .await
            .expect("the query succeeds");
        assert_eq!(hierarchy, None);
    }

    /// Every answer takes 14 seconds, under the timeout of one request; the fourth would end
    /// past the budget of the whole walk, so it is given up at the budget.
    #[gpui::test]
    async fn test_the_whole_walk_has_a_time_budget(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;
        let (server, _registration) = start_language_server(&project, true, cx).await;
        serve_call_graph(&server, None);
        let requests = Arc::new(Mutex::new(Vec::new()));
        server.set_request_handler::<lsp::request::CallHierarchyIncomingCalls, _, _>({
            let requests = requests.clone();
            move |params, cx| {
                let requests = requests.clone();
                async move {
                    let name = params.item.name;
                    requests.lock().expect("not poisoned").push(name.clone());
                    cx.background_executor()
                        .timer(Duration::from_secs(14))
                        .await;
                    Ok(Some(
                        calls_into(&name)
                            .into_iter()
                            .map(|(caller, rows)| lsp::CallHierarchyIncomingCall {
                                from: function(&caller),
                                from_ranges: call_ranges(&rows),
                            })
                            .collect(),
                    ))
                }
            }
        });

        let mut task = query(&project, "src/lib.rs", 1, CallDirection::Incoming, 3, 0, cx);
        let started = cx.executor().now();
        let mut hierarchy = None;
        for _ in 0..120 {
            cx.executor().advance_clock(Duration::from_secs(1));
            cx.executor().run_until_parked();
            if let Some(result) = (&mut task).now_or_never() {
                hierarchy = Some(result);
                break;
            }
        }
        let elapsed = cx.executor().now() - started;
        let hierarchy = hierarchy
            .expect("the walk ends")
            .expect("what was found is kept")
            .expect("there is a function on line 1");
        assert_eq!(hierarchy.stopped, Some(CallWalkStop::TimeLimit));
        assert_eq!(asked(&requests), ["leaf", "middle", "other", "top"]);
        assert!(
            elapsed <= CALL_HIERARCHY_TIME_BUDGET + Duration::from_secs(1),
            "{elapsed:?}"
        );
        let top = hierarchy
            .root
            .children
            .iter()
            .flat_map(|caller| &caller.children)
            .find(|caller| caller.name == "top")
            .expect("middle is called by top");
        assert_eq!(top.expansion, CallExpansion::Stopped);
    }

    /// A function reported more than once (two servers for one file, or a server repeating
    /// itself) is listed once, with the calls of every report.
    #[gpui::test]
    async fn test_a_function_reported_twice_is_listed_once(cx: &mut TestAppContext) {
        init_test(cx);
        let project = test_project(cx).await;
        let (server, _registration) = start_language_server(&project, true, cx).await;
        serve_call_graph(&server, None);
        server.set_request_handler::<lsp::request::CallHierarchyIncomingCalls, _, _>(
            |_, _| async move {
                Ok(Some(
                    [&[2][..], &[3], &[2]]
                        .into_iter()
                        .map(|rows| lsp::CallHierarchyIncomingCall {
                            from: function("middle"),
                            from_ranges: call_ranges(rows),
                        })
                        .collect(),
                ))
            },
        );

        let hierarchy = query(&project, "src/lib.rs", 1, CallDirection::Incoming, 1, 0, cx)
            .await
            .expect("the query succeeds")
            .expect("there is a function on line 1");
        assert_eq!(
            outline(&hierarchy.root),
            [
                "leaf src/lib.rs:1-1 [] \"fn leaf() {}\" Expanded",
                "  middle src/lib.rs:2-4 [3, 4] \"leaf();\" DepthLimit",
            ]
        );
    }

    /// The remote file count only stops a walk over a remote project, and only once it reaches
    /// the limit; the request and result limits come first.
    #[test]
    fn test_walk_stops_at_each_limit() {
        let files = MAX_REMOTE_CALL_HIERARCHY_FILES;
        assert_eq!(walk_stop(0, 0, 50, None), None);
        assert_eq!(walk_stop(0, 0, 50, Some(files - 1)), None);
        assert_eq!(
            walk_stop(0, 0, 50, Some(files)),
            Some(CallWalkStop::RemoteFileLimit)
        );
        assert_eq!(
            walk_stop(0, 0, 50, None),
            None,
            "a local project has no file limit"
        );
        assert_eq!(
            walk_stop(MAX_CALL_HIERARCHY_REQUESTS - 1, 49, 50, Some(files - 1)),
            None
        );
        assert_eq!(
            walk_stop(0, 50, 50, Some(files)),
            Some(CallWalkStop::ResultLimit)
        );
        assert_eq!(
            walk_stop(MAX_CALL_HIERARCHY_REQUESTS, 50, 50, Some(files)),
            Some(CallWalkStop::RequestLimit)
        );
    }

    #[test]
    fn test_names_are_one_bounded_line() {
        assert_eq!(single_line_name("  run  "), "run");
        assert_eq!(
            single_line_name("run\n  leaf src/lib.rs:1\r\tDepthLimit"),
            "run leaf src/lib.rs:1 DepthLimit"
        );
        let long = single_line_name(&"x".repeat(MAX_CALL_NAME_CHARS * 5));
        assert_eq!(
            long.chars().count(),
            MAX_CALL_NAME_CHARS + 1,
            "the name and `…`"
        );
        assert!(long.ends_with('…'));
    }
}
