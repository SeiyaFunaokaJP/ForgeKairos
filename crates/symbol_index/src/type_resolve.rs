//! Looks type names up in the index and follows them: typedef chains, member access chains
//! (`a->b.c`), and the tree of a type's members for display.
//!
//! Everything here blocks on reading files through the [`FileSource`] and parsing the
//! definitions it finds, so it belongs on a background thread. A [`TypeResolver`] caches file
//! contents for its lifetime; create one per request (a hover, a tool call) so that edits are
//! seen by the next request.

use std::{cell::RefCell, sync::Arc};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    Definition, DefinitionKind, FileSource, SymbolIndex,
    extract::{Position, Range, SourceLanguage, language_for_path},
    type_layout::{
        ChainRoot, Confidence, Enumerator, Member, MemberAccess, TagKind, TypeAt, TypeKind,
        TypeLayout, TypeLocator, TypeText, VariableType, alias_for_name, declared_names_in_file,
        enclosing_condition, include_paths, parse_type_definition_bytes, tags_compatible,
        type_at_bytes,
    },
};

/// Candidates returned for one name.
pub const MAX_CANDIDATES: usize = 16;
/// Typedefs followed from one name (`a_t` -> `b_t` -> `struct b`).
pub const MAX_TYPEDEF_HOPS: usize = 8;
/// Lines of a macro definition returned, the first line included.
pub const MAX_MACRO_LINES: usize = 12;
const MAX_MACRO_LINE_CHARS: usize = 400;
/// Files larger than this are parsed only around the cursor by [`TypeResolver::describe_at`].
pub const WINDOWED_PARSE_BYTES: usize = 256 * 1024;
/// Files searched by name when the index has no type definition of the name.
const MAX_FALLBACK_FILES: usize = 4;
/// Hard limit on [`ExpandOptions::max_depth`].
pub const MAX_EXPAND_DEPTH: usize = 16;

/// How close a definition is to the file the request came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Proximity {
    SameFile,
    SameDirectory,
    /// A header whose path ends with an `#include` of the file.
    IncludedHeader,
    Other,
}

/// The file a request is made from, used to rank same-name definitions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NearFile {
    /// Index-relative, `/`-separated.
    pub path: String,
    pub includes: Vec<String>,
}

impl NearFile {
    pub fn new(path: &str, source: &[u8]) -> Self {
        Self {
            path: path.to_owned(),
            includes: include_paths(source),
        }
    }

    /// Without the file's contents only the same file and directory rank higher.
    pub fn path_only(path: &str) -> Self {
        Self {
            path: path.to_owned(),
            includes: Vec::new(),
        }
    }

    pub fn proximity(&self, candidate: &str) -> Proximity {
        if candidate == self.path {
            return Proximity::SameFile;
        }
        if directory_of(candidate) == directory_of(&self.path) {
            return Proximity::SameDirectory;
        }
        let included = self.includes.iter().any(|include| {
            let include = include
                .trim_start_matches("./")
                .trim_start_matches("../")
                .trim_start_matches('/');
            !include.is_empty()
                && (candidate == include
                    || candidate
                        .strip_suffix(include)
                        .is_some_and(|prefix| prefix.ends_with('/')))
        });
        if included {
            Proximity::IncludedHeader
        } else {
            Proximity::Other
        }
    }
}

fn directory_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(directory, _)| directory)
}

/// One typedef followed on the way to a definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypedefStep {
    pub name: String,
    pub path: Arc<str>,
    pub range: Range,
    /// What the typedef names: `struct foo *`.
    pub target: TypeText,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedType {
    pub layout: TypeLayout,
    /// Index-relative path of the file holding the definition.
    pub path: Arc<str>,
    pub language: SourceLanguage,
    /// Typedefs followed to get here, from the requested name.
    pub typedef_chain: Vec<TypedefStep>,
    /// Of the requested name's definition (the first typedef, if any).
    pub proximity: Proximity,
    /// The `#if` conditions the definition is under in its file, as written.
    pub condition: Option<String>,
}

impl ResolvedType {
    /// 0-based row of the definition's name (or its start).
    pub fn row(&self) -> u32 {
        self.layout
            .name_range
            .map_or(self.layout.range.start.row, |range| range.start.row)
    }

    fn key(&self) -> (Arc<str>, u32) {
        (self.path.clone(), self.layout.range.start.byte)
    }
}

/// A macro definition as text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MacroText {
    pub name: String,
    pub path: Arc<str>,
    pub name_range: Range,
    pub is_function_like: bool,
    /// The `#define` line and its continuation lines, up to [`MAX_MACRO_LINES`].
    pub text: String,
    pub truncated: bool,
}

/// Which type a member step was looked up in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainOwner {
    pub display_name: String,
    pub path: Option<Arc<str>>,
    pub range: Range,
    /// Same-name definitions there were to choose from; the first was used.
    pub candidates: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainStep {
    pub name: String,
    /// `None` when the type holding the member could not be found.
    pub owner: Option<ChainOwner>,
    /// `None` when the member is not in the owner (or the owner is unknown).
    pub member: Option<Member>,
}

/// Where the type of a member access chain's root was read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChainRootOrigin {
    /// The file asked about: a variable declared there, or a type written there (a cast).
    InFile,
    /// A global variable of [`MemberChain::root_name`], declared in some file of the index.
    Global,
    /// The return type of the function [`MemberChain::root_name`], from its definitions or
    /// prototypes in the index.
    Call,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberChain {
    /// The type of the chain's root, when it could be found.
    pub root_type: Option<TypeText>,
    pub root_origin: ChainRootOrigin,
    /// The variable or function the root names; `None` for a type written in place.
    pub root_name: Option<String>,
    /// One step per member; the steps after the first failure are absent.
    pub steps: Vec<ChainStep>,
    /// Every member was found.
    pub complete: bool,
}

impl MemberChain {
    /// The member at the end of the chain, when the whole chain was followed.
    pub fn final_member(&self) -> Option<&Member> {
        if !self.complete {
            return None;
        }
        self.steps.last()?.member.as_ref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HoverSubject {
    TypeName { name: String, tag: Option<TagKind> },
    Variable(VariableType),
    Member(MemberChain),
    Macro(Vec<MacroText>),
}

/// What [`TypeResolver::describe_at`] found at a position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeHover {
    pub name: String,
    pub range: Range,
    pub subject: HoverSubject,
    /// The definitions of the subject's type, best first; empty for builtin and function types,
    /// macros, and names that were not found. A member whose type is defined in place carries it
    /// in [`Member::nested`] instead.
    pub types: Vec<ResolvedType>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExpandOptions {
    /// Levels of named types expanded below the root.
    pub max_depth: usize,
    /// Pointers followed on the way down; members reached through more are not expanded.
    pub max_pointer_hops: usize,
    /// Members and enumerators in the whole tree.
    pub max_nodes: usize,
}

impl Default for ExpandOptions {
    fn default() -> Self {
        Self {
            max_depth: 3,
            max_pointer_hops: 1,
            max_nodes: 400,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeState {
    Expanded,
    /// The type contains itself through this member (`struct list_head *next`).
    Recursive,
    /// Expanded earlier in the tree.
    SeeAbove,
    /// No definition of the name was found.
    NotFound,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeNode {
    pub title: String,
    /// `None` for a body defined in place inside its parent.
    pub path: Option<Arc<str>>,
    pub range: Option<Range>,
    pub typedef_chain: Vec<TypedefStep>,
    /// For a typedef of a builtin, pointer or function type: what it names.
    pub alias: Option<TypeText>,
    /// Same-name definitions there were to choose from; the first was used.
    pub candidates: usize,
    pub condition: Option<String>,
    pub state: NodeState,
    pub members: Vec<MemberNode>,
    pub enumerators: Vec<Enumerator>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberNode {
    pub member: Member,
    pub child: Option<TypeNode>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeTree {
    pub root: TypeNode,
    /// The node limit cut the tree short.
    pub truncated: bool,
}

/// What expanding one member leads to, for a UI that expands one level at a time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemberTarget<'a> {
    /// The member's type is defined in place.
    Inline(&'a TypeLayout),
    Resolved(Vec<ResolvedType>),
    /// A builtin, function pointer or unparsed member.
    NotExpandable,
}

/// Resolves type names against an index, reading definitions through `files`.
pub struct TypeResolver<'a> {
    index: &'a SymbolIndex,
    files: &'a dyn FileSource,
    path_filter: Option<&'a dyn Fn(&str) -> bool>,
    candidate_limit: usize,
    contents: RefCell<FxHashMap<Arc<str>, Option<Arc<[u8]>>>>,
}

/// Definitions of `name` that are types, best first. See [`TypeResolver::resolve_type`].
pub fn resolve_type(
    index: &SymbolIndex,
    files: &dyn FileSource,
    name: &str,
    tag: Option<TagKind>,
    near: &NearFile,
) -> Vec<ResolvedType> {
    TypeResolver::new(index, files).resolve_type(name, tag, near)
}

fn is_type_kind(kind: DefinitionKind) -> bool {
    matches!(
        kind,
        DefinitionKind::Struct
            | DefinitionKind::Union
            | DefinitionKind::Enum
            | DefinitionKind::Class
            | DefinitionKind::Typedef
    )
}

fn declares(layout: &TypeLayout, name: &str) -> bool {
    layout.tag_name.as_deref() == Some(name)
        || layout
            .typedef_names
            .iter()
            .any(|typedef| typedef.name == name)
}

fn definition_type_kind(kind: DefinitionKind) -> Option<TypeKind> {
    match kind {
        DefinitionKind::Struct => Some(TypeKind::Struct),
        DefinitionKind::Union => Some(TypeKind::Union),
        DefinitionKind::Enum => Some(TypeKind::Enum),
        DefinitionKind::Class => Some(TypeKind::Class),
        DefinitionKind::Typedef => Some(TypeKind::Typedef),
        _ => None,
    }
}

impl<'a> TypeResolver<'a> {
    pub fn new(index: &'a SymbolIndex, files: &'a dyn FileSource) -> Self {
        Self {
            index,
            files,
            path_filter: None,
            candidate_limit: MAX_CANDIDATES,
            contents: RefCell::new(FxHashMap::default()),
        }
    }

    /// Makes [`Self::resolve_type`] (and so [`Self::describe_at`] and
    /// [`Self::member_target`]) read definitions only until `limit` of them have a body, for a
    /// caller that shows the rest by location only (see [`Self::type_candidates`]).
    pub fn with_candidate_limit(mut self, limit: usize) -> Self {
        self.candidate_limit = limit.clamp(1, MAX_CANDIDATES);
        self
    }

    /// The file `path` as a starting point for ranking: its directory and `#include`s.
    pub fn near(&self, path: &str) -> NearFile {
        self.near_file(&Arc::from(path))
    }

    /// Leaves out every definition in a file for which `filter` (given the index-relative
    /// path) returns false, as if the index did not have it: such a file is neither read nor
    /// counted among the candidates.
    pub fn with_path_filter(mut self, filter: &'a dyn Fn(&str) -> bool) -> Self {
        self.path_filter = Some(filter);
        self
    }

    fn path_allowed(&self, path: &str) -> bool {
        self.path_filter.is_none_or(|filter| filter(path))
    }

    fn definitions_named(&self, name: &str) -> Vec<Definition> {
        let mut definitions = self.index.definitions(name);
        definitions.retain(|definition| self.path_allowed(&definition.path));
        definitions
    }

    fn read(&self, path: &Arc<str>) -> Option<Arc<[u8]>> {
        if !self.path_allowed(path) {
            return None;
        }
        if let Some(cached) = self.contents.borrow().get(path) {
            return cached.clone();
        }
        let contents = match self.files.read(path) {
            Ok(contents) => Some(Arc::<[u8]>::from(contents)),
            Err(error) => {
                log::debug!("symbol_index: cannot read {path} for a type lookup: {error}");
                None
            }
        };
        self.contents
            .borrow_mut()
            .insert(path.clone(), contents.clone());
        contents
    }

    fn near_file(&self, path: &Arc<str>) -> NearFile {
        match self.read(path) {
            Some(contents) => NearFile::new(path, &contents),
            None => NearFile::path_only(path),
        }
    }

    /// Definitions of the type `name` (`foo` of `struct foo` when `tag` is given, or a typedef
    /// or C++ class name), best first: definitions with a body before forward declarations and
    /// typedefs whose target was not found, then by [`Proximity`] to `near`. Typedefs are
    /// followed to the struct, union or enum they name, and the steps are recorded.
    pub fn resolve_type(
        &self,
        name: &str,
        tag: Option<TagKind>,
        near: &NearFile,
    ) -> Vec<ResolvedType> {
        self.resolve_limited(name, tag, near, self.candidate_limit)
    }

    /// The best definition of the type, reading only as many candidates as it takes to find
    /// one with a body, and how many same-name definitions the index has. Expanding a tree
    /// uses this, since reading every same-name candidate for every member is slow.
    pub fn resolve_best(
        &self,
        name: &str,
        tag: Option<TagKind>,
        near: &NearFile,
    ) -> (Option<ResolvedType>, usize) {
        let candidates = self.candidate_definitions(name, tag, near).len();
        let best = self.resolve_limited(name, tag, near, 1).into_iter().next();
        let candidates = candidates.max(usize::from(best.is_some()));
        (best, candidates)
    }

    /// [`Self::resolve_type`] reading definitions only until `limit` of them have a body (and
    /// returning at most `limit`); see [`Self::type_candidates`] for the rest.
    pub fn resolve_type_limited(
        &self,
        name: &str,
        tag: Option<TagKind>,
        near: &NearFile,
        limit: usize,
    ) -> Vec<ResolvedType> {
        self.resolve_limited(name, tag, near, limit.clamp(1, MAX_CANDIDATES))
    }

    /// The index's definitions of the type `name` in the order [`Self::resolve_type`] tries
    /// them, without reading any file. Typedefs are not followed.
    pub fn type_candidates(
        &self,
        name: &str,
        tag: Option<TagKind>,
        near: &NearFile,
    ) -> Vec<Definition> {
        self.candidate_definitions(name, tag, near)
    }

    /// The type defined at `row` and `column` (0-based; the column counts bytes) of `path`,
    /// found again by its position rather than through the index, whose ranges go stale.
    /// `name` is the tag or typedef name it was found under, and must still be declared
    /// there. A typedef is followed to what it names, as [`Self::resolve_type`] does.
    pub fn locate_type(
        &self,
        path: &str,
        row: u32,
        column: u32,
        name: &str,
    ) -> Option<ResolvedType> {
        let path: Arc<str> = Arc::from(path);
        let contents = self.read(&path)?;
        let language = language_for_path(&path)?.resolve(&contents);
        let byte = byte_of_row_and_column(&contents, row, column)?;
        let start = Position {
            byte: u32::try_from(byte).ok()?,
            row,
            column,
        };
        let layout = parse_type_definition_bytes(
            &contents,
            TypeLocator::Range(Range { start, end: start }),
            language,
        )
        .filter(|layout| declares(layout, name))
        .or_else(|| {
            parse_type_definition_bytes(&contents, TypeLocator::Name { name, tag: None }, language)
        })?;
        let near = NearFile::new(&path, &contents);
        let mut visited = FxHashSet::default();
        let mut results = Vec::new();
        self.accept(
            name,
            layout,
            &path,
            language,
            &contents,
            &near,
            0,
            1,
            &mut visited,
            &mut results,
        );
        results.into_iter().next()
    }

    fn resolve_limited(
        &self,
        name: &str,
        tag: Option<TagKind>,
        near: &NearFile,
        limit: usize,
    ) -> Vec<ResolvedType> {
        let mut visited = FxHashSet::default();
        let mut resolved = self.resolve_with(name, tag, near, 0, limit, &mut visited);
        resolved.sort_by_key(|candidate| (!candidate.layout.has_body(), candidate.proximity));
        resolved.truncate(limit);
        resolved
    }

    fn candidate_definitions(
        &self,
        name: &str,
        tag: Option<TagKind>,
        near: &NearFile,
    ) -> Vec<Definition> {
        let mut definitions: Vec<Definition> = self
            .definitions_named(name)
            .into_iter()
            .filter(|definition| is_type_kind(definition.kind))
            .filter(
                |definition| match (tag, definition_type_kind(definition.kind)) {
                    (Some(tag), Some(kind)) => tags_compatible(tag, kind, definition.language),
                    (None, _) => true,
                    (Some(_), None) => false,
                },
            )
            .collect();
        definitions.sort_by(|left, right| {
            let rank = |definition: &Definition| {
                (
                    near.proximity(&definition.path),
                    // A bare name in C is a typedef; in C++ it may also be a class.
                    tag.is_none() && definition.kind != DefinitionKind::Typedef,
                )
            };
            rank(left).cmp(&rank(right))
        });
        definitions
    }

    /// Files where the index saw `name` defined, as anything. When a file's parse went wrong
    /// the index can record a typedef as a variable, or with the range of something else;
    /// searching those files by name still finds the definition.
    fn fallback_files(&self, name: &str, near: &NearFile) -> Vec<(Arc<str>, SourceLanguage)> {
        let mut files: Vec<(Arc<str>, SourceLanguage)> = Vec::new();
        // The requesting file itself, for a type the index did not record at all (a member's
        // type is usually defined next to its owner).
        if let Some(guess) = language_for_path(&near.path) {
            let path: Arc<str> = Arc::from(near.path.as_str());
            if let Some(contents) = self.read(&path) {
                files.push((path, guess.resolve(&contents)));
            }
        }
        for definition in self.definitions_named(name) {
            if !files.iter().any(|(path, _)| *path == definition.path) {
                files.push((definition.path.clone(), definition.language));
            }
        }
        files.sort_by_key(|(path, _)| near.proximity(path));
        files.truncate(MAX_FALLBACK_FILES);
        files
    }

    fn resolve_with(
        &self,
        name: &str,
        tag: Option<TagKind>,
        near: &NearFile,
        hops: usize,
        limit: usize,
        visited: &mut FxHashSet<(Arc<str>, u32)>,
    ) -> Vec<ResolvedType> {
        if hops > MAX_TYPEDEF_HOPS {
            return Vec::new();
        }
        let mut results = Vec::new();
        let enough = |results: &Vec<ResolvedType>| {
            results.len() >= MAX_CANDIDATES
                || results
                    .iter()
                    .filter(|candidate| candidate.layout.has_body())
                    .count()
                    >= limit
        };
        for definition in self.candidate_definitions(name, tag, near) {
            if enough(&results) {
                break;
            }
            let Some(contents) = self.read(&definition.path) else {
                continue;
            };
            let Some(layout) = parse_type_definition_bytes(
                &contents,
                TypeLocator::Range(definition.item_range),
                definition.language,
            ) else {
                continue;
            };
            // A range that no longer holds this name (the file changed since it was indexed, or
            // the index misread it) is left to the search by name below.
            if !declares(&layout, name) {
                continue;
            }
            self.accept(
                name,
                layout,
                &definition.path,
                definition.language,
                &contents,
                near,
                hops,
                limit,
                visited,
                &mut results,
            );
        }
        if results.is_empty() {
            for (path, language) in self.fallback_files(name, near) {
                if enough(&results) {
                    break;
                }
                let Some(contents) = self.read(&path) else {
                    continue;
                };
                let Some(layout) = parse_type_definition_bytes(
                    &contents,
                    TypeLocator::Name { name, tag },
                    language,
                ) else {
                    continue;
                };
                self.accept(
                    name,
                    layout,
                    &path,
                    language,
                    &contents,
                    near,
                    hops,
                    limit,
                    visited,
                    &mut results,
                );
            }
        }
        results
    }

    /// Adds a parsed definition to `results`, following a typedef to what it names.
    fn accept(
        &self,
        name: &str,
        mut layout: TypeLayout,
        path: &Arc<str>,
        language: SourceLanguage,
        contents: &[u8],
        near: &NearFile,
        hops: usize,
        limit: usize,
        visited: &mut FxHashSet<(Arc<str>, u32)>,
        results: &mut Vec<ResolvedType>,
    ) {
        if !visited.insert((path.clone(), layout.range.start.byte)) {
            return;
        }
        alias_for_name(contents, &mut layout, name);
        let proximity = near.proximity(path);
        let condition = enclosing_condition(contents, layout.range.start.byte as usize);
        let alias = layout
            .alias
            .clone()
            .filter(|alias| layout.kind == TypeKind::Typedef && alias.is_expandable());
        if let Some(alias) = alias
            && let Some(target_name) = alias.core_name.clone()
        {
            let step = TypedefStep {
                name: name.to_owned(),
                path: path.clone(),
                range: layout.name_range.unwrap_or(layout.range),
                target: alias.clone(),
            };
            let typedef_near = NearFile::new(path, contents);
            let targets = self.resolve_with(
                &target_name,
                alias.tag,
                &typedef_near,
                hops + 1,
                limit,
                visited,
            );
            if !targets.is_empty() {
                for mut target in targets {
                    target.typedef_chain.insert(0, step.clone());
                    target.proximity = proximity;
                    results.push(target);
                }
                return;
            }
        }
        results.push(ResolvedType {
            layout,
            path: path.clone(),
            language,
            typedef_chain: Vec::new(),
            proximity,
            condition,
        });
    }

    /// The definitions of a member's type, for expanding it by one level.
    pub fn member_target<'m>(&self, member: &'m Member, owner_path: &Arc<str>) -> MemberTarget<'m> {
        if let Some(nested) = &member.nested {
            return MemberTarget::Inline(nested);
        }
        if member.kind == crate::type_layout::MemberKind::Unparsed || !member.ty.is_expandable() {
            return MemberTarget::NotExpandable;
        }
        let Some(name) = member.ty.core_name.as_deref() else {
            return MemberTarget::NotExpandable;
        };
        let near = self.near_file(owner_path);
        MemberTarget::Resolved(self.resolve_type(name, member.ty.tag, &near))
    }

    /// The type a function returns, from its definition or prototype.
    pub fn function_return_type(&self, name: &str, near: &NearFile) -> Option<TypeText> {
        let mut definitions: Vec<Definition> = self
            .definitions_named(name)
            .into_iter()
            .filter(|definition| definition.kind.is_function_like())
            .collect();
        definitions.sort_by_key(|definition| near.proximity(&definition.path));
        definitions.into_iter().find_map(|definition| {
            let contents = self.read(&definition.path)?;
            declared_names_in_file(
                &contents,
                definition.item_range.start,
                definition.item_range.end.byte as usize,
                definition.language,
            )
            .into_iter()
            .find(|declared| declared.name == name)
            .and_then(|declared| declared.return_type)
        })
    }

    /// A global variable of this name declared in some file.
    pub fn global_variable(&self, name: &str, near: &NearFile) -> Option<VariableType> {
        let mut definitions: Vec<Definition> = self
            .definitions_named(name)
            .into_iter()
            .filter(|definition| definition.kind == DefinitionKind::Variable)
            .collect();
        definitions.sort_by_key(|definition| {
            (near.proximity(&definition.path), definition.is_declaration)
        });
        definitions.into_iter().find_map(|definition| {
            let contents = self.read(&definition.path)?;
            let declared = declared_names_in_file(
                &contents,
                definition.item_range.start,
                definition.item_range.end.byte as usize,
                definition.language,
            )
            .into_iter()
            .find(|declared| declared.name == name && !declared.ty.is_function)?;
            Some(VariableType {
                name: name.to_owned(),
                ty: declared.ty,
                declaration: Some(declared.name_range),
                confidence: Confidence::NameOnly,
            })
        })
    }

    /// Macro definitions of `name`, as text.
    pub fn macro_definitions(&self, name: &str) -> Vec<MacroText> {
        self.definitions_named(name)
            .into_iter()
            .filter(|definition| definition.kind.is_macro())
            .filter_map(|definition| {
                let contents = self.read(&definition.path)?;
                let (text, truncated) =
                    macro_lines(&contents, definition.item_range.start.byte as usize);
                Some(MacroText {
                    name: definition.name.clone(),
                    path: definition.path.clone(),
                    name_range: definition.name_range,
                    is_function_like: definition.kind == DefinitionKind::FunctionMacro,
                    text,
                    truncated,
                })
            })
            .take(MAX_CANDIDATES)
            .collect()
    }

    /// Follows `a->b.c` member by member. The chain stops at the first member that cannot be
    /// found, or whose holding type cannot be.
    pub fn member_chain_type(&self, access: &MemberAccess, near: &NearFile) -> MemberChain {
        let root_type = match &access.root {
            ChainRoot::Variable(variable) => Some(variable.ty.clone()),
            ChainRoot::Type(ty) => Some(ty.clone()),
            ChainRoot::Call { function, .. } => self.function_return_type(function, near),
            ChainRoot::Unknown { name, .. } => {
                self.global_variable(name, near).map(|variable| variable.ty)
            }
        };
        let (root_origin, root_name) = match &access.root {
            ChainRoot::Variable(variable) => (ChainRootOrigin::InFile, Some(variable.name.clone())),
            ChainRoot::Type(_) => (ChainRootOrigin::InFile, None),
            ChainRoot::Call { function, .. } => (ChainRootOrigin::Call, Some(function.clone())),
            ChainRoot::Unknown { name, .. } => (ChainRootOrigin::Global, Some(name.clone())),
        };
        let mut chain = MemberChain {
            root_type: root_type.clone(),
            root_origin,
            root_name,
            steps: Vec::new(),
            complete: false,
        };
        enum Current {
            Text(TypeText, NearFile),
            Inline(TypeLayout, Option<Arc<str>>),
        }
        let Some(root_type) = root_type else {
            if let Some(first) = access.members.first() {
                chain.steps.push(ChainStep {
                    name: first.name.clone(),
                    owner: None,
                    member: None,
                });
            }
            return chain;
        };
        let mut current = Current::Text(root_type, near.clone());
        for step in &access.members {
            let (layout, path, candidates) = match current {
                Current::Inline(layout, path) => (layout, path, 1),
                Current::Text(ty, near) => {
                    let (best, candidates) = match ty.core_name.as_deref() {
                        Some(name) if ty.is_expandable() => self.resolve_best(name, ty.tag, &near),
                        _ => (None, 0),
                    };
                    match best.filter(|found| found.layout.has_body()) {
                        Some(found) => (found.layout, Some(found.path), candidates),
                        None => {
                            chain.steps.push(ChainStep {
                                name: step.name.clone(),
                                owner: None,
                                member: None,
                            });
                            return chain;
                        }
                    }
                }
            };
            let owner = ChainOwner {
                display_name: layout.display_name(),
                path: path.clone(),
                range: layout.range,
                candidates,
            };
            let member = layout.find_member(&step.name).cloned();
            chain.steps.push(ChainStep {
                name: step.name.clone(),
                owner: Some(owner),
                member: member.clone(),
            });
            let Some(member) = member else {
                return chain;
            };
            current = match member.nested {
                Some(nested) => Current::Inline(*nested, path),
                None => {
                    let near = match &path {
                        Some(path) => self.near_file(path),
                        None => near.clone(),
                    };
                    Current::Text(member.ty, near)
                }
            };
        }
        chain.complete = true;
        chain
    }

    /// Describes the type of what is at `byte_offset` in `source` (the contents of `path`):
    /// the cursor's name is read from the file, then looked up through the index.
    pub fn describe_at(
        &self,
        path: &str,
        source: &[u8],
        byte_offset: usize,
        language: SourceLanguage,
    ) -> Option<TypeHover> {
        let near = NearFile::new(path, source);
        self.contents
            .borrow_mut()
            .entry(Arc::from(path))
            .or_insert_with(|| Some(Arc::from(source)));
        self.describe_with(path, source, byte_offset, language, near, true)
    }

    /// [`Self::describe_at`] given only `window`, a part of `path` (the function around the
    /// cursor, say) that holds `byte_offset`, as a client of a remote host sends it rather than
    /// the whole file. Positions in the result are within `window`, except the definitions
    /// found elsewhere. The file itself is read from the [`FileSource`] for what lies outside
    /// the window: its `#include`s and the types and globals defined at file scope.
    pub fn describe_in_window(
        &self,
        path: &str,
        window: &[u8],
        byte_offset: usize,
        language: SourceLanguage,
    ) -> Option<TypeHover> {
        let near = self.near_file(&Arc::from(path));
        self.describe_with(path, window, byte_offset, language, near, false)
    }

    fn describe_with(
        &self,
        path: &str,
        source: &[u8],
        byte_offset: usize,
        language: SourceLanguage,
        near: NearFile,
        source_is_the_file: bool,
    ) -> Option<TypeHover> {
        let types_of = |ty: &TypeText| match ty.core_name.as_deref() {
            Some(name) if ty.is_expandable() => self.resolve_type(name, ty.tag, &near),
            _ => Vec::new(),
        };
        // Parsing a large file whole takes longer than a hover can wait; the function around the
        // cursor holds what matters, and globals are found through the index.
        let window = if source_is_the_file && source.len() > WINDOWED_PARSE_BYTES {
            let byte = u32::try_from(byte_offset).unwrap_or(u32::MAX);
            self.index
                .enclosing_definition(path, byte)
                .map(|definition| definition.item_range)
                .or_else(|| {
                    // Outside functions (a struct or a global in a large header), the whole
                    // declaration around the cursor is enough to tell what the name is.
                    self.index
                        .file_symbols(path)?
                        .definitions
                        .into_iter()
                        .map(|definition| definition.item_range)
                        .filter(|range| range.start.byte <= byte && byte <= range.end.byte)
                        .max_by_key(|range| range.end.byte.saturating_sub(range.start.byte))
                })
                .map(|range| range.start.byte as usize..range.end.byte as usize)
        } else {
            None
        };
        match type_at_bytes(source, byte_offset, language, window)? {
            TypeAt::TypeName { name, tag, range } => Some(TypeHover {
                types: self.resolve_type(&name, tag, &near),
                name: name.clone(),
                range,
                subject: HoverSubject::TypeName { name, tag },
            }),
            TypeAt::Variable { variable, range } => Some(TypeHover {
                name: variable.name.clone(),
                types: types_of(&variable.ty),
                range,
                subject: HoverSubject::Variable(variable),
            }),
            TypeAt::MemberAccess { access, range } => {
                let chain = self.member_chain_type(&access, &near);
                let types = chain
                    .final_member()
                    .filter(|member| member.nested.is_none())
                    .map(|member| types_of(&member.ty))
                    .unwrap_or_default();
                Some(TypeHover {
                    name: access
                        .members
                        .last()
                        .map(|step| step.name.clone())
                        .unwrap_or_default(),
                    range,
                    subject: HoverSubject::Member(chain),
                    types,
                })
            }
            TypeAt::Identifier { name, range } => {
                let macros = self.macro_definitions(&name);
                if !macros.is_empty() {
                    return Some(TypeHover {
                        name,
                        range,
                        subject: HoverSubject::Macro(macros),
                        types: Vec::new(),
                    });
                }
                if let Some(variable) = self.global_variable(&name, &near) {
                    return Some(TypeHover {
                        name,
                        range,
                        types: types_of(&variable.ty),
                        subject: HoverSubject::Variable(variable),
                    });
                }
                let types = self.resolve_type(&name, None, &near);
                if types.is_empty() {
                    return None;
                }
                Some(TypeHover {
                    name: name.clone(),
                    range,
                    subject: HoverSubject::TypeName { name, tag: None },
                    types,
                })
            }
        }
    }

    /// Expands a type's members down to `options.max_depth` levels of named types. Types
    /// reached through pointers are expanded only within `options.max_pointer_hops`; a type
    /// already expanded higher up is marked [`NodeState::SeeAbove`], and one that contains
    /// itself [`NodeState::Recursive`].
    pub fn expand(&self, root: &ResolvedType, options: ExpandOptions) -> TypeTree {
        let options = ExpandOptions {
            max_depth: options.max_depth.min(MAX_EXPAND_DEPTH),
            ..options
        };
        let mut walk = Walk {
            resolver: self,
            options,
            ancestors: Vec::new(),
            seen: FxHashSet::default(),
            budget: options.max_nodes,
            truncated: false,
            resolved: FxHashMap::default(),
        };
        let mut root_node = walk.layout_node(&root.layout, Some(root.path.clone()), 0, 0);
        root_node.typedef_chain = root.typedef_chain.clone();
        root_node.condition = root.condition.clone();
        TypeTree {
            root: root_node,
            truncated: walk.truncated,
        }
    }
}

struct Walk<'r, 'a> {
    resolver: &'r TypeResolver<'a>,
    options: ExpandOptions,
    ancestors: Vec<(Arc<str>, u32)>,
    seen: FxHashSet<(Arc<str>, u32)>,
    budget: usize,
    truncated: bool,
    resolved: FxHashMap<(Arc<str>, String, Option<TagKind>), (Option<ResolvedType>, usize)>,
}

impl Walk<'_, '_> {
    fn layout_node(
        &mut self,
        layout: &TypeLayout,
        path: Option<Arc<str>>,
        depth: usize,
        pointer_hops: usize,
    ) -> TypeNode {
        let key = path
            .as_ref()
            .map(|path| (path.clone(), layout.range.start.byte));
        if let Some(key) = &key {
            self.ancestors.push(key.clone());
            self.seen.insert(key.clone());
        }
        let mut node = TypeNode {
            title: layout.display_name(),
            path: path.clone(),
            range: path
                .as_ref()
                .map(|_| layout.name_range.unwrap_or(layout.range)),
            typedef_chain: Vec::new(),
            alias: layout.alias.clone(),
            candidates: 1,
            condition: None,
            state: NodeState::Expanded,
            members: Vec::new(),
            enumerators: Vec::new(),
        };
        for member in &layout.members {
            if self.budget == 0 {
                self.truncated = true;
                break;
            }
            self.budget -= 1;
            let child = self.member_child(member, path.as_ref(), depth, pointer_hops);
            node.members.push(MemberNode {
                member: member.clone(),
                child,
            });
        }
        for enumerator in &layout.enumerators {
            if self.budget == 0 {
                self.truncated = true;
                break;
            }
            self.budget -= 1;
            node.enumerators.push(enumerator.clone());
        }
        if key.is_some() {
            self.ancestors.pop();
        }
        node
    }

    fn member_child(
        &mut self,
        member: &Member,
        owner_path: Option<&Arc<str>>,
        depth: usize,
        pointer_hops: usize,
    ) -> Option<TypeNode> {
        if let Some(nested) = &member.nested {
            // A body defined in place belongs to this type; it is always shown.
            let mut node = self.layout_node(nested, None, depth, pointer_hops);
            node.path = None;
            node.range = None;
            return Some(node);
        }
        if member.kind == crate::type_layout::MemberKind::Unparsed || !member.ty.is_expandable() {
            return None;
        }
        let name = member.ty.core_name.clone()?;
        let through_pointer = member.ty.pointer_depth > 0 || member.ty.is_reference;
        let hops = pointer_hops + usize::from(through_pointer);
        if hops > self.options.max_pointer_hops || depth + 1 > self.options.max_depth {
            return None;
        }
        let owner_path = owner_path.cloned()?;
        let cache_key = (owner_path.clone(), name.clone(), member.ty.tag);
        let (best, candidates) = match self.resolved.get(&cache_key) {
            Some(resolved) => resolved.clone(),
            None => {
                let near = self.resolver.near_file(&owner_path);
                let resolved = self.resolver.resolve_best(&name, member.ty.tag, &near);
                self.resolved.insert(cache_key, resolved.clone());
                resolved
            }
        };
        let Some(first) = best else {
            return Some(TypeNode {
                title: member
                    .ty
                    .tag
                    .map_or_else(|| name.clone(), |tag| format!("{} {name}", tag.keyword())),
                path: None,
                range: None,
                typedef_chain: Vec::new(),
                alias: None,
                candidates: 0,
                condition: None,
                state: NodeState::NotFound,
                members: Vec::new(),
                enumerators: Vec::new(),
            });
        };
        let key = first.key();
        let marker = |state| TypeNode {
            title: first.layout.display_name(),
            path: Some(first.path.clone()),
            range: Some(first.layout.name_range.unwrap_or(first.layout.range)),
            typedef_chain: first.typedef_chain.clone(),
            alias: first.layout.alias.clone(),
            candidates,
            condition: first.condition.clone(),
            state,
            members: Vec::new(),
            enumerators: Vec::new(),
        };
        if self.ancestors.contains(&key) {
            return Some(marker(NodeState::Recursive));
        }
        if !first.layout.has_body() {
            // A typedef of a builtin or pointer type: shown on the member's line.
            return Some(marker(NodeState::Expanded));
        }
        if self.seen.contains(&key) {
            return Some(marker(NodeState::SeeAbove));
        }
        let mut node = self.layout_node(&first.layout, Some(first.path.clone()), depth + 1, hops);
        node.typedef_chain = first.typedef_chain;
        node.candidates = candidates;
        node.condition = first.condition;
        Some(node)
    }
}

/// The byte of `row` and `column` in `source`, if the row exists and is at least that long.
fn byte_of_row_and_column(source: &[u8], row: u32, column: u32) -> Option<usize> {
    let mut line_start = 0usize;
    for _ in 0..row {
        let newline = source.get(line_start..)?.iter().position(|&c| c == b'\n')?;
        line_start += newline + 1;
    }
    let byte = line_start.checked_add(usize::try_from(column).ok()?)?;
    let line_end = source
        .get(line_start..)?
        .iter()
        .position(|&c| c == b'\n')
        .map_or(source.len(), |offset| line_start + offset);
    (byte <= line_end).then_some(byte)
}

/// The language of `path` by its extension, a `.h` header decided by `contents`.
pub fn source_language(path: &str, contents: &[u8]) -> Option<SourceLanguage> {
    Some(language_for_path(path)?.resolve(contents))
}

/// The `#define` at `start` and its continuation lines.
fn macro_lines(source: &[u8], start: usize) -> (String, bool) {
    let mut lines = Vec::new();
    let mut line_start = start.min(source.len());
    // The index records a macro from its `#`; include any indentation before it on the line.
    loop {
        let line_end = source
            .get(line_start..)
            .and_then(|rest| rest.iter().position(|&c| c == b'\n'))
            .map_or(source.len(), |offset| line_start + offset);
        let line = source.get(line_start..line_end).unwrap_or_default();
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let continues = line.trim_ascii_end().ends_with(b"\\");
        let text = String::from_utf8_lossy(line);
        let mut text: String = text.trim_end().chars().take(MAX_MACRO_LINE_CHARS).collect();
        if line.len() > MAX_MACRO_LINE_CHARS && text.chars().count() == MAX_MACRO_LINE_CHARS {
            text.push('…');
        }
        lines.push(text);
        if !continues || line_end >= source.len() {
            return (lines.join("\n"), false);
        }
        if lines.len() >= MAX_MACRO_LINES {
            return (lines.join("\n"), true);
        }
        line_start = line_end + 1;
    }
}

/// Renders a tree as indented text for tools and logs, cut at `max_chars` characters:
///
/// ```text
/// struct file  (include/linux/fs.h:980)
///   struct path f_path  (include/linux/path.h:8)
///     struct vfsmount *mnt
///   const struct file_operations *f_op  (see above)
///   unsigned int f_flags : 3  [#if CONFIG_X]  // comment
/// ```
pub fn render_text(tree: &TypeTree, max_chars: usize) -> String {
    let mut renderer = Renderer {
        output: String::new(),
        output_chars: 0,
        max_chars,
        full: false,
    };
    let root = &tree.root;
    let mut header = String::new();
    for step in &root.typedef_chain {
        header.push_str(&step.name);
        header.push_str(" -> ");
    }
    header.push_str(&root.title);
    if let Some(path) = &root.path {
        let row = root.range.map_or(0, |range| range.start.row);
        header.push_str(&format!("  ({path}:{})", row + 1));
    }
    if let Some(condition) = &root.condition {
        header.push_str(&format!("  [#if {condition}]"));
    }
    renderer.line(0, &header);
    if root.members.is_empty()
        && root.enumerators.is_empty()
        && let Some(alias) = &root.alias
    {
        renderer.line(1, &format!("= {}", alias.text));
    }
    renderer.children(root, 1);
    if tree.truncated {
        renderer.line(1, "... (more members not shown)");
    }
    renderer.output
}

struct Renderer {
    output: String,
    /// `output.chars().count()`, kept so that each line does not count everything again.
    output_chars: usize,
    max_chars: usize,
    full: bool,
}

/// The line [`render_text`] ends with when `max_chars` cut the text short.
pub const RENDER_TRUNCATED_MARKER: &str = "... (truncated)";
const TRUNCATED_MARKER: &str = RENDER_TRUNCATED_MARKER;

impl Renderer {
    fn line(&mut self, indent: usize, text: &str) {
        if self.full {
            return;
        }
        let line = format!("{}{text}\n", "  ".repeat(indent));
        let used = self.output_chars;
        let length = line.chars().count();
        if used + length + TRUNCATED_MARKER.len() + 1 > self.max_chars {
            self.full = true;
            if used + TRUNCATED_MARKER.len() < self.max_chars {
                self.output.push_str(TRUNCATED_MARKER);
                self.output.push('\n');
                self.output_chars += TRUNCATED_MARKER.len() + 1;
            }
            return;
        }
        self.output.push_str(&line);
        self.output_chars += length;
    }

    fn children(&mut self, node: &TypeNode, indent: usize) {
        for member_node in &node.members {
            let member = &member_node.member;
            let mut text = member.text.clone();
            let mut expand_child = None;
            match &member_node.child {
                Some(child) => match child.state {
                    NodeState::Recursive => text.push_str("  (recursive)"),
                    NodeState::SeeAbove => text.push_str("  (see above)"),
                    NodeState::NotFound => text.push_str("  (not found)"),
                    NodeState::Expanded => {
                        if child.members.is_empty() && child.enumerators.is_empty() {
                            if let Some(alias) = &child.alias {
                                text.push_str(&format!("  (= {})", alias.text));
                            }
                        } else {
                            if let Some(path) = &child.path {
                                let row = child.range.map_or(0, |range| range.start.row);
                                text.push_str(&format!("  ({path}:{})", row + 1));
                            }
                            expand_child = Some(child);
                        }
                    }
                },
                None => {}
            }
            if member.kind == crate::type_layout::MemberKind::Unparsed {
                text.push_str("  (unparsed)");
            }
            if let Some(condition) = &member.condition {
                text.push_str(&format!("  [#if {condition}]"));
            }
            if let Some(comment) = &member.comment {
                text.push_str(&format!("  // {comment}"));
            }
            self.line(indent, &text);
            if let Some(child) = expand_child {
                self.children(child, indent + 1);
            }
        }
        for enumerator in &node.enumerators {
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
            self.line(indent, &text);
        }
    }
}
