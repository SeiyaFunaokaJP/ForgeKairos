//! Where the panel's calls come from.
//!
//! Every question the panel asks about calls goes through [`prepare_root`] and
//! [`start_fetch`]. Two sources answer: the language server's call hierarchy,
//! which is exact, and, in C and C++, the project's symbol index, which finds
//! calls by name (ADR-F68). [`combine`] merges their answers so that a function
//! both know is one row, and each row says which source it came from.

use std::{ops::Range, path::PathBuf, sync::Arc};

use anyhow::{Context as _, Result, anyhow};
use collections::HashSet;
use futures::future::join_all;
use gpui::{App, AsyncApp, Entity, SharedString, Task, WeakEntity};
use language::{
    Anchor, Bias, Buffer, BufferSnapshot, CharKind, File as _, Location, Point, PointUtf16,
    ToOffset as _, ToPoint as _, ToPointUtf16, Unclipped,
};
use lsp::LanguageServerId;
use project::{
    CallHierarchyItem, Project, ProjectPath, SymbolIndexStore,
    symbol_index_store::{
        CallKind, CalleeTarget, DefinitionKind, IndexedDefinition, MAX_CALLERS_LIMIT,
        SymbolIndexStatus,
    },
};
use serde::{Deserialize, Serialize};
use settings::{CallGraphSources, Settings as _};
use util::{ResultExt as _, paths::PathExt as _};

use crate::CallGraphPanelSettings;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallDirection {
    /// Who calls the function.
    #[default]
    Incoming,
    /// What the function calls.
    Outgoing,
}

impl CallDirection {
    pub fn opposite(self) -> Self {
        match self {
            CallDirection::Incoming => CallDirection::Outgoing,
            CallDirection::Outgoing => CallDirection::Incoming,
        }
    }
}

/// Which source produced a symbol. The panel draws each row's badge from it,
/// so a result that is only a name match never looks like an exact one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallOrigin {
    /// Answered by a language server's call hierarchy; exact.
    LanguageServer,
    /// Found by the symbol index by name.
    NameMatch,
    /// A function the symbol index saw stored into the member an indirect
    /// call goes through; it may or may not be the one called.
    FunctionPointerCandidate,
    /// A macro the symbol index found by name.
    Macro,
}

/// Which sources the panel asks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SourceFilter {
    #[default]
    All,
    LanguageServer,
    NameMatch,
}

impl SourceFilter {
    pub fn asks_language_server(self) -> bool {
        !matches!(self, SourceFilter::NameMatch)
    }

    pub fn asks_index(self) -> bool {
        !matches!(self, SourceFilter::LanguageServer)
    }
}

impl From<CallGraphSources> for SourceFilter {
    fn from(sources: CallGraphSources) -> Self {
        match sources {
            CallGraphSources::All => SourceFilter::All,
            CallGraphSources::LanguageServer => SourceFilter::LanguageServer,
            CallGraphSources::NameMatch => SourceFilter::NameMatch,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct FetchOptions {
    pub sources: SourceFilter,
    /// Callees with these names are set apart instead of listed.
    pub excluded_callees: Arc<[String]>,
}

impl FetchOptions {
    pub fn from_settings(cx: &App) -> Self {
        let settings = CallGraphPanelSettings::get_global(cx);
        Self {
            sources: settings.sources.into(),
            excluded_callees: settings.excluded_callees.clone(),
        }
    }
}

/// The points of a location. A language server counts columns in UTF-16
/// units; the symbol index counts them in UTF-8 bytes of the file on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocationPoints {
    Utf16(Range<PointUtf16>),
    Utf8(Range<Point>),
}

impl LocationPoints {
    pub fn start_row(&self) -> u32 {
        match self {
            LocationPoints::Utf16(points) => points.start.row,
            LocationPoints::Utf8(points) => points.start.row,
        }
    }

    fn start_key(&self) -> (u32, u32) {
        match self {
            LocationPoints::Utf16(points) => (points.start.row, points.start.column),
            LocationPoints::Utf8(points) => (points.start.row, points.start.column),
        }
    }

    fn anchors(&self, snapshot: &BufferSnapshot) -> Range<Anchor> {
        match self {
            LocationPoints::Utf16(points) => anchors_for_points(snapshot, points),
            LocationPoints::Utf8(points) => {
                let start = snapshot.clip_point(points.start, Bias::Left);
                let end = snapshot.clip_point(points.end, Bias::Right);
                snapshot.anchor_after(start)..snapshot.anchor_before(end)
            }
        }
    }
}

/// A place in a file that survives its buffer being released.
///
/// The panel holds only a weak handle to the buffer, so that a deep tree does
/// not keep every file it ever touched open. When the buffer is gone, the
/// location is reopened from its path and its points. A place the symbol index
/// found never had a buffer: its file is opened only to jump to it.
#[derive(Clone, Debug)]
pub struct SymbolLocation {
    pub buffer: WeakEntity<Buffer>,
    /// The place in `buffer`, when the location was made from an open buffer.
    pub range: Option<Range<Anchor>>,
    pub points: LocationPoints,
    pub abs_path: Option<PathBuf>,
    pub project_path: Option<ProjectPath>,
}

impl SymbolLocation {
    fn new(buffer: &Entity<Buffer>, range: Range<Anchor>, cx: &App) -> Self {
        let buffer_ref = buffer.read(cx);
        let snapshot = buffer_ref.snapshot();
        let point_range =
            range.start.to_point_utf16(&snapshot)..range.end.to_point_utf16(&snapshot);
        let file = project::File::from_dyn(buffer_ref.file());
        Self {
            buffer: buffer.downgrade(),
            range: Some(range),
            points: LocationPoints::Utf16(point_range),
            abs_path: file.map(|file| file.worktree.read(cx).absolutize(&file.path)),
            project_path: file.map(|file| ProjectPath {
                worktree_id: file.worktree_id(cx),
                path: file.path.clone(),
            }),
        }
    }

    fn from_index(path: &ProjectPath, range: Range<Point>, project: &Project, cx: &App) -> Self {
        let abs_path = project
            .worktree_for_id(path.worktree_id, cx)
            .map(|worktree| worktree.read(cx).absolutize(&path.path));
        Self {
            buffer: WeakEntity::new_invalid(),
            range: None,
            points: LocationPoints::Utf8(range),
            abs_path,
            project_path: Some(path.clone()),
        }
    }

    pub fn same_file(&self, other: &SymbolLocation) -> bool {
        match (&self.project_path, &other.project_path) {
            (Some(left), Some(right)) if left.worktree_id == right.worktree_id => left == right,
            _ => match (&self.abs_path, &other.abs_path) {
                (Some(left), Some(right)) => left == right,
                _ => self.buffer == other.buffer,
            },
        }
    }

    /// Whether two locations name the same place, whether or not their buffers
    /// are still open.
    pub fn same_place(&self, other: &SymbolLocation) -> bool {
        self.same_file(other) && self.points == other.points
    }

    fn same_line(&self, other: &SymbolLocation) -> bool {
        self.same_file(other) && self.points.start_row() == other.points.start_row()
    }
}

/// What a row stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymbolRole {
    Function,
    /// Several definitions share the callee's name; expanding lists them.
    SameNameDefinitions(usize),
    /// A call through a struct member; expanding lists the functions stored
    /// into that member.
    IndirectCall,
    /// A callee the symbol index has no definition of.
    Unresolved,
    /// Calls outside any function, such as in a file-scope initializer.
    FileScope,
}

/// A function in the call graph and, when it is a child, how its parent reaches it.
#[derive(Clone, Debug)]
pub struct CallSymbol {
    pub name: SharedString,
    /// The signature after the name, such as `(int count) -> void`.
    pub detail: Option<SharedString>,
    pub kind: lsp::SymbolKind,
    pub origin: CallOrigin,
    /// The symbol's name at its definition.
    pub location: SymbolLocation,
    /// The calls between this symbol and its parent, earliest first: in the
    /// caller for callers, in the parent for callees. Empty for a root.
    pub call_sites: Vec<SymbolLocation>,
    /// The file the row shows, relative to its worktree when it is in one.
    pub display_path: Option<SharedString>,
    /// Zero-based row of the first call site, or of the definition when there is none.
    pub display_row: u32,
    /// Whether the symbol is in a C or C++ file, the languages the symbol
    /// index covers.
    c_family: bool,
    source: SourceHandle,
}

impl CallSymbol {
    /// How many times the parent and this symbol call each other.
    pub fn site_count(&self) -> usize {
        self.call_sites.len().max(1)
    }

    /// Where opening the row goes: the `site_index`-th call site, or the
    /// definition when the row has none (a root, or a server that gave no ranges).
    pub fn jump_target(&self, site_index: usize) -> &SymbolLocation {
        self.call_sites
            .get(site_index % self.call_sites.len().max(1))
            .unwrap_or(&self.location)
    }

    pub fn file_name(&self) -> Option<SharedString> {
        let path = self.location.abs_path.as_ref()?;
        Some(path.file_name()?.to_string_lossy().into_owned().into())
    }

    pub fn role(&self) -> SymbolRole {
        match &self.source {
            SourceHandle::Function { .. } => SymbolRole::Function,
            SourceHandle::Candidates(candidates) => {
                SymbolRole::SameNameDefinitions(candidates.len())
            }
            SourceHandle::Indirect => SymbolRole::IndirectCall,
            SourceHandle::Unresolved => SymbolRole::Unresolved,
            SourceHandle::FileScope => SymbolRole::FileScope,
        }
    }

    pub fn can_expand(&self) -> bool {
        !matches!(
            self.source,
            SourceHandle::Unresolved | SourceHandle::FileScope
        )
    }

    pub fn is_c_family(&self) -> bool {
        self.c_family
    }

    /// Whether both are the same function: the same name in the same file,
    /// defined on the same row or one inside the other's definition. Rows are
    /// compared rather than columns, because a language server and the symbol
    /// index count columns differently. The name is compared too because one
    /// macro invocation can define several functions on one row.
    pub fn same_symbol(&self, other: &CallSymbol) -> bool {
        if !matches!(self.source, SourceHandle::Function { .. })
            || !matches!(other.source, SourceHandle::Function { .. })
        {
            return false;
        }
        if index_name(&self.name) != index_name(&other.name)
            || !self.location.same_file(&other.location)
        {
            return false;
        }
        let self_row = self.location.points.start_row();
        let other_row = other.location.points.start_row();
        self_row == other_row || self.spans_row(other_row) || other.spans_row(self_row)
    }

    /// Whether the rows stand for the same thing, so that a new answer can
    /// update a row in place instead of replacing it.
    pub fn same_row(&self, other: &CallSymbol) -> bool {
        match (&self.source, &other.source) {
            (SourceHandle::Function { .. }, SourceHandle::Function { .. }) => {
                self.same_symbol(other)
            }
            (SourceHandle::Candidates(_), SourceHandle::Candidates(_))
            | (SourceHandle::Indirect, SourceHandle::Indirect)
            | (SourceHandle::Unresolved, SourceHandle::Unresolved) => self.name == other.name,
            (SourceHandle::FileScope, SourceHandle::FileScope) => true,
            _ => false,
        }
    }

    fn spans_row(&self, row: u32) -> bool {
        let SourceHandle::Function {
            language_server,
            index,
        } = &self.source
        else {
            return false;
        };
        let in_server_range = language_server.as_ref().is_some_and(|handle| {
            (handle.full_point_range.start.row..=handle.full_point_range.end.row).contains(&row)
        });
        let in_index_range = index.as_ref().is_some_and(|definition| {
            (definition.item_range.start.row..=definition.item_range.end.row).contains(&row)
        });
        in_server_range || in_index_range
    }

    /// Whether `position` in `buffer` lies within the symbol's whole body, as
    /// far as the sources know it.
    pub fn encloses(&self, buffer: &Entity<Buffer>, position: Anchor, cx: &App) -> bool {
        let SourceHandle::Function {
            language_server,
            index,
        } = &self.source
        else {
            return false;
        };
        let buffer_ref = buffer.read(cx);
        let snapshot = buffer_ref.snapshot();
        if let Some(handle) = language_server
            && self.location.buffer == buffer.downgrade()
        {
            return handle.full_range.start.cmp(&position, &snapshot).is_le()
                && handle.full_range.end.cmp(&position, &snapshot).is_ge();
        }
        if let Some(definition) = index
            && let Some(file) = buffer_ref.file()
            && file.worktree_id(cx) == definition.path.worktree_id
            && file.path() == &definition.path.path
        {
            let point = position.to_point(&snapshot);
            return definition.item_range.start <= point && point <= definition.item_range.end;
        }
        false
    }

    /// Takes in what the symbol index knows of the same function: its
    /// definition and the call sites the language server did not report.
    fn absorb(&mut self, other: CallSymbol) {
        if let (
            SourceHandle::Function { index, .. },
            SourceHandle::Function {
                index: other_index, ..
            },
        ) = (&mut self.source, other.source)
            && index.is_none()
        {
            *index = other_index;
        }
        self.absorb_sites(other.call_sites);
    }

    fn absorb_sites(&mut self, sites: Vec<SymbolLocation>) {
        for site in sites {
            if !self.call_sites.iter().any(|known| known.same_line(&site)) {
                self.call_sites.push(site);
            }
        }
        self.call_sites.sort_by_key(|site| site.points.start_key());
        if let Some(first) = self.call_sites.first() {
            self.display_row = first.points.start_row();
        }
    }
}

/// What a source needs to ask about a symbol again.
#[derive(Clone, Debug)]
enum SourceHandle {
    /// A function the language server, the symbol index, or both know.
    Function {
        language_server: Option<ServerHandle>,
        index: Option<IndexedDefinition>,
    },
    Candidates(Vec<CallSymbol>),
    /// The member's name is the symbol's name.
    Indirect,
    Unresolved,
    FileScope,
}

#[derive(Clone, Debug)]
struct ServerHandle {
    server_id: LanguageServerId,
    raw_detail: Option<String>,
    full_range: Range<Anchor>,
    full_point_range: Range<PointUtf16>,
    data: Option<serde_json::Value>,
}

/// The outcome of looking for a root under the cursor.
#[derive(Debug)]
pub enum RootOutcome {
    Found(CallSymbol),
    /// The symbol index found several functions by the name at the cursor.
    Candidates(Vec<CallSymbol>),
    /// A language server answered, but there is no function at the position.
    NoSymbol,
    /// No language server runs for the file.
    NoLanguageServer,
    /// The file's language servers do not offer call hierarchy.
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ServerSupport {
    None,
    Unsupported,
    Supported,
}

fn call_hierarchy_support(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    cx: &App,
) -> ServerSupport {
    let lsp_store = project.read(cx).lsp_store().read(cx);
    let server_ids = lsp_store.relevant_server_ids_for_capability_check(buffer, cx);
    if server_ids.is_empty() {
        return ServerSupport::None;
    }
    // A server whose capabilities have not arrived yet is given the benefit of
    // the doubt: the request itself will tell.
    let supported = server_ids.iter().any(|server_id| {
        lsp_store
            .lsp_server_capabilities
            .get(server_id)
            .is_none_or(|capabilities| {
                capabilities
                    .call_hierarchy_provider
                    .as_ref()
                    .is_some_and(|provider| match provider {
                        lsp::CallHierarchyServerCapability::Simple(supported) => *supported,
                        lsp::CallHierarchyServerCapability::Options(_) => true,
                    })
            })
    });
    if supported {
        ServerSupport::Supported
    } else {
        ServerSupport::Unsupported
    }
}

pub fn is_c_family(buffer: &Buffer) -> bool {
    buffer
        .language()
        .is_some_and(|language| matches!(language.name().as_ref(), "C" | "C++"))
}

/// The last part of a qualified name (`Widget::draw` becomes `draw`), which
/// is what the symbol index records.
fn index_name(name: &str) -> &str {
    name.rsplit_once("::").map_or(name, |(_, last)| last)
}

fn is_header_path(path: &str) -> bool {
    let Some((_, extension)) = path.rsplit_once('.') else {
        return false;
    };
    matches!(
        extension,
        "h" | "hh" | "hpp" | "hxx" | "h++" | "H" | "inl" | "ipp" | "tcc"
    )
}

fn is_function_like(kind: DefinitionKind) -> bool {
    matches!(
        kind,
        DefinitionKind::Function
            | DefinitionKind::Method
            | DefinitionKind::Macro
            | DefinitionKind::FunctionMacro
    )
}

/// The functions and macros among `definitions`, leaving out declarations
/// when there is a definition, as a language server would go to the body.
fn function_definitions(definitions: Vec<IndexedDefinition>) -> Vec<IndexedDefinition> {
    let mut functions = definitions
        .into_iter()
        .filter(|definition| is_function_like(definition.kind))
        .collect::<Vec<_>>();
    if functions
        .iter()
        .any(|definition| !definition.is_declaration)
    {
        functions.retain(|definition| !definition.is_declaration);
    }
    functions
}

struct IndexRootQuery {
    name: Option<String>,
    path: ProjectPath,
    point: Point,
}

fn index_root_query(buffer: &Entity<Buffer>, position: Anchor, cx: &App) -> Option<IndexRootQuery> {
    let buffer_ref = buffer.read(cx);
    if !is_c_family(buffer_ref) {
        return None;
    }
    let file = buffer_ref.file()?;
    let snapshot = buffer_ref.snapshot();
    let offset = position.to_offset(&snapshot);
    let (range, kind) = snapshot.surrounding_word(offset, None);
    let name = (kind == Some(CharKind::Word) && !range.is_empty())
        .then(|| snapshot.text_for_range(range).collect::<String>())
        .filter(|name| !name.starts_with(|character: char| character.is_ascii_digit()));
    Some(IndexRootQuery {
        name,
        path: ProjectPath {
            worktree_id: file.worktree_id(cx),
            path: file.path().clone(),
        },
        point: position.to_point(&snapshot),
    })
}

/// Finds the function at `position` to use as a root. The language server is
/// asked first; in C and C++ the symbol index fills in when it has no answer.
pub fn prepare_root(
    project: &Entity<Project>,
    buffer: Entity<Buffer>,
    position: Anchor,
    sources: SourceFilter,
    cx: &mut App,
) -> Task<Result<RootOutcome>> {
    let support = call_hierarchy_support(project, &buffer, cx);
    let prepare =
        (sources.asks_language_server() && support == ServerSupport::Supported).then(|| {
            project.update(cx, |project, cx| {
                project.prepare_call_hierarchy(&buffer, position, cx)
            })
        });
    let index_query = sources
        .asks_index()
        .then(|| index_root_query(&buffer, position, cx))
        .flatten();
    let project = project.clone();
    cx.spawn(async move |cx| {
        let from_language_server = match prepare {
            Some(prepare) => match prepare.await {
                Ok(items) => match items.unwrap_or_default().into_iter().next() {
                    Some(item) => {
                        return Ok(cx.update(|cx| {
                            RootOutcome::Found(symbol_from_item(&item, Vec::new(), cx))
                        }));
                    }
                    None => Ok(RootOutcome::NoSymbol),
                },
                Err(error) => Err(error),
            },
            None if !sources.asks_language_server() => Ok(RootOutcome::NoSymbol),
            None => Ok(match support {
                ServerSupport::None => RootOutcome::NoLanguageServer,
                ServerSupport::Unsupported => RootOutcome::Unsupported,
                ServerSupport::Supported => RootOutcome::NoSymbol,
            }),
        };
        if let Some(query) = index_query
            && let Some(outcome) = index_root(&project, query, cx).await
        {
            if let Err(error) = &from_language_server {
                log::warn!("call graph: the language server found no root: {error:#}");
            }
            return Ok(outcome);
        }
        from_language_server
    })
}

async fn index_root(
    project: &Entity<Project>,
    query: IndexRootQuery,
    cx: &mut AsyncApp,
) -> Option<RootOutcome> {
    let store = symbol_index_store(project, cx);
    if let Some(name) = &query.name {
        let definitions = store
            .update(cx, |store, cx| {
                store.definitions(name, Some(query.path.clone()), cx)
            })
            .await
            .log_err()
            .unwrap_or_default();
        let mut functions = function_definitions(definitions);
        // On a definition's name the cursor means that definition; at a call, a definition in
        // the same file (its own `static` one, say) is the one called.
        let chosen = functions
            .iter()
            .position(|definition| {
                definition.path == query.path && definition.name_range.start.row == query.point.row
            })
            .or_else(|| {
                let mut in_file = functions
                    .iter()
                    .enumerate()
                    .filter(|(_, definition)| definition.path == query.path);
                match (in_file.next(), in_file.next()) {
                    (Some((index, _)), None) => Some(index),
                    _ => None,
                }
            });
        if let Some(chosen) = chosen {
            let definition = functions.swap_remove(chosen);
            return Some(project.read_with(cx, |project, cx| {
                RootOutcome::Found(symbol_from_definition(
                    &definition,
                    &[],
                    CallOrigin::NameMatch,
                    project,
                    cx,
                ))
            }));
        }
        if functions.len() > 1 {
            return Some(project.read_with(cx, |project, cx| {
                RootOutcome::Candidates(
                    functions
                        .iter()
                        .map(|definition| {
                            symbol_from_definition(
                                definition,
                                &[],
                                CallOrigin::NameMatch,
                                project,
                                cx,
                            )
                        })
                        .collect(),
                )
            }));
        }
        if let Some(definition) = functions.pop() {
            return Some(project.read_with(cx, |project, cx| {
                RootOutcome::Found(symbol_from_definition(
                    &definition,
                    &[],
                    CallOrigin::NameMatch,
                    project,
                    cx,
                ))
            }));
        }
    }
    let callees = store
        .update(cx, |store, cx| store.callees(query.path, query.point, cx))
        .await
        .log_err()
        .flatten()?;
    Some(project.read_with(cx, |project, cx| {
        RootOutcome::Found(symbol_from_definition(
            &callees.function,
            &[],
            CallOrigin::NameMatch,
            project,
            cx,
        ))
    }))
}

fn symbol_index_store(project: &Entity<Project>, cx: &mut AsyncApp) -> Entity<SymbolIndexStore> {
    project.read_with(cx, |project, _| project.symbol_index_store().clone())
}

/// A language server's answer about one symbol.
pub struct LanguageServerAnswer {
    pub symbols: Vec<CallSymbol>,
    /// Files opened again to ask, which the caller may keep open for a while
    /// so that expanding nearby rows does not reopen them each time.
    pub opened_buffers: Vec<Entity<Buffer>>,
}

/// The symbol index's answer about one symbol.
#[derive(Clone, Debug, Default)]
pub struct IndexAnswer {
    pub symbols: Vec<CallSymbol>,
    /// More calls exist than the index answers with.
    pub truncated: bool,
    /// The index was still being built, so the answer may be incomplete.
    pub partial: bool,
}

/// The two questions about one symbol's children, asked at the same time.
/// Either is `None` when that source is not asked. A language server's
/// answer of `None` means that no server could be asked after all.
pub struct ChildrenFetch {
    pub language_server: Option<Task<Result<Option<LanguageServerAnswer>>>>,
    pub index: Option<Task<Result<IndexAnswer>>>,
}

/// A symbol's children, from all the sources asked.
#[derive(Default)]
pub struct CallChildren {
    pub symbols: Vec<CallSymbol>,
    /// Callees that `excluded_callees` sets apart.
    pub excluded: Vec<CallSymbol>,
    pub truncated: bool,
    pub index_partial: bool,
    /// The language server was asked and found nothing, which it may do while
    /// it indexes.
    pub language_server_empty: bool,
    pub opened_buffers: Vec<Entity<Buffer>>,
}

/// Starts asking for the callers or callees of `symbol`, one level deep.
pub fn start_fetch(
    project: &Entity<Project>,
    symbol: &CallSymbol,
    direction: CallDirection,
    options: &FetchOptions,
    cx: &mut App,
) -> ChildrenFetch {
    match &symbol.source {
        SourceHandle::Candidates(candidates) => ChildrenFetch {
            language_server: None,
            index: Some(Task::ready(Ok(IndexAnswer {
                symbols: candidates.clone(),
                ..IndexAnswer::default()
            }))),
        },
        SourceHandle::Indirect => ChildrenFetch {
            language_server: None,
            index: options.sources.asks_index().then(|| {
                let project = project.clone();
                let field = symbol.name.to_string();
                cx.spawn(async move |cx| index_pointer_candidates(project, field, cx).await)
            }),
        },
        SourceHandle::Unresolved | SourceHandle::FileScope => ChildrenFetch {
            language_server: None,
            index: None,
        },
        SourceHandle::Function {
            language_server, ..
        } => {
            let ask_server = options.sources.asks_language_server()
                && (language_server.is_some() || symbol.c_family);
            let language_server = ask_server.then(|| {
                let project = project.clone();
                let symbol = symbol.clone();
                cx.spawn(async move |cx| fetch_lsp_children(project, symbol, direction, cx).await)
            });
            let index = (options.sources.asks_index() && symbol.c_family).then(|| {
                let project = project.clone();
                let symbol = symbol.clone();
                cx.spawn(async move |cx| index_children(project, symbol, direction, cx).await)
            });
            ChildrenFetch {
                language_server,
                index,
            }
        }
    }
}

/// Asks every source for the callers or callees of `symbol` and merges the answers.
pub fn fetch_children(
    project: &Entity<Project>,
    symbol: &CallSymbol,
    direction: CallDirection,
    options: FetchOptions,
    cx: &mut App,
) -> Task<Result<CallChildren>> {
    let ChildrenFetch {
        language_server,
        index,
    } = start_fetch(project, symbol, direction, &options, cx);
    cx.spawn(async move |_| {
        let language_server = match language_server {
            Some(task) => Some(task.await),
            None => None,
        };
        let index = match index {
            Some(task) => Some(task.await),
            None => None,
        };
        combine(direction, language_server, index, &options)
    })
}

/// Merges the sources' answers: a function both found is one row with the
/// language server's badge and the call sites of both; what only the index
/// found follows as name matches. A source that failed is left out while the
/// other has an answer.
pub fn combine(
    direction: CallDirection,
    language_server: Option<Result<Option<LanguageServerAnswer>>>,
    index: Option<Result<IndexAnswer>>,
    options: &FetchOptions,
) -> Result<CallChildren> {
    let mut children = CallChildren::default();
    let mut server_error = None;
    let server_symbols = match language_server {
        Some(Ok(Some(answer))) => {
            children.opened_buffers = answer.opened_buffers;
            children.language_server_empty = answer.symbols.is_empty();
            Some(answer.symbols)
        }
        Some(Ok(None)) | None => None,
        Some(Err(error)) => {
            server_error = Some(error);
            None
        }
    };
    let mut index_error = None;
    let index_symbols = match index {
        Some(Ok(answer)) => {
            children.truncated = answer.truncated;
            children.index_partial = answer.partial;
            Some(answer.symbols)
        }
        Some(Err(error)) => {
            index_error = Some(error);
            None
        }
        None => None,
    };
    if let Some(error) = server_error {
        if index_symbols.as_ref().is_none_or(Vec::is_empty) {
            if let Some(index_error) = index_error {
                log::warn!("call graph: the symbol index failed too: {index_error:#}");
            }
            return Err(error);
        }
        log::warn!("call graph: the language server failed, showing name matches: {error:#}");
    }
    if let Some(error) = index_error {
        if server_symbols.is_none() {
            return Err(error);
        }
        log::warn!("call graph: the symbol index failed: {error:#}");
    }

    let symbols = merge(
        server_symbols.unwrap_or_default(),
        index_symbols.unwrap_or_default(),
    );
    if direction == CallDirection::Outgoing && !options.excluded_callees.is_empty() {
        let (excluded, kept) = symbols.into_iter().partition(|symbol: &CallSymbol| {
            symbol.role() != SymbolRole::IndirectCall
                && options
                    .excluded_callees
                    .iter()
                    .any(|excluded| excluded.as_str() == index_name(&symbol.name))
        });
        children.symbols = kept;
        children.excluded = excluded;
    } else {
        children.symbols = symbols;
    }
    Ok(children)
}

fn merge(mut merged: Vec<CallSymbol>, from_index: Vec<CallSymbol>) -> Vec<CallSymbol> {
    let from_server = merged.len();
    let mut index_only = Vec::new();
    for symbol in from_index {
        let known = match &symbol.source {
            SourceHandle::Function { .. } => merged
                .iter()
                .take(from_server)
                .position(|known| known.same_symbol(&symbol)),
            SourceHandle::Candidates(candidates) => {
                merged.iter().take(from_server).position(|known| {
                    candidates
                        .iter()
                        .any(|candidate| known.same_symbol(candidate))
                })
            }
            SourceHandle::Unresolved => merged
                .iter()
                .take(from_server)
                .position(|known| index_name(&known.name) == index_name(&symbol.name)),
            SourceHandle::Indirect | SourceHandle::FileScope => None,
        };
        match known.and_then(|position| merged.get_mut(position)) {
            Some(known) => {
                if matches!(symbol.source, SourceHandle::Function { .. }) {
                    known.absorb(symbol);
                } else {
                    known.absorb_sites(symbol.call_sites);
                }
            }
            None => index_only.push(symbol),
        }
    }
    index_only.sort_by(|left, right| {
        (&left.display_path, left.display_row).cmp(&(&right.display_path, right.display_row))
    });
    merged.extend(index_only);
    merged
}

async fn fetch_lsp_children(
    project: Entity<Project>,
    symbol: CallSymbol,
    direction: CallDirection,
    cx: &mut AsyncApp,
) -> Result<Option<LanguageServerAnswer>> {
    let SourceHandle::Function {
        language_server, ..
    } = &symbol.source
    else {
        return Ok(None);
    };
    let Some(handle) = language_server else {
        return fetch_lsp_children_of_name_match(project, symbol, direction, cx).await;
    };
    let (buffer, reopened) =
        resolve_buffer(&project, &symbol.location, Some(handle.server_id), cx).await?;
    let item = cx.update(|cx| {
        let snapshot = buffer.read(cx).snapshot();
        let (range, selection_range) = match (&symbol.location.range, reopened) {
            (Some(selection_range), false) => (handle.full_range.clone(), selection_range.clone()),
            _ => (
                anchors_for_points(&snapshot, &handle.full_point_range),
                symbol.location.points.anchors(&snapshot),
            ),
        };
        CallHierarchyItem {
            buffer: buffer.clone(),
            server_id: handle.server_id,
            name: symbol.name.to_string(),
            kind: symbol.kind,
            detail: handle.raw_detail.clone(),
            range,
            selection_range,
            data: handle.data.clone(),
        }
    });
    let symbols = ask_language_server(&project, item, direction, cx).await?;
    Ok(Some(LanguageServerAnswer {
        symbols,
        opened_buffers: buffers_to_keep(reopened, buffer, cx),
    }))
}

/// The reopened buffer, when it may be kept open for a while. A file outside
/// the project's folders is released at once: its buffer holds the invisible
/// worktree it was opened in, on the client and, over SSH, on the host, and a
/// remote buffer that outlives its answer keeps that worktree for a while
/// even after it is released.
fn buffers_to_keep(
    reopened: bool,
    buffer: Entity<Buffer>,
    cx: &mut AsyncApp,
) -> Vec<Entity<Buffer>> {
    let in_project_folder = cx.update(|cx| {
        project::File::from_dyn(buffer.read(cx).file())
            .is_some_and(|file| file.worktree.read(cx).is_visible())
    });
    if reopened && in_project_folder {
        return vec![buffer];
    }
    drop(buffer);
    // Ending an update releases the dropped buffer now, before the work its
    // opening queued gets to run and take hold of its file.
    cx.update(|_| {});
    Vec::new()
}

/// Asks the language server about a function only the symbol index found,
/// by preparing the call hierarchy at its name first. This needs its file.
async fn fetch_lsp_children_of_name_match(
    project: Entity<Project>,
    symbol: CallSymbol,
    direction: CallDirection,
    cx: &mut AsyncApp,
) -> Result<Option<LanguageServerAnswer>> {
    let (buffer, reopened) = resolve_buffer(&project, &symbol.location, None, cx).await?;
    let prepare = cx.update(|cx| {
        if call_hierarchy_support(&project, &buffer, cx) != ServerSupport::Supported {
            return None;
        }
        let position = match (&symbol.location.range, reopened) {
            (Some(range), false) => range.start,
            _ => {
                symbol
                    .location
                    .points
                    .anchors(&buffer.read(cx).snapshot())
                    .start
            }
        };
        Some(project.update(cx, |project, cx| {
            project.prepare_call_hierarchy(&buffer, position, cx)
        }))
    });
    let Some(prepare) = prepare else {
        return Ok(None);
    };
    let item = prepare
        .await
        .context("asking the language server for the function")?
        .unwrap_or_default()
        .into_iter()
        .find(|item| index_name(&item.name) == index_name(&symbol.name));
    let symbols = match item {
        Some(item) => ask_language_server(&project, item, direction, cx).await?,
        None => Vec::new(),
    };
    Ok(Some(LanguageServerAnswer {
        symbols,
        opened_buffers: buffers_to_keep(reopened, buffer, cx),
    }))
}

async fn ask_language_server(
    project: &Entity<Project>,
    item: CallHierarchyItem,
    direction: CallDirection,
    cx: &mut AsyncApp,
) -> Result<Vec<CallSymbol>> {
    let raw_calls = match direction {
        CallDirection::Incoming => project
            .update(cx, |project, cx| project.incoming_calls(item, cx))
            .await
            .context("asking the language server for incoming calls")?
            .unwrap_or_default()
            .into_iter()
            .map(|call| (call.from, call.from_ranges))
            .collect::<Vec<_>>(),
        CallDirection::Outgoing => project
            .update(cx, |project, cx| project.outgoing_calls(item, cx))
            .await
            .context("asking the language server for outgoing calls")?
            .unwrap_or_default()
            .into_iter()
            .map(|call| (call.to, call.from_ranges))
            .collect::<Vec<_>>(),
    };
    Ok(cx.update(|cx| {
        let mut children = raw_calls
            .into_iter()
            .map(|(item, mut sites)| {
                sites.sort_by_key(|site| {
                    site.range
                        .start
                        .to_point_utf16(&site.buffer.read(cx).snapshot())
                });
                symbol_from_item(&item, sites, cx)
            })
            .collect::<Vec<_>>();
        children.sort_by(|left, right| {
            (&left.display_path, left.display_row).cmp(&(&right.display_path, right.display_row))
        });
        children
    }))
}

async fn index_children(
    project: Entity<Project>,
    symbol: CallSymbol,
    direction: CallDirection,
    cx: &mut AsyncApp,
) -> Result<IndexAnswer> {
    let store = symbol_index_store(&project, cx);
    let partial = store.read_with(cx, |store, cx| {
        store.statuses(cx).iter().any(|(_, status)| {
            matches!(
                status,
                SymbolIndexStatus::Indexing { .. } | SymbolIndexStatus::Scanning
            )
        })
    });
    let definition = index_definition(&store, &symbol, cx).await;
    let (symbols, truncated) = match direction {
        CallDirection::Incoming => {
            index_callers(&project, &store, &symbol, definition.as_ref(), cx).await?
        }
        CallDirection::Outgoing => match definition {
            Some(definition) => (
                index_callees(&project, &store, definition, cx).await?,
                false,
            ),
            None => (Vec::new(), false),
        },
    };
    Ok(IndexAnswer {
        symbols,
        truncated,
        partial,
    })
}

/// The symbol index's definition of `symbol`: the one it came with, or the
/// one of its name at the language server's place for it, or, for a function
/// pointer candidate, the best one of its name.
async fn index_definition(
    store: &Entity<SymbolIndexStore>,
    symbol: &CallSymbol,
    cx: &mut AsyncApp,
) -> Option<IndexedDefinition> {
    let SourceHandle::Function {
        language_server,
        index,
    } = &symbol.source
    else {
        return None;
    };
    if let Some(definition) = index {
        return Some(definition.clone());
    }
    let near = symbol.location.project_path.clone();
    let definitions = store
        .update(cx, |store, cx| {
            store.definitions(index_name(&symbol.name), near.clone(), cx)
        })
        .await
        .log_err()?;
    if language_server.is_some() {
        let row = symbol.location.points.start_row();
        definitions.into_iter().find(|definition| {
            Some(&definition.path) == near.as_ref()
                && (definition.name_range.start.row == row
                    || (definition.item_range.start.row..=definition.item_range.end.row)
                        .contains(&row))
        })
    } else {
        function_definitions(definitions).into_iter().next()
    }
}

type IndexSite = (ProjectPath, Range<Point>);

async fn index_callers(
    project: &Entity<Project>,
    store: &Entity<SymbolIndexStore>,
    symbol: &CallSymbol,
    definition: Option<&IndexedDefinition>,
    cx: &mut AsyncApp,
) -> Result<(Vec<CallSymbol>, bool)> {
    let name = index_name(&symbol.name).to_string();
    let callers = store
        .update(cx, |store, cx| store.callers(&name, MAX_CALLERS_LIMIT, cx))
        .await
        .context("asking the symbol index for callers")?;
    // In C a member call such as `ops->open()` never calls a function named
    // `open`; in C++ it may call a method of that name.
    let member_calls =
        definition.is_some_and(|definition| definition.kind == DefinitionKind::Method);
    // Callers can only be looked up by name, so a static function would also
    // get the calls of every other static function of its name. Only its own
    // file can call it (unless it is in a header, which each includer copies).
    let own_file = definition
        .filter(|definition| {
            definition.is_static && !is_header_path(definition.path.path.as_unix_str())
        })
        .map(|definition| definition.path.clone());
    // For the same reason, a function that is not static would get the calls
    // that other source files make to their own static function of its name.
    let root_file = definition
        .map(|definition| definition.path.clone())
        .or_else(|| symbol.location.project_path.clone());
    let shadowing_files = if own_file.is_none()
        && callers
            .calls
            .iter()
            .any(|call| Some(&call.path) != root_file.as_ref())
    {
        files_with_own_static(store, &name, root_file.as_ref(), cx).await
    } else {
        HashSet::default()
    };

    let mut groups: Vec<(Option<IndexedDefinition>, Vec<IndexSite>)> = Vec::new();
    for call in callers.calls {
        if call.callee != name
            || (call.kind == CallKind::Member && !member_calls)
            || own_file
                .as_ref()
                .is_some_and(|own_file| &call.path != own_file)
            || shadowing_files.contains(&call.path)
        {
            continue;
        }
        let site = (call.path, call.range);
        let group = groups
            .iter_mut()
            .find(|(caller, _)| match (caller, &call.caller) {
                (Some(left), Some(right)) => {
                    left.path == right.path
                        && left.name == right.name
                        && left.name_range == right.name_range
                }
                (None, None) => true,
                _ => false,
            });
        match group {
            Some((_, sites)) => sites.push(site),
            None => groups.push((call.caller, vec![site])),
        }
    }

    let symbols = project.read_with(cx, |project, cx| {
        groups
            .iter()
            .map(|(caller, sites)| match caller {
                Some(caller) => {
                    symbol_from_definition(caller, sites, CallOrigin::NameMatch, project, cx)
                }
                None => special_symbol(
                    SharedString::default(),
                    SourceHandle::FileScope,
                    sites,
                    project,
                    cx,
                ),
            })
            .collect()
    });
    Ok((symbols, callers.truncated))
}

/// The source files other than `root_file` that define a static function
/// named `name`: their calls of that name go to it.
async fn files_with_own_static(
    store: &Entity<SymbolIndexStore>,
    name: &str,
    root_file: Option<&ProjectPath>,
    cx: &mut AsyncApp,
) -> HashSet<ProjectPath> {
    // Without a place to be near, the store keeps every file's static functions.
    let definitions = store
        .update(cx, |store, cx| store.definitions(name, None, cx))
        .await
        .context("asking the symbol index for static functions")
        .log_err()
        .unwrap_or_default();
    definitions
        .into_iter()
        .filter(|definition| {
            definition.is_static
                && is_function_like(definition.kind)
                && !is_header_path(definition.path.path.as_unix_str())
                && Some(&definition.path) != root_file
        })
        .map(|definition| definition.path)
        .collect()
}

async fn index_callees(
    project: &Entity<Project>,
    store: &Entity<SymbolIndexStore>,
    definition: IndexedDefinition,
    cx: &mut AsyncApp,
) -> Result<Vec<CallSymbol>> {
    let callees = store
        .update(cx, |store, cx| {
            store.callees(definition.path.clone(), definition.name_range.start, cx)
        })
        .await
        .context("asking the symbol index for callees")?;
    let Some(callees) = callees else {
        return Ok(Vec::new());
    };

    struct CalleeGroup {
        name: String,
        member: bool,
        target: CalleeTarget,
        sites: Vec<IndexSite>,
    }
    let mut groups: Vec<CalleeGroup> = Vec::new();
    for callee in callees.callees {
        let member = callee.kind == CallKind::Member;
        let site = (callee.path, callee.range);
        match groups
            .iter_mut()
            .find(|group| group.name == callee.callee && group.member == member)
        {
            Some(group) => group.sites.push(site),
            None => groups.push(CalleeGroup {
                name: callee.callee,
                member,
                target: callee.target,
                sites: vec![site],
            }),
        }
    }

    let looked_up = groups
        .iter()
        .filter(|group| !group.member && group.target != CalleeTarget::Unresolved)
        .map(|group| group.name.clone())
        .collect::<Vec<_>>();
    let lookups = store.update(cx, |store, cx| {
        looked_up
            .iter()
            .map(|name| store.definitions(name, Some(definition.path.clone()), cx))
            .collect::<Vec<_>>()
    });
    let mut found = looked_up
        .into_iter()
        .zip(join_all(lookups).await)
        .map(|(name, definitions)| {
            (
                name,
                function_definitions(definitions.log_err().unwrap_or_default()),
            )
        })
        .collect::<Vec<_>>();

    Ok(project.read_with(cx, |project, cx| {
        groups
            .into_iter()
            .map(|group| {
                if group.member {
                    return special_symbol(
                        group.name.into(),
                        SourceHandle::Indirect,
                        &group.sites,
                        project,
                        cx,
                    );
                }
                let definitions = found
                    .iter_mut()
                    .find(|(name, _)| *name == group.name)
                    .map(|(_, definitions)| std::mem::take(definitions))
                    .unwrap_or_default();
                match definitions.as_slice() {
                    [] => special_symbol(
                        group.name.into(),
                        SourceHandle::Unresolved,
                        &group.sites,
                        project,
                        cx,
                    ),
                    [definition] => symbol_from_definition(
                        definition,
                        &group.sites,
                        CallOrigin::NameMatch,
                        project,
                        cx,
                    ),
                    candidates => {
                        let candidates = candidates
                            .iter()
                            .map(|candidate| {
                                symbol_from_definition(
                                    candidate,
                                    &[],
                                    CallOrigin::NameMatch,
                                    project,
                                    cx,
                                )
                            })
                            .collect();
                        special_symbol(
                            group.name.into(),
                            SourceHandle::Candidates(candidates),
                            &group.sites,
                            project,
                            cx,
                        )
                    }
                }
            })
            .collect()
    }))
}

/// The functions stored into members named `field`, for an indirect call
/// through such a member.
async fn index_pointer_candidates(
    project: Entity<Project>,
    field: String,
    cx: &mut AsyncApp,
) -> Result<IndexAnswer> {
    let store = symbol_index_store(&project, cx);
    let assignments = store
        .update(cx, |store, cx| store.assignments(&field, false, cx))
        .await
        .context("asking the symbol index for functions stored into the member")?;
    let mut groups: Vec<(String, Vec<String>, Vec<IndexSite>)> = Vec::new();
    for assignment in assignments {
        let site = (assignment.path, assignment.range);
        let group = match groups
            .iter_mut()
            .position(|(function, _, _)| *function == assignment.function)
        {
            Some(position) => groups.get_mut(position),
            None => {
                groups.push((assignment.function, Vec::new(), Vec::new()));
                groups.last_mut()
            }
        };
        if let Some((_, containers, sites)) = group {
            if let Some(container) = assignment.container
                && !containers.contains(&container)
            {
                containers.push(container);
            }
            sites.push(site);
        }
    }
    let symbols = project.read_with(cx, |project, cx| {
        groups
            .into_iter()
            .filter_map(|(function, containers, mut sites)| {
                sites.sort_by_key(|(path, range)| (path.clone(), range.start));
                let (first_path, first_range) = sites.first()?.clone();
                let location =
                    SymbolLocation::from_index(&first_path, first_range.clone(), project, cx);
                let call_sites = sites
                    .iter()
                    .map(|(path, range)| {
                        SymbolLocation::from_index(path, range.clone(), project, cx)
                    })
                    .collect();
                Some(CallSymbol {
                    name: function.into(),
                    detail: (!containers.is_empty())
                        .then(|| SharedString::from(containers.join(", "))),
                    kind: lsp::SymbolKind::FUNCTION,
                    origin: CallOrigin::FunctionPointerCandidate,
                    location,
                    call_sites,
                    display_path: index_display_path(&first_path, project, cx),
                    display_row: first_range.start.row,
                    c_family: true,
                    source: SourceHandle::Function {
                        language_server: None,
                        index: None,
                    },
                })
            })
            .collect::<Vec<_>>()
    });
    let partial = store.read_with(cx, |store, cx| {
        store.statuses(cx).iter().any(|(_, status)| {
            matches!(
                status,
                SymbolIndexStatus::Indexing { .. } | SymbolIndexStatus::Scanning
            )
        })
    });
    Ok(IndexAnswer {
        symbols,
        truncated: false,
        partial,
    })
}

fn index_display_path(path: &ProjectPath, project: &Project, cx: &App) -> Option<SharedString> {
    let worktree = project.worktree_for_id(path.worktree_id, cx)?;
    let full_path = worktree.read(cx).full_path(&path.path);
    Some(full_path.to_string_lossy().into_owned().into())
}

fn symbol_from_definition(
    definition: &IndexedDefinition,
    sites: &[IndexSite],
    origin: CallOrigin,
    project: &Project,
    cx: &App,
) -> CallSymbol {
    let is_macro = matches!(
        definition.kind,
        DefinitionKind::Macro | DefinitionKind::FunctionMacro
    );
    let location =
        SymbolLocation::from_index(&definition.path, definition.name_range.clone(), project, cx);
    let mut call_sites = sites
        .iter()
        .map(|(path, range)| SymbolLocation::from_index(path, range.clone(), project, cx))
        .collect::<Vec<_>>();
    call_sites.sort_by_key(|site| site.points.start_key());
    let (shown_path, shown_row) = match sites.iter().min_by_key(|(_, range)| range.start) {
        Some((path, range)) => (path, range.start.row),
        None => (&definition.path, definition.name_range.start.row),
    };
    CallSymbol {
        name: definition.name.clone().into(),
        detail: None,
        kind: match definition.kind {
            DefinitionKind::Method => lsp::SymbolKind::METHOD,
            DefinitionKind::Macro | DefinitionKind::FunctionMacro => lsp::SymbolKind::CONSTANT,
            _ => lsp::SymbolKind::FUNCTION,
        },
        origin: if is_macro { CallOrigin::Macro } else { origin },
        location,
        call_sites,
        display_path: index_display_path(shown_path, project, cx),
        display_row: shown_row,
        c_family: true,
        source: SourceHandle::Function {
            language_server: None,
            index: Some(definition.clone()),
        },
    }
}

/// A row that is not one function: its location is its first call site.
fn special_symbol(
    name: SharedString,
    source: SourceHandle,
    sites: &[IndexSite],
    project: &Project,
    cx: &App,
) -> CallSymbol {
    let mut call_sites = sites
        .iter()
        .map(|(path, range)| SymbolLocation::from_index(path, range.clone(), project, cx))
        .collect::<Vec<_>>();
    call_sites.sort_by_key(|site| site.points.start_key());
    let first = sites
        .iter()
        .min_by_key(|(path, range)| (path.clone(), range.start))
        .cloned();
    let location = match &first {
        Some((path, range)) => SymbolLocation::from_index(path, range.clone(), project, cx),
        None => SymbolLocation {
            buffer: WeakEntity::new_invalid(),
            range: None,
            points: LocationPoints::Utf8(Point::zero()..Point::zero()),
            abs_path: None,
            project_path: None,
        },
    };
    let kind = match source {
        SourceHandle::Indirect => lsp::SymbolKind::FIELD,
        _ => lsp::SymbolKind::FUNCTION,
    };
    CallSymbol {
        name,
        detail: None,
        kind,
        origin: CallOrigin::NameMatch,
        location,
        call_sites,
        display_path: first
            .as_ref()
            .and_then(|(path, _)| index_display_path(path, project, cx)),
        display_row: first.map_or(0, |(_, range)| range.start.row),
        c_family: true,
        source,
    }
}

fn anchors_for_points(snapshot: &BufferSnapshot, points: &Range<PointUtf16>) -> Range<Anchor> {
    let start = snapshot.clip_point_utf16(Unclipped(points.start), Bias::Left);
    let end = snapshot.clip_point_utf16(Unclipped(points.end), Bias::Right);
    snapshot.anchor_after(start)..snapshot.anchor_before(end)
}

/// Returns the location's buffer, opening it again when it has been released
/// (or, for a place the symbol index found, was never opened), and whether it
/// had to be opened (its anchors are then stale).
pub async fn resolve_buffer(
    project: &Entity<Project>,
    location: &SymbolLocation,
    server_id: Option<LanguageServerId>,
    cx: &mut AsyncApp,
) -> Result<(Entity<Buffer>, bool)> {
    if let Some(buffer) = location.buffer.upgrade() {
        return Ok((buffer, false));
    }
    let abs_path = || {
        location
            .abs_path
            .clone()
            .ok_or_else(|| anyhow!("the file of this call is no longer open and has no path"))
    };
    let is_local = project.read_with(cx, |project, _| project.is_local());
    let buffer = if is_local && let Some(server_id) = server_id {
        let abs_path = abs_path()?;
        // Opening through the language server registers files outside the
        // project's worktrees with that server, as the first answer did.
        let uri = lsp::Uri::from_file_path(&abs_path)
            .map_err(|()| anyhow!("cannot make a URI of {}", abs_path.display()))?;
        project
            .update(cx, |project, cx| {
                project.open_local_buffer_via_lsp(uri, server_id, cx)
            })
            .await?
    } else {
        // The worktree the file was found in is reused while it lives, so that
        // reopening neither asks the remote host to add a worktree again nor
        // depends on how its path compares with the client's path style.
        let known_worktree = location.project_path.as_ref().and_then(|project_path| {
            project.read_with(cx, |project, cx| {
                project.worktree_for_id(project_path.worktree_id, cx)
            })
        });
        // An invisible worktree is only weakly held by the project, so this
        // handle must outlive the opening of the buffer, which then holds it.
        let (worktree, project_path) = match (known_worktree, location.project_path.clone()) {
            (Some(worktree), Some(project_path)) => (worktree, project_path),
            _ => {
                let abs_path = abs_path()?;
                let (worktree, relative_path) = project
                    .update(cx, |project, cx| {
                        project.find_or_create_worktree(&abs_path, false, cx)
                    })
                    .await?;
                let project_path = ProjectPath {
                    worktree_id: worktree.read_with(cx, |worktree, _| worktree.id()),
                    path: relative_path,
                };
                (worktree, project_path)
            }
        };
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(project_path, cx))
            .await?;
        drop(worktree);
        buffer
    };
    Ok((buffer, true))
}

/// Returns the buffer and range to open for `location`.
pub async fn resolve_location(
    project: &Entity<Project>,
    location: &SymbolLocation,
    cx: &mut AsyncApp,
) -> Result<Location> {
    let (buffer, reopened) = resolve_buffer(project, location, None, cx).await?;
    let range = match (&location.range, reopened) {
        (Some(range), false) => range.clone(),
        _ => buffer.read_with(cx, |buffer, _| location.points.anchors(&buffer.snapshot())),
    };
    Ok(Location { buffer, range })
}

fn symbol_from_item(item: &CallHierarchyItem, sites: Vec<Location>, cx: &App) -> CallSymbol {
    let location = SymbolLocation::new(&item.buffer, item.selection_range.clone(), cx);
    let call_sites = sites
        .iter()
        .map(|site| SymbolLocation::new(&site.buffer, site.range.clone(), cx))
        .collect::<Vec<_>>();
    let shown = call_sites.first().unwrap_or(&location);
    let display_row = shown.points.start_row();
    let display_path = sites
        .first()
        .map(|site| &site.buffer)
        .or(Some(&item.buffer))
        .and_then(|buffer| display_path(buffer.read(cx), cx));
    let buffer = item.buffer.read(cx);
    let snapshot = buffer.snapshot();
    let full_point_range =
        item.range.start.to_point_utf16(&snapshot)..item.range.end.to_point_utf16(&snapshot);
    CallSymbol {
        name: SharedString::from(item.name.clone()),
        detail: signature_detail(item.detail.as_deref(), &item.name).map(SharedString::from),
        kind: item.kind,
        origin: CallOrigin::LanguageServer,
        location,
        call_sites,
        display_path,
        display_row,
        c_family: is_c_family(buffer),
        source: SourceHandle::Function {
            language_server: Some(ServerHandle {
                server_id: item.server_id,
                raw_detail: item.detail.clone(),
                full_range: item.range.clone(),
                full_point_range,
                data: item.data.clone(),
            }),
            index: None,
        },
    }
}

fn display_path(buffer: &Buffer, cx: &App) -> Option<SharedString> {
    let file = buffer.file()?;
    let path = if let Some(project_file) = project::File::from_dyn(Some(file))
        && project_file.worktree.read(cx).is_visible()
    {
        project_file.full_path(cx)
    } else if let Some(local_file) = file.as_local() {
        local_file.abs_path(cx).compact()
    } else {
        file.full_path(cx)
    };
    Some(path.to_string_lossy().into_owned().into())
}

/// The signature without the name and whatever precedes it, on one line:
/// `static int parse(const char *text)` becomes `(const char *text)`.
fn signature_detail(detail: Option<&str>, name: &str) -> Option<String> {
    let detail = detail.map(str::trim).filter(|detail| !detail.is_empty())?;
    let detail = detail
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace("( ", "(")
        .replace(", )", ")")
        .replace(" )", ")");
    let suffix = find_symbol_name(&detail, name)
        .and_then(|start| detail.get(start + name.len()..))
        .map(str::trim)
        .filter(|suffix| !suffix.is_empty())
        .map(ToOwned::to_owned);
    Some(suffix.unwrap_or(detail))
}

fn find_symbol_name(text: &str, symbol_name: &str) -> Option<usize> {
    if symbol_name.is_empty() {
        return None;
    }
    let is_word = |character: char| character.is_alphanumeric() || character == '_';
    text.match_indices(symbol_name)
        .map(|(start, _)| start)
        .find(|&start| {
            let boundary_before = text
                .get(..start)
                .and_then(|before| before.chars().next_back())
                .is_none_or(|character| !is_word(character));
            let boundary_after = text
                .get(start + symbol_name.len()..)
                .and_then(|after| after.chars().next())
                .is_none_or(|character| !is_word(character));
            boundary_before && boundary_after
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn location_at(file: &str, row: u32) -> SymbolLocation {
        SymbolLocation {
            buffer: WeakEntity::new_invalid(),
            range: None,
            points: LocationPoints::Utf16(PointUtf16::new(row, 0)..PointUtf16::new(row, 12)),
            abs_path: Some(PathBuf::from(file)),
            project_path: None,
        }
    }

    fn server_symbol(name: &str, row: u32, rows: Range<u32>) -> CallSymbol {
        let location = location_at("/src/handlers.c", row);
        CallSymbol {
            name: SharedString::from(name.to_string()),
            detail: None,
            kind: lsp::SymbolKind::FUNCTION,
            origin: CallOrigin::LanguageServer,
            location,
            call_sites: Vec::new(),
            display_path: None,
            display_row: row,
            c_family: true,
            source: SourceHandle::Function {
                language_server: Some(ServerHandle {
                    server_id: LanguageServerId(0),
                    raw_detail: None,
                    full_range: Anchor::min_min_range_for_buffer(
                        language::BufferId::new(1).expect("nonzero"),
                    ),
                    full_point_range: PointUtf16::new(rows.start, 0)..PointUtf16::new(rows.end, 1),
                    data: None,
                }),
                index: None,
            },
        }
    }

    fn symbol_at(name: &str, row: u32) -> CallSymbol {
        server_symbol(name, row, row..row)
    }

    fn index_symbol(name: &str, row: u32, sites: &[u32]) -> CallSymbol {
        CallSymbol {
            name: SharedString::from(name.to_string()),
            detail: None,
            kind: lsp::SymbolKind::FUNCTION,
            origin: CallOrigin::NameMatch,
            location: SymbolLocation {
                points: LocationPoints::Utf8(Point::new(row, 4)..Point::new(row, 10)),
                ..location_at("/src/handlers.c", row)
            },
            call_sites: sites
                .iter()
                .map(|row| location_at("/src/main.c", *row))
                .collect(),
            display_path: None,
            display_row: sites.first().copied().unwrap_or(row),
            c_family: true,
            source: SourceHandle::Function {
                language_server: None,
                index: None,
            },
        }
    }

    #[test]
    fn test_same_symbol_needs_the_same_name() {
        // Two functions a single macro invocation defines share its range.
        let init = symbol_at("handler_init", 3);
        let exit = symbol_at("handler_exit", 3);
        assert!(!init.same_symbol(&exit));
        assert!(init.same_symbol(&symbol_at("handler_init", 3)));
        assert!(!init.same_symbol(&symbol_at("handler_init", 4)));
    }

    #[test]
    fn test_same_symbol_across_sources() {
        let from_server = server_symbol("parse", 10, 9..20);
        assert!(
            from_server.same_symbol(&index_symbol("parse", 10, &[])),
            "the same row, whatever the columns"
        );
        assert!(
            from_server.same_symbol(&index_symbol("parse", 9, &[])),
            "the index's name inside the server's whole definition"
        );
        assert!(!from_server.same_symbol(&index_symbol("parse", 30, &[])));
        assert!(
            from_server.same_symbol(&index_symbol("parse", 20, &[])),
            "the definition's last row is still inside it"
        );
        assert!(!from_server.same_symbol(&index_symbol("parse", 21, &[])));
        assert!(!from_server.same_symbol(&index_symbol("parse", 8, &[])));
        assert!(
            server_symbol("Widget::parse", 10, 9..20).same_symbol(&index_symbol("parse", 10, &[])),
            "the index records the last part of a qualified name"
        );
        assert!(!from_server.same_symbol(&index_symbol("parser", 10, &[])));
        let mut elsewhere = index_symbol("parse", 10, &[]);
        elsewhere.location.abs_path = Some(PathBuf::from("/src/other.c"));
        assert!(!from_server.same_symbol(&elsewhere), "another file");
    }

    #[test]
    fn test_merge_puts_name_matches_after_exact_results() {
        let mut exact = server_symbol("caller_a", 10, 9..20);
        exact.call_sites = vec![location_at("/src/main.c", 12)];
        let merged = merge(
            vec![exact],
            vec![
                index_symbol("caller_b", 40, &[41]),
                index_symbol("caller_a", 10, &[12, 15]),
            ],
        );
        let names = merged
            .iter()
            .map(|symbol| (symbol.name.to_string(), symbol.origin))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                ("caller_a".to_string(), CallOrigin::LanguageServer),
                ("caller_b".to_string(), CallOrigin::NameMatch),
            ]
        );
        let rows = merged
            .first()
            .map(|symbol| {
                symbol
                    .call_sites
                    .iter()
                    .map(|site| site.points.start_row())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        assert_eq!(
            rows,
            vec![12, 15],
            "the union of the call sites, each line once"
        );
    }

    #[test]
    fn test_combine_prefers_an_answer_over_an_error() {
        let options = FetchOptions::default();
        let from_index = IndexAnswer {
            symbols: vec![index_symbol("caller_b", 40, &[41])],
            truncated: true,
            partial: false,
        };
        let children = combine(
            CallDirection::Incoming,
            Some(Err(anyhow!("the server is busy"))),
            Some(Ok(from_index)),
            &options,
        )
        .expect("the index's answer stands in");
        assert_eq!(children.symbols.len(), 1);
        assert!(children.truncated);

        let error = combine(
            CallDirection::Incoming,
            Some(Err(anyhow!("the server is busy"))),
            Some(Ok(IndexAnswer::default())),
            &options,
        );
        assert!(
            error.is_err(),
            "an empty name match does not hide the error"
        );

        let children = combine(
            CallDirection::Incoming,
            Some(Ok(Some(LanguageServerAnswer {
                symbols: Vec::new(),
                opened_buffers: Vec::new(),
            }))),
            Some(Ok(IndexAnswer {
                symbols: vec![index_symbol("caller_b", 40, &[41])],
                ..IndexAnswer::default()
            })),
            &options,
        )
        .expect("an answer");
        assert_eq!(
            children.symbols.len(),
            1,
            "an empty exact answer does not hide name matches"
        );
        assert!(children.language_server_empty);
    }

    #[test]
    fn test_excluded_callees_are_set_apart() {
        let options = FetchOptions {
            sources: SourceFilter::All,
            excluded_callees: vec!["printk".to_string()].into(),
        };
        let answer = || IndexAnswer {
            symbols: vec![
                index_symbol("printk", 40, &[41]),
                index_symbol("parse", 50, &[42]),
                index_symbol("printk_once", 60, &[43]),
            ],
            ..IndexAnswer::default()
        };
        let callees = combine(CallDirection::Outgoing, None, Some(Ok(answer())), &options)
            .expect("an answer");
        assert_eq!(
            callees
                .symbols
                .iter()
                .map(|symbol| symbol.name.to_string())
                .collect::<Vec<_>>(),
            vec!["parse", "printk_once"],
            "a name is excluded only when it is the whole name"
        );
        assert_eq!(callees.excluded.len(), 1);
        let callers = combine(CallDirection::Incoming, None, Some(Ok(answer())), &options)
            .expect("an answer");
        assert_eq!(callers.symbols.len(), 3, "only callees are excluded");
    }

    #[test]
    fn test_signature_detail() {
        assert_eq!(
            signature_detail(Some("static int parse(const char *text)"), "parse").as_deref(),
            Some("(const char *text)")
        );
        assert_eq!(
            signature_detail(Some("fn f(\n    a: i32,\n) -> i32"), "f").as_deref(),
            Some("(a: i32) -> i32")
        );
        assert_eq!(signature_detail(Some("   "), "f"), None);
        assert_eq!(
            signature_detail(Some("void other()"), "missing").as_deref(),
            Some("void other()")
        );
        assert_eq!(
            signature_detail(Some("int parse_all()"), "parse").as_deref(),
            Some("int parse_all()")
        );
    }
}
