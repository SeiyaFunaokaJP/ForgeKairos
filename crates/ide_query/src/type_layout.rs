use std::{
    fmt,
    pin::pin,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use collections::{HashMap, HashSet};
use futures::{
    FutureExt as _,
    future::{self, Either, LocalBoxFuture},
};
use gpui::{App, AsyncApp, Entity, Task};
use project::{
    Project, ProjectPath, WorktreeId,
    symbol_index_store::{
        ChainRootOrigin, Confidence, DEFAULT_TYPE_TREE_NODES, DEFAULT_TYPE_TREE_POINTER_HOPS,
        DefinitionKind, MAX_DEFINITIONS, MAX_TYPE_READ_LIMIT, MacroSummary, MemberExpansionTarget,
        MemberSelector, MemberSummary, MemberSummaryKind, ResolvedTypes, SymbolIndexStatus,
        SymbolIndexStore, SymbolIndexUnavailable, TagKind, TypeHoverSubject, TypeSummary,
        TypeTreeOptions, TypeTreeSubject,
    },
};

use crate::{LinePosition, agent_withheld_reason, open_target, resolve_position};

pub const DEFAULT_TYPE_LAYOUT_DEPTH: usize = 2;
pub const MAX_TYPE_LAYOUT_DEPTH: usize = 4;
pub const DEFAULT_TYPE_LAYOUT_CHARS: usize = 8_000;
pub const MAX_TYPE_LAYOUT_CHARS: usize = 16_000;
const MIN_TYPE_LAYOUT_CHARS: usize = 256;
/// Same-name definitions listed besides the one shown, by location only.
pub const MAX_OTHER_TYPE_CANDIDATES: usize = 8;
/// Bounds the whole answer; each request to the index has its own, shorter limit.
pub const TYPE_LAYOUT_TIME_BUDGET: Duration = Duration::from_secs(20);
/// Requests to the index one answer may make when it has to walk the type itself (every
/// member it expands is one, and over a remote connection one round trip).
pub const MAX_TYPE_LAYOUT_REQUESTS: usize = 40;
/// The walk stops expanding this long before [`TYPE_LAYOUT_TIME_BUDGET`] runs out, so that what
/// it found is still returned instead of a time-out.
const WALK_TIME_MARGIN: Duration = Duration::from_secs(5);
const MAX_TYPE_NAME_CHARS: usize = 200;
const TRUNCATED_MARKER: &str = "... (truncated)";
const MORE_MEMBERS_MARKER: &str = "... (more members not shown)";
pub const WITHHELD_MARKER: &str = "(withheld)";

/// What to describe: a type, typedef or macro by name, or the name at a position (a type name,
/// a variable, a member in `a->b.c`, a macro), whose type is found from its declaration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeLayoutSubject {
    Name {
        /// `foo_t`, or with its tag: `struct foo`.
        name: String,
        /// Same-name definitions closer to this file rank first.
        near: Option<ProjectPath>,
    },
    Position {
        path: ProjectPath,
        position: LinePosition,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeLayoutRequest {
    pub subject: TypeLayoutSubject,
    /// Levels of named types expanded below the type. Zero means
    /// [`DEFAULT_TYPE_LAYOUT_DEPTH`]; at most [`MAX_TYPE_LAYOUT_DEPTH`].
    pub depth: usize,
    /// Zero means [`DEFAULT_TYPE_LAYOUT_CHARS`]; at most [`MAX_TYPE_LAYOUT_CHARS`].
    pub max_chars: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypeLayoutKind {
    Type,
    Macro,
    /// Nothing that may be shown was found; see [`TypeLayoutReport::withheld`].
    NotFound,
}

/// 1-based line.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TypeLayoutLocation {
    pub path: ProjectPath,
    pub line: u32,
}

/// The type of the name at a position, as its declaration writes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InferredType {
    /// The variable, parameter or member at the position.
    pub name: String,
    pub type_text: String,
    /// `None` for a member of `a->b.c`, which is found through the types along the way.
    pub confidence: Option<Confidence>,
    /// For a member: the type it is a member of.
    pub member_of: Option<String>,
}

/// A type's members as indented text, found by name in the symbol index (not resolved by a
/// language server). Nothing in it comes from a file the agent may not read: such
/// definitions are only counted in `withheld`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeLayoutReport {
    pub kind: TypeLayoutKind,
    /// The name asked about, or the name at the position.
    pub requested: String,
    /// `struct foo`, `foo_t`, or the macro's name.
    pub title: String,
    pub definition: Option<TypeLayoutLocation>,
    /// The tree (see `symbol_index::render_text`), or the macros' `#define` lines.
    /// Locations in it are relative to the root folder of the definition's worktree.
    pub text: String,
    /// Every definition the text names.
    pub definitions: Vec<TypeLayoutLocation>,
    /// Same-name definitions that may be shown, including the one shown.
    pub candidates: usize,
    /// More same-name definitions exist than the index listed.
    pub candidates_truncated: bool,
    /// The others, by location; at most [`MAX_OTHER_TYPE_CANDIDATES`].
    pub other_candidates: Vec<TypeLayoutLocation>,
    /// Definitions left out because they lie in files the agent may not read.
    pub withheld: usize,
    pub inferred: Option<InferredType>,
    /// Branches of the tree were cut at definitions the agent may not read.
    pub pruned: bool,
    pub truncated: bool,
    /// Set while the index is still being built.
    pub index_note: Option<String>,
}

impl TypeLayoutReport {
    fn not_found(requested: String, withheld: usize) -> Self {
        Self {
            kind: TypeLayoutKind::NotFound,
            requested,
            title: String::new(),
            definition: None,
            text: String::new(),
            definitions: Vec::new(),
            candidates: 0,
            candidates_truncated: false,
            other_candidates: Vec::new(),
            withheld,
            inferred: None,
            pruned: false,
            truncated: false,
            index_note: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeIndexUnavailableReason {
    /// The index only covers C and C++.
    NotCOrCpp,
    Disabled,
    /// The worktree is still being scanned, or a remote host has not said yet whether it has
    /// an index.
    NotReady,
    UnsupportedHost,
    Collaboration,
    InUseByAnotherWindow,
    Failed,
}

/// What [`type_layout`] fails with when the symbol index can't answer at all, which differs
/// from "no such type". Callers find it with `error.downcast_ref::<TypeIndexUnavailable>()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeIndexUnavailable {
    pub reason: TypeIndexUnavailableReason,
    /// The file asked about, for [`TypeIndexUnavailableReason::NotCOrCpp`].
    pub path: Option<String>,
}

impl fmt::Display for TypeIndexUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.reason {
            TypeIndexUnavailableReason::NotCOrCpp => write!(
                formatter,
                "Name index not available: `{}` isn't a C or C++ file, and the name index only covers C and C++",
                self.path.as_deref().unwrap_or_default()
            ),
            TypeIndexUnavailableReason::Disabled => write!(
                formatter,
                "Name index not available: the symbol index is turned off, or has no C or C++ files to index in this project"
            ),
            TypeIndexUnavailableReason::NotReady => write!(
                formatter,
                "Name index not available yet: the project is still being scanned"
            ),
            TypeIndexUnavailableReason::UnsupportedHost => write!(
                formatter,
                "Name index not available: the remote server is too old to have one"
            ),
            TypeIndexUnavailableReason::Collaboration => write!(
                formatter,
                "Name index not available in a project shared by someone else"
            ),
            TypeIndexUnavailableReason::InUseByAnotherWindow => write!(
                formatter,
                "Name index not available: another Zed window has this folder's index open"
            ),
            TypeIndexUnavailableReason::Failed => write!(
                formatter,
                "Name index not available: it failed to open (details are in Zed's log)"
            ),
        }
    }
}

impl std::error::Error for TypeIndexUnavailable {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TypeLayoutTimedOut;

impl fmt::Display for TypeLayoutTimedOut {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "The name index didn't answer within {} seconds",
            TYPE_LAYOUT_TIME_BUDGET.as_secs()
        )
    }
}

impl std::error::Error for TypeLayoutTimedOut {}

/// Decides which definitions may reach the agent that asked. The store already leaves out what
/// the settings hide; this is the caller's own judgement on top (outside the project, a
/// sandbox's roots, a remote host's verdict), applied to every definition before its name,
/// members or location are used.
pub trait TypeLayoutPaths {
    fn allows<'a>(
        &'a mut self,
        path: &'a ProjectPath,
        cx: &'a mut AsyncApp,
    ) -> LocalBoxFuture<'a, bool>;
}

/// The judgement for Zed's own agent: [`agent_withheld_reason`].
pub struct AgentTypeLayoutPaths {
    project: Entity<Project>,
    checked: HashMap<ProjectPath, bool>,
}

impl AgentTypeLayoutPaths {
    pub fn new(project: Entity<Project>) -> Self {
        Self {
            project,
            checked: HashMap::default(),
        }
    }
}

impl TypeLayoutPaths for AgentTypeLayoutPaths {
    fn allows<'a>(
        &'a mut self,
        path: &'a ProjectPath,
        cx: &'a mut AsyncApp,
    ) -> LocalBoxFuture<'a, bool> {
        async move {
            if let Some(allowed) = self.checked.get(path) {
                return *allowed;
            }
            let allowed = self.project.read_with(cx, |project, cx| {
                agent_withheld_reason(project, path, cx).is_none()
            });
            self.checked.insert(path.clone(), allowed);
            allowed
        }
        .boxed_local()
    }
}

/// What a C or C++ type (or typedef, or macro) holds, for Zed's own agent.
pub fn type_layout(
    project: &Entity<Project>,
    request: TypeLayoutRequest,
    cx: &mut App,
) -> Task<Result<TypeLayoutReport>> {
    let project = project.clone();
    cx.spawn(async move |cx| {
        let mut paths = AgentTypeLayoutPaths::new(project.clone());
        type_layout_with(&project, request, &mut paths, cx).await
    })
}

/// [`type_layout`] with the caller's own judgement of which definitions may be shown.
pub async fn type_layout_with(
    project: &Entity<Project>,
    request: TypeLayoutRequest,
    paths: &mut dyn TypeLayoutPaths,
    cx: &mut AsyncApp,
) -> Result<TypeLayoutReport> {
    let executor = cx.background_executor().clone();
    let deadline = executor.now() + TYPE_LAYOUT_TIME_BUDGET;
    let timer = executor.timer(TYPE_LAYOUT_TIME_BUDGET);
    let store = project.read_with(cx, |project, _| project.symbol_index_store().clone());
    let mut finder = Finder {
        project,
        store,
        paths,
        options: TypeTreeOptions {
            max_depth: match request.depth {
                0 => DEFAULT_TYPE_LAYOUT_DEPTH,
                depth => depth.min(MAX_TYPE_LAYOUT_DEPTH),
            },
            max_pointer_hops: DEFAULT_TYPE_TREE_POINTER_HOPS,
            max_nodes: DEFAULT_TYPE_TREE_NODES,
            max_chars: match request.max_chars {
                0 => DEFAULT_TYPE_LAYOUT_CHARS,
                max_chars => max_chars.clamp(MIN_TYPE_LAYOUT_CHARS, MAX_TYPE_LAYOUT_CHARS),
            },
        },
        deadline,
        requests: 0,
    };
    let work = finder.find(request.subject, cx);
    match future::select(pin!(work), pin!(timer)).await {
        Either::Left((result, _)) => result,
        Either::Right(_) => Err(anyhow!(TypeLayoutTimedOut)),
    }
}

struct Finder<'a> {
    project: &'a Entity<Project>,
    store: Entity<SymbolIndexStore>,
    paths: &'a mut dyn TypeLayoutPaths,
    options: TypeTreeOptions,
    deadline: Instant,
    requests: usize,
}

type DefinitionKey = (ProjectPath, u32);

enum Expansion {
    Type(TypeSummary),
    Withheld,
    NotFound,
    NotExpandable,
    Stopped,
}

struct Layout {
    text: String,
    definitions: Vec<TypeLayoutLocation>,
    truncated: bool,
    pruned: bool,
    withheld: usize,
}

impl Finder<'_> {
    async fn find(
        &mut self,
        subject: TypeLayoutSubject,
        cx: &mut AsyncApp,
    ) -> Result<TypeLayoutReport> {
        match subject {
            TypeLayoutSubject::Name { name, near } => self.by_name(&name, near, cx).await,
            TypeLayoutSubject::Position { path, position } => {
                self.at_position(path, position, cx).await
            }
        }
    }

    async fn by_name(
        &mut self,
        text: &str,
        near: Option<ProjectPath>,
        cx: &mut AsyncApp,
    ) -> Result<TypeLayoutReport> {
        let (name, tag) = parse_type_name(text)?;
        let index_note = self.availability(None, cx)?;
        let candidates = self
            .store
            .update(cx, |store, cx| {
                store.resolve_type(&name, tag, near.clone(), 1, cx)
            })
            .await?;
        let requested = match tag {
            Some(tag) => format!("{} {name}", tag.keyword()),
            None => name.clone(),
        };
        let mut report = self
            .report_for_candidates(requested, candidates, cx)
            .await?;
        if report.kind == TypeLayoutKind::NotFound && tag.is_none() {
            let withheld = report.withheld;
            report = self.macros_named(&name, near, cx).await?;
            report.withheld += withheld;
        }
        report.index_note = index_note;
        Ok(report)
    }

    async fn at_position(
        &mut self,
        path: ProjectPath,
        position: LinePosition,
        cx: &mut AsyncApp,
    ) -> Result<TypeLayoutReport> {
        let target = open_target(self.project, &path, cx).await?;
        if !self.paths.allows(&path, cx).await {
            return Err(anyhow!(
                "`{}` can't be used because it lies outside the project or can't be read",
                target.display_path
            ));
        }
        if !symbol_index::is_indexable_path(path.path.as_unix_str()) {
            return Err(anyhow!(TypeIndexUnavailable {
                reason: TypeIndexUnavailableReason::NotCOrCpp,
                path: Some(target.display_path.clone()),
            }));
        }
        let index_note = self.availability(Some(path.worktree_id), cx)?;
        let (snapshot, point) = target.buffer.read_with(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            let point = resolve_position(&snapshot, &position, &target.display_path)?;
            anyhow::Ok((snapshot, point))
        })?;
        self.requests += 1;
        let hover = self
            .store
            .update(cx, |store, cx| {
                store.describe_type_at(path.clone(), point, Some(snapshot), None, 1, cx)
            })
            .await?;
        let Some(hover) = hover else {
            let requested = position
                .symbol
                .clone()
                .unwrap_or_else(|| format!("line {}", position.line));
            let mut report = TypeLayoutReport::not_found(requested, 0);
            report.index_note = index_note;
            return Ok(report);
        };
        let mut report = match hover.subject {
            TypeHoverSubject::Macro { macros } => self.macro_report(hover.name, macros, cx).await,
            TypeHoverSubject::TypeName { .. } => {
                self.report_for_candidates(hover.name, hover.types, cx)
                    .await?
            }
            TypeHoverSubject::Variable {
                type_text,
                confidence,
                ..
            } => {
                // `NameOnly` read the declaration from a same-name global in some file the
                // store doesn't name, so its type is shown only when every such declaration
                // may be.
                if confidence == Confidence::NameOnly
                    && !self
                        .declarations_shown(&hover.name, DefinitionKind::Variable, cx)
                        .await
                {
                    let mut report = TypeLayoutReport::not_found(hover.name, 1);
                    report.index_note = index_note;
                    return Ok(report);
                }
                let mut report = self
                    .report_for_candidates(hover.name.clone(), hover.types, cx)
                    .await?;
                report.inferred = Some(InferredType {
                    name: hover.name,
                    type_text,
                    confidence: Some(confidence),
                    member_of: None,
                });
                report
            }
            TypeHoverSubject::Member {
                steps,
                root_type,
                root_origin,
                root_name,
                ..
            } => {
                // The root's type may have been read from a global's or a function's
                // declaration in another file, judged as a `NameOnly` variable is.
                let mut chain_shown = match (root_origin, root_name.as_deref()) {
                    _ if root_type.is_none() => true,
                    (Some(ChainRootOrigin::InFile), _) => true,
                    (Some(ChainRootOrigin::Global), Some(name)) => {
                        self.declarations_shown(name, DefinitionKind::Variable, cx)
                            .await
                    }
                    (Some(ChainRootOrigin::Call), Some(name)) => {
                        self.declarations_shown(name, DefinitionKind::Function, cx)
                            .await
                    }
                    // A host too old to say where the root came from can't be judged.
                    _ => false,
                };
                // A member's declaration lies in the file of the type holding it, so every
                // owner along `a->b.c` has to be shown before anything of the last member is.
                for step in &steps {
                    let Some(owner) = &step.owner else {
                        continue;
                    };
                    // An owner whose file couldn't be named can't be judged.
                    if owner.path.is_none() {
                        chain_shown = false;
                    }
                    for owner_path in owner
                        .path
                        .iter()
                        .chain(owner.type_ref.as_ref().map(|type_ref| &type_ref.path))
                    {
                        if !self.paths.allows(owner_path, cx).await {
                            chain_shown = false;
                        }
                    }
                }
                if !chain_shown {
                    TypeLayoutReport::not_found(hover.name, 1)
                } else {
                    let last = steps.last();
                    let member = last.and_then(|step| step.member.as_ref());
                    let inferred = member.map(|member| InferredType {
                        name: hover.name.clone(),
                        type_text: member.type_text.clone(),
                        confidence: None,
                        member_of: last
                            .and_then(|step| step.owner.as_ref())
                            .map(|owner| owner.display_name.clone()),
                    });
                    let inline = member.and_then(|member| member.inline.as_deref());
                    let mut report = match inline {
                        Some(inline) => self.inline_report(hover.name.clone(), inline, cx).await?,
                        None => {
                            self.report_for_candidates(hover.name.clone(), hover.types, cx)
                                .await?
                        }
                    };
                    report.inferred = inferred;
                    report
                }
            }
        };
        report.index_note = index_note;
        Ok(report)
    }

    /// `Ok(note)` when the index can answer (the note says it is still being built);
    /// `worktree_id` is the file asked about, or `None` for every visible worktree.
    fn availability(
        &self,
        worktree_id: Option<WorktreeId>,
        cx: &mut AsyncApp,
    ) -> Result<Option<String>> {
        let statuses = self.project.read_with(cx, |_, cx| {
            let store = self.store.read(cx);
            match worktree_id {
                Some(worktree_id) => vec![store.status(worktree_id)],
                None => store
                    .statuses(cx)
                    .into_iter()
                    .map(|(_, status)| status)
                    .collect(),
            }
        });
        let mut indexing = None;
        let mut ready = false;
        let mut failure = None;
        for status in statuses {
            let reason = match status {
                SymbolIndexStatus::Ready { .. } => {
                    ready = true;
                    continue;
                }
                SymbolIndexStatus::Indexing { done, total } => {
                    indexing = Some((done, total));
                    continue;
                }
                SymbolIndexStatus::Disabled => TypeIndexUnavailableReason::Disabled,
                SymbolIndexStatus::Scanning => TypeIndexUnavailableReason::NotReady,
                SymbolIndexStatus::Unavailable { reason } => match reason {
                    SymbolIndexUnavailable::InUseByAnotherWindow => {
                        TypeIndexUnavailableReason::InUseByAnotherWindow
                    }
                    SymbolIndexUnavailable::UnsupportedHost => {
                        TypeIndexUnavailableReason::UnsupportedHost
                    }
                    SymbolIndexUnavailable::Collaboration => {
                        TypeIndexUnavailableReason::Collaboration
                    }
                    SymbolIndexUnavailable::Error(message) => {
                        log::warn!("type layout: the symbol index failed: {message}");
                        TypeIndexUnavailableReason::Failed
                    }
                },
            };
            // "Turned off" is the least telling reason when another worktree says more.
            if failure.is_none() || failure == Some(TypeIndexUnavailableReason::Disabled) {
                failure = Some(reason);
            }
        }
        if let Some((done, total)) = indexing {
            return Ok(Some(format!(
                "The name index is still being built ({done} of {total} files), so some definitions may be missing."
            )));
        }
        if ready {
            return Ok(None);
        }
        Err(anyhow!(TypeIndexUnavailable {
            reason: failure.unwrap_or(TypeIndexUnavailableReason::Disabled),
            path: None,
        }))
    }

    /// Whether every global variable (`kind` [`DefinitionKind::Variable`]) or function
    /// ([`DefinitionKind::Function`], methods too) named `name` the index knows lies in a file
    /// that may be shown. An empty or capped list can't vouch for the one the type was read
    /// from.
    async fn declarations_shown(
        &mut self,
        name: &str,
        kind: DefinitionKind,
        cx: &mut AsyncApp,
    ) -> bool {
        let wanted = |found: DefinitionKind| match kind {
            DefinitionKind::Function => found.is_function_like(),
            _ => found == kind,
        };
        self.requests += 1;
        // Not near the file asked about: that would leave out the other files' `static`
        // namesakes, which the type engine may still have read the type from.
        let definitions = match self
            .store
            .update(cx, |store, cx| store.definitions(name, None, cx))
            .await
        {
            Ok(definitions) => definitions,
            Err(error) => {
                log::warn!("type layout: listing the declarations of a variable failed: {error:#}");
                return false;
            }
        };
        if definitions.len() >= MAX_DEFINITIONS {
            return false;
        }
        let mut found = false;
        for definition in definitions {
            if !wanted(definition.kind) {
                continue;
            }
            found = true;
            if !self.paths.allows(&definition.path, cx).await {
                return false;
            }
        }
        found
    }

    async fn summary_shown(&mut self, summary: &TypeSummary, cx: &mut AsyncApp) -> bool {
        if !self.paths.allows(&summary.path, cx).await {
            return false;
        }
        if let Some(type_ref) = &summary.type_ref
            && !self.paths.allows(&type_ref.path, cx).await
        {
            return false;
        }
        for step in &summary.typedef_chain {
            if !self.paths.allows(&step.path, cx).await {
                return false;
            }
        }
        true
    }

    async fn report_for_candidates(
        &mut self,
        requested: String,
        candidates: ResolvedTypes,
        cx: &mut AsyncApp,
    ) -> Result<TypeLayoutReport> {
        let mut shown = Vec::new();
        let mut withheld = 0;
        for summary in candidates.types {
            if self.summary_shown(&summary, cx).await {
                shown.push(summary);
            } else {
                withheld += 1;
            }
        }
        let Some(root) = shown.first().cloned() else {
            return Ok(TypeLayoutReport::not_found(requested, withheld));
        };
        let layout = self.layout_of(&root, cx).await?;
        Ok(TypeLayoutReport {
            kind: TypeLayoutKind::Type,
            requested,
            title: root.name.clone(),
            definition: Some(location_of(&root)),
            text: layout.text,
            definitions: layout.definitions,
            candidates: candidates.total.saturating_sub(withheld).max(shown.len()),
            candidates_truncated: candidates.truncated,
            other_candidates: shown
                .iter()
                .skip(1)
                .take(MAX_OTHER_TYPE_CANDIDATES)
                .map(location_of)
                .collect(),
            withheld: withheld + layout.withheld,
            inferred: None,
            pruned: layout.pruned,
            truncated: layout.truncated,
            index_note: None,
        })
    }

    async fn inline_report(
        &mut self,
        requested: String,
        inline: &TypeSummary,
        cx: &mut AsyncApp,
    ) -> Result<TypeLayoutReport> {
        if !self.summary_shown(inline, cx).await {
            return Ok(TypeLayoutReport::not_found(requested, 1));
        }
        let layout = self.walk(inline, cx).await;
        Ok(TypeLayoutReport {
            kind: TypeLayoutKind::Type,
            requested,
            title: inline.name.clone(),
            definition: Some(location_of(inline)),
            text: layout.text,
            definitions: layout.definitions,
            candidates: 1,
            candidates_truncated: false,
            other_candidates: Vec::new(),
            withheld: layout.withheld,
            inferred: None,
            pruned: layout.pruned,
            truncated: layout.truncated,
            index_note: None,
        })
    }

    /// The index renders the whole tree in one request; that text is used as it is unless it
    /// names a definition the agent may not read, in which case the tree is walked here one
    /// member at a time and such branches are cut.
    async fn layout_of(&mut self, root: &TypeSummary, cx: &mut AsyncApp) -> Result<Layout> {
        if let Some(type_ref) = &root.type_ref {
            self.requests += 1;
            let options = self.options;
            let tree = self
                .store
                .update(cx, |store, cx| {
                    store.type_tree_text(TypeTreeSubject::Ref(type_ref.clone()), options, cx)
                })
                .await?;
            if let Some(tree) = tree {
                let mut all_shown = true;
                for (path, _) in &tree.definitions {
                    if !self.paths.allows(path, cx).await {
                        all_shown = false;
                        break;
                    }
                }
                if all_shown {
                    // The tree is asked for by the definition's own reference, which starts
                    // at the type the typedefs lead to.
                    let mut text = String::new();
                    for step in &root.typedef_chain {
                        text.push_str(&step.name);
                        text.push_str(" -> ");
                    }
                    text.push_str(&tree.text);
                    let mut definitions = Vec::new();
                    let typedef_rows = root
                        .typedef_chain
                        .iter()
                        .map(|step| (step.path.clone(), step.range.start.row));
                    for (path, row) in tree.definitions.into_iter().chain(typedef_rows) {
                        let location = TypeLayoutLocation {
                            path,
                            line: row + 1,
                        };
                        if !definitions.contains(&location) {
                            definitions.push(location);
                        }
                    }
                    return Ok(Layout {
                        text,
                        definitions,
                        truncated: tree.truncated,
                        pruned: false,
                        withheld: 0,
                    });
                }
            }
        }
        let mut root = root.clone();
        if !root.members_read {
            let read = self.read_members(&root, cx).await;
            // The definition read again may come with its own typedef chain.
            if self.summary_shown(&read, cx).await {
                root = read;
            }
        }
        Ok(self.walk(&root, cx).await)
    }

    async fn read_members(&mut self, summary: &TypeSummary, cx: &mut AsyncApp) -> TypeSummary {
        let Some(type_ref) = summary.type_ref.clone() else {
            return summary.clone();
        };
        self.requests += 1;
        let near = Some(summary.path.clone());
        let found = self
            .store
            .update(cx, |store, cx| {
                store.resolve_type(&type_ref.name, None, near, MAX_TYPE_READ_LIMIT, cx)
            })
            .await;
        match found {
            Ok(found) => found
                .types
                .into_iter()
                .find(|candidate| {
                    candidate.members_read && candidate.type_ref.as_ref() == Some(&type_ref)
                })
                .unwrap_or_else(|| summary.clone()),
            Err(error) => {
                log::warn!(
                    "type layout: reading the members of {} failed: {error:#}",
                    type_ref.name
                );
                summary.clone()
            }
        }
    }

    async fn walk(&mut self, root: &TypeSummary, cx: &mut AsyncApp) -> Layout {
        let mut writer = TreeWriter::new(self.options.max_chars, self.options.max_nodes);
        let mut header = String::new();
        for step in &root.typedef_chain {
            header.push_str(&step.name);
            header.push_str(" -> ");
        }
        header.push_str(&root.name);
        header.push_str(&format!(
            "  ({}:{})",
            root.path.path.as_unix_str(),
            root.range.start.row + 1
        ));
        if let Some(condition) = &root.condition {
            header.push_str(&format!("  [#if {condition}]"));
        }
        writer.line(0, &header);
        writer.add_definition(&root.path, root.range.start.row);
        for step in &root.typedef_chain {
            writer.add_definition(&step.path, step.range.start.row);
        }
        if root.members.is_empty()
            && root.enumerators.is_empty()
            && let Some(alias) = &root.alias
        {
            writer.line(1, &format!("= {alias}"));
        }
        writer.truncated |= root.truncated;
        let key = key_of(root);
        writer.expanded.insert(key.clone());
        let mut ancestors = vec![key];
        let depth = self.options.max_depth;
        let hops = self.options.max_pointer_hops;
        self.write_members(root, 1, depth, hops, &mut ancestors, &mut writer, cx)
            .await;
        if writer.nodes_cut && !writer.full {
            writer.max_lines = usize::MAX;
            writer.line(1, MORE_MEMBERS_MARKER);
        }
        Layout {
            truncated: writer.truncated || writer.nodes_cut || writer.full,
            pruned: writer.withheld > 0,
            withheld: writer.withheld,
            text: writer.text,
            definitions: writer.definitions,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn write_members<'b>(
        &'b mut self,
        summary: &'b TypeSummary,
        indent: usize,
        depth_left: usize,
        hops_left: usize,
        ancestors: &'b mut Vec<DefinitionKey>,
        writer: &'b mut TreeWriter,
        cx: &'b mut AsyncApp,
    ) -> LocalBoxFuture<'b, ()> {
        async move {
            for member in &summary.members {
                if writer.full || writer.nodes_cut {
                    return;
                }
                let mut text = member.text.clone();
                let inline = member.inline.as_deref();
                let mut child = None;
                let follows_pointer = member.pointer_depth > 0 || is_reference(&member.type_text);
                if inline.is_none()
                    && member.expandable
                    && depth_left > 0
                    && (!follows_pointer || hops_left > 0)
                {
                    match self.expand(summary, member, cx).await {
                        Expansion::Type(found) => {
                            let key = key_of(&found);
                            if ancestors.contains(&key) {
                                text.push_str("  (recursive)");
                            } else if writer.expanded.contains(&key) {
                                text.push_str("  (see above)");
                            } else if found.members.is_empty() && found.enumerators.is_empty() {
                                if let Some(alias) = &found.alias {
                                    text.push_str(&format!("  (= {alias})"));
                                }
                            } else {
                                text.push_str(&format!(
                                    "  ({}:{})",
                                    found.path.path.as_unix_str(),
                                    found.range.start.row + 1
                                ));
                                writer.expanded.insert(key.clone());
                                writer.add_definition(&found.path, found.range.start.row);
                                for step in &found.typedef_chain {
                                    writer.add_definition(&step.path, step.range.start.row);
                                }
                                child = Some((found, key));
                            }
                        }
                        Expansion::Withheld => {
                            text.push_str("  ");
                            text.push_str(WITHHELD_MARKER);
                            writer.withheld += 1;
                        }
                        Expansion::NotFound => text.push_str("  (not found)"),
                        Expansion::NotExpandable => {}
                        Expansion::Stopped => writer.truncated = true,
                    }
                }
                if member.kind == MemberSummaryKind::Unparsed {
                    text.push_str("  (unparsed)");
                }
                if let Some(condition) = &member.condition {
                    text.push_str(&format!("  [#if {condition}]"));
                }
                if let Some(comment) = &member.comment {
                    text.push_str(&format!("  // {comment}"));
                }
                writer.line(indent, &text);
                if let Some(inline) = inline {
                    self.write_members(
                        inline,
                        indent + 1,
                        depth_left,
                        hops_left,
                        ancestors,
                        writer,
                        cx,
                    )
                    .await;
                } else if let Some((found, key)) = child {
                    ancestors.push(key);
                    let hops_left = if follows_pointer {
                        hops_left.saturating_sub(1)
                    } else {
                        hops_left
                    };
                    self.write_members(
                        &found,
                        indent + 1,
                        depth_left - 1,
                        hops_left,
                        ancestors,
                        writer,
                        cx,
                    )
                    .await;
                    ancestors.pop();
                }
            }
            for enumerator in &summary.enumerators {
                if writer.full || writer.nodes_cut {
                    return;
                }
                let mut text = enumerator.name.clone();
                match (&enumerator.value_text, enumerator.value) {
                    (Some(value_text), _) => text.push_str(&format!(" = {value_text}")),
                    (None, Some(value)) => text.push_str(&format!("  (= {value})")),
                    (None, None) => {}
                }
                if let Some(condition) = &enumerator.condition {
                    text.push_str(&format!("  [#if {condition}]"));
                }
                if let Some(comment) = &enumerator.comment {
                    text.push_str(&format!("  // {comment}"));
                }
                writer.line(indent, &text);
            }
        }
        .boxed_local()
    }

    async fn expand(
        &mut self,
        owner: &TypeSummary,
        member: &MemberSummary,
        cx: &mut AsyncApp,
    ) -> Expansion {
        let Some(owner_ref) = owner.type_ref.clone() else {
            return Expansion::NotExpandable;
        };
        let selector = if member.index_path.is_empty() {
            match &member.name {
                Some(name) => MemberSelector::Name(name.clone()),
                None => return Expansion::NotExpandable,
            }
        } else {
            MemberSelector::Path(member.index_path.clone())
        };
        let now = cx.background_executor().now();
        if self.requests >= MAX_TYPE_LAYOUT_REQUESTS || now + WALK_TIME_MARGIN >= self.deadline {
            return Expansion::Stopped;
        }
        self.requests += 1;
        // A remote request may take longer than the margin; whatever was walked so far is
        // still returned instead of the whole answer timing out.
        let timer = cx.background_executor().timer(
            self.deadline
                .saturating_duration_since(now)
                .saturating_sub(WALK_TIME_MARGIN / 2),
        );
        let request = self.store.update(cx, |store, cx| {
            store.expand_member(owner_ref, selector, 1, cx)
        });
        let expansion = match future::select(pin!(request), pin!(timer)).await {
            Either::Left((Ok(expansion), _)) => expansion,
            Either::Right(_) => {
                log::warn!("type layout: expanding a member took too long");
                return Expansion::Stopped;
            }
            Either::Left((Err(error), _)) => {
                log::warn!("type layout: expanding a member failed: {error:#}");
                return Expansion::Stopped;
            }
        };
        let found = match expansion.target {
            MemberExpansionTarget::Types(types) => types.types.into_iter().next(),
            MemberExpansionTarget::Inline(inline) => Some(*inline),
            MemberExpansionTarget::NotFound => return Expansion::NotFound,
            MemberExpansionTarget::NotExpandable => return Expansion::NotExpandable,
        };
        match found {
            None => Expansion::NotFound,
            Some(found) => {
                if self.summary_shown(&found, cx).await {
                    Expansion::Type(found)
                } else {
                    Expansion::Withheld
                }
            }
        }
    }

    async fn macros_named(
        &mut self,
        name: &str,
        near: Option<ProjectPath>,
        cx: &mut AsyncApp,
    ) -> Result<TypeLayoutReport> {
        self.requests += 1;
        let definitions = self
            .store
            .update(cx, |store, cx| store.definitions(name, near, cx))
            .await?;
        let mut macros = Vec::new();
        let mut withheld = 0;
        let mut read = 0;
        let mut unread = 0;
        for definition in definitions {
            if !matches!(
                definition.kind,
                DefinitionKind::Macro | DefinitionKind::FunctionMacro
            ) {
                continue;
            }
            if !self.paths.allows(&definition.path, cx).await {
                withheld += 1;
                continue;
            }
            if read >= MAX_TYPE_READ_LIMIT {
                unread += 1;
                continue;
            }
            read += 1;
            self.requests += 1;
            let path = definition.path.clone();
            let position = definition.name_range.start;
            let hover = self
                .store
                .update(cx, |store, cx| {
                    store.describe_type_at(path, position, None, None, 1, cx)
                })
                .await?;
            if let Some(hover) = hover
                && let TypeHoverSubject::Macro { macros: found } = hover.subject
            {
                for found in found {
                    if found.path == definition.path
                        && !macros.iter().any(|known: &MacroSummary| {
                            known.path == found.path && known.range == found.range
                        })
                    {
                        macros.push(found);
                    }
                }
            }
        }
        let mut report = self.macro_report(name.to_string(), macros, cx).await;
        report.withheld += withheld;
        if report.kind == TypeLayoutKind::Macro && unread > 0 {
            report.candidates += unread;
            report.candidates_truncated = true;
        }
        Ok(report)
    }

    async fn macro_report(
        &mut self,
        requested: String,
        macros: Vec<MacroSummary>,
        cx: &mut AsyncApp,
    ) -> TypeLayoutReport {
        let mut shown = Vec::new();
        let mut withheld = 0;
        for found in macros {
            if self.paths.allows(&found.path, cx).await {
                shown.push(found);
            } else {
                withheld += 1;
            }
        }
        let Some(first) = shown.first() else {
            return TypeLayoutReport::not_found(requested, withheld);
        };
        let title = first.name.clone();
        let mut writer = TreeWriter::new(self.options.max_chars, usize::MAX);
        for found in &shown {
            writer.line(
                0,
                &format!(
                    "{}:{}",
                    found.path.path.as_unix_str(),
                    found.range.start.row + 1
                ),
            );
            for line in found.text.lines() {
                writer.line(1, line);
            }
            writer.truncated |= found.truncated;
            writer.add_definition(&found.path, found.range.start.row);
        }
        TypeLayoutReport {
            kind: TypeLayoutKind::Macro,
            requested,
            title,
            definition: writer.definitions.first().cloned(),
            candidates: shown.len(),
            candidates_truncated: false,
            other_candidates: Vec::new(),
            withheld,
            inferred: None,
            pruned: false,
            truncated: writer.truncated || writer.full,
            index_note: None,
            text: writer.text,
            definitions: writer.definitions,
        }
    }
}

struct TreeWriter {
    text: String,
    /// `text.chars().count()`.
    chars: usize,
    max_chars: usize,
    lines: usize,
    max_lines: usize,
    /// `max_chars` was reached; the text ends with [`TRUNCATED_MARKER`].
    full: bool,
    nodes_cut: bool,
    truncated: bool,
    definitions: Vec<TypeLayoutLocation>,
    withheld: usize,
    expanded: HashSet<DefinitionKey>,
}

impl TreeWriter {
    fn new(max_chars: usize, max_lines: usize) -> Self {
        Self {
            text: String::new(),
            chars: 0,
            max_chars,
            lines: 0,
            max_lines,
            full: false,
            nodes_cut: false,
            truncated: false,
            definitions: Vec::new(),
            withheld: 0,
            expanded: HashSet::default(),
        }
    }

    fn line(&mut self, indent: usize, text: &str) {
        if self.full {
            return;
        }
        if self.lines > self.max_lines {
            self.nodes_cut = true;
            return;
        }
        let line = format!("{}{text}\n", "  ".repeat(indent));
        let length = line.chars().count();
        if self.chars + length + TRUNCATED_MARKER.len() + 1 > self.max_chars {
            self.full = true;
            if self.chars + TRUNCATED_MARKER.len() < self.max_chars {
                self.text.push_str(TRUNCATED_MARKER);
                self.text.push('\n');
                self.chars += TRUNCATED_MARKER.len() + 1;
            }
            return;
        }
        self.text.push_str(&line);
        self.chars += length;
        self.lines += 1;
    }

    fn add_definition(&mut self, path: &ProjectPath, row: u32) {
        let location = TypeLayoutLocation {
            path: path.clone(),
            line: row + 1,
        };
        if !self.definitions.contains(&location) {
            self.definitions.push(location);
        }
    }
}

/// The index counts a C++ reference member as one step through a pointer.
fn is_reference(type_text: &str) -> bool {
    type_text.contains('&')
        && symbol_index::parse_type_text(type_text, symbol_index::SourceLanguage::Cpp)
            .is_some_and(|parsed| parsed.is_reference)
}

fn key_of(summary: &TypeSummary) -> DefinitionKey {
    (summary.path.clone(), summary.range.start.row)
}

fn location_of(summary: &TypeSummary) -> TypeLayoutLocation {
    TypeLayoutLocation {
        path: summary.path.clone(),
        line: summary.range.start.row + 1,
    }
}

/// `foo_t` or `struct foo` (also `union`, `enum`, `class`).
pub fn parse_type_name(text: &str) -> Result<(String, Option<TagKind>)> {
    let text = text.trim();
    if text.is_empty() {
        return Err(anyhow!("The type name is empty"));
    }
    if text.chars().count() > MAX_TYPE_NAME_CHARS {
        return Err(anyhow!(
            "The type name must be at most {MAX_TYPE_NAME_CHARS} characters"
        ));
    }
    let mut words = text.split_whitespace();
    let first = words.next().unwrap_or_default();
    let tag = match first {
        "struct" => Some(TagKind::Struct),
        "union" => Some(TagKind::Union),
        "enum" => Some(TagKind::Enum),
        "class" => Some(TagKind::Class),
        _ => None,
    };
    let name = if tag.is_some() {
        words.next()
    } else {
        Some(first)
    };
    let well_formed = |name: &str| {
        !name.is_empty()
            && !name.starts_with(|character: char| character.is_ascii_digit())
            && name.chars().all(|character| {
                character.is_alphanumeric() || character == '_' || character == ':'
            })
    };
    match name {
        Some(name) if words.next().is_none() && well_formed(name) => Ok((name.to_string(), tag)),
        _ => Err(anyhow!(
            "`{text}` isn't a type name. Give one name, such as `foo_t` or `struct foo`"
        )),
    }
}

#[cfg(test)]
#[path = "type_layout_tests.rs"]
mod tests;
