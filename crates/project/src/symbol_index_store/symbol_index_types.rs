//! ADR-F68: the type engine of the symbol index ([`symbol_index::TypeResolver`]), asked
//! through the store: what the name at a position is and what its type holds, the definitions
//! of a type name, one level of a member, and a type's members as text.
//!
//! The engine reads files, so it runs where they are: on this machine, or on the remote
//! server, whose answers come back over the wire. Everything here is found by name, not by a
//! language server, and shown as such. A definition in a file the settings hide (excluded,
//! private, ignored, or outside the worktree) is left out as if the index did not have it: it
//! is neither read, nor listed, nor counted.

use std::{
    ops::RangeInclusive,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

use symbol_index::{
    Enumerator, Member, MemberKind, NearFile, ResolvedType, TypeKind, TypeLayout, TypeResolver,
};

use super::*;

pub use symbol_index::{ChainRootOrigin, Confidence, TagKind, TypeText, parse_type_text};

/// Candidates listed for one name, read or not.
pub const MAX_TYPE_CANDIDATES: usize = symbol_index::MAX_CANDIDATES;
/// Candidates whose members one request may ask to read.
pub const MAX_TYPE_READ_LIMIT: usize = 4;
/// Members (and enumerators) listed for one type, counting those of bodies defined in place.
pub const MAX_TYPE_MEMBERS: usize = 1024;
/// Each text of a member (its declaration, type, comment, condition) is cut to this.
pub const MAX_TYPE_TEXT_CHARS: usize = 512;
/// A macro's `#define` lines are cut to this.
pub const MAX_MACRO_TEXT_CHARS: usize = 8 * 1024;
/// The texts of one answer together; members past it are left out and `truncated` is set.
pub const MAX_TYPE_RESPONSE_BYTES: usize = 512 * 1024;
/// The most of a buffer a client of a remote server sends with a
/// [`SymbolIndexStore::describe_type_at`] request (the host cuts larger windows too).
pub const MAX_TYPE_WINDOW_BYTES: usize = 64 * 1024;
/// Files are no longer read for a request after this; what was found by then is returned
/// and marked truncated.
pub const TYPE_QUERY_TIME_LIMIT: Duration = Duration::from_secs(5);
pub const DEFAULT_TYPE_TREE_DEPTH: usize = 2;
pub const MAX_TYPE_TREE_DEPTH: usize = 4;
pub const DEFAULT_TYPE_TREE_POINTER_HOPS: usize = 1;
pub const MAX_TYPE_TREE_POINTER_HOPS: usize = 2;
pub const DEFAULT_TYPE_TREE_NODES: usize = 400;
pub const MAX_TYPE_TREE_NODES: usize = 2000;
pub const DEFAULT_TYPE_TREE_CHARS: usize = 16 * 1024;
pub const MAX_TYPE_TREE_CHARS: usize = 64 * 1024;
/// Bodies defined in place inside each other, as far as they are summarized.
const MAX_INLINE_DEPTH: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TypeSummaryKind {
    Struct,
    Union,
    Enum,
    Class,
    Typedef,
}

/// A type definition, found again by its position on each request: the index's own ids do not
/// survive an update, and this can cross the wire.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TypeRef {
    pub path: ProjectPath,
    /// The tag or typedef name the definition declares.
    pub name: String,
    /// Where the definition starts (a `typedef` or the tag keyword), in the file on disk.
    pub position: Point,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypedefStepSummary {
    pub name: String,
    pub path: ProjectPath,
    pub range: Range<Point>,
    /// What the typedef names, as written: `struct foo *`.
    pub target: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeSummary {
    /// `struct foo`, `foo_t` for a typedef of an anonymous struct, `union (anonymous)`.
    pub name: String,
    pub kind: TypeSummaryKind,
    pub path: ProjectPath,
    /// The definition's name (or its start, for a body without one).
    pub range: Range<Point>,
    /// For [`SymbolIndexStore::expand_member`] and [`SymbolIndexStore::type_tree_text`]. A body
    /// defined in place carries the reference of the type it is in, and its members'
    /// [`MemberSummary::index_path`] start from there. `None` for an anonymous type that
    /// cannot be found again by name.
    pub type_ref: Option<TypeRef>,
    /// Typedefs followed from the requested name to this definition.
    pub typedef_chain: Vec<TypedefStepSummary>,
    /// The `#if` conditions the definition is under, as written.
    pub condition: Option<String>,
    /// For a typedef of a builtin, pointer or function type: what it names.
    pub alias: Option<String>,
    pub base_classes: Vec<String>,
    pub members: Vec<MemberSummary>,
    pub enumerators: Vec<EnumeratorSummary>,
    /// False for a candidate listed by its location only; ask for it with its `type_ref`.
    pub members_read: bool,
    /// Members were left out: the definition was too long for the engine, or the answer for
    /// its limits.
    pub truncated: bool,
    pub is_forward_declaration: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MemberSummaryKind {
    Field,
    FunctionPointer,
    /// A C++ member function.
    Method,
    /// A C11 anonymous struct or union, whose members are reached directly.
    AnonymousAggregate,
    /// A named struct, union or enum defined inside the body without declaring a member.
    NestedType,
    /// A declaration that could not be read (usually a macro); see
    /// [`MemberSummary::unparsed_text`].
    Unparsed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberSummary {
    pub name: Option<String>,
    pub kind: MemberSummaryKind,
    /// The declaration alone, whitespace collapsed: `unsigned int flags : 3`.
    pub text: String,
    /// The type without the name: `const struct file_operations *`.
    pub type_text: String,
    /// The name to look up for the next level (`file_operations`, `u32`).
    pub core_name: Option<String>,
    pub tag: Option<TagKind>,
    pub pointer_depth: u32,
    /// Dimensions as written, outermost first; empty for `[]`.
    pub array: Vec<String>,
    pub bit_width: Option<String>,
    pub is_function_pointer: bool,
    /// For a function pointer or a method: its type, `int (*)(struct inode *)`.
    pub signature: Option<String>,
    pub comment: Option<String>,
    pub condition: Option<String>,
    /// The member's name, or its whole declaration without one.
    pub range: Range<Point>,
    /// [`SymbolIndexStore::expand_member`] can lead somewhere: a body defined in place, or a
    /// named type that is not a builtin.
    pub expandable: bool,
    pub unparsed_text: Option<String>,
    /// Indices from the members of the type its [`TypeSummary::type_ref`] names down to this
    /// member, through bodies defined in place ([`MemberSelector::Path`]). Empty for a member
    /// of a member access chain, which is selected by name.
    pub index_path: Vec<u32>,
    /// A body defined in place (`struct { ... } u;`).
    pub inline: Option<Box<TypeSummary>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumeratorSummary {
    pub name: String,
    pub value_text: Option<String>,
    /// When written as an integer literal or counted from one, and within `i64`.
    pub value: Option<i64>,
    pub value_is_inferred: bool,
    pub range: Range<Point>,
    pub comment: Option<String>,
    pub condition: Option<String>,
}

/// Definitions of a type name, best first: those whose members were read, then the rest by
/// location.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResolvedTypes {
    pub types: Vec<TypeSummary>,
    /// Candidates there are, including those past [`MAX_TYPE_CANDIDATES`].
    pub total: usize,
    /// More candidates exist than listed, or an answer limit (size, time) cut something.
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MacroSummary {
    pub name: String,
    pub path: ProjectPath,
    pub range: Range<Point>,
    pub is_function_like: bool,
    /// The `#define` line and its continuation lines.
    pub text: String,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainOwnerSummary {
    pub display_name: String,
    pub path: Option<ProjectPath>,
    pub range: Range<Point>,
    pub type_ref: Option<TypeRef>,
    /// Same-name definitions there were to choose from; the first was used.
    pub candidates: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainStepSummary {
    pub name: String,
    /// `None` when the type holding the member was not found.
    pub owner: Option<ChainOwnerSummary>,
    /// `None` when the member is not in the owner (or the owner is unknown).
    pub member: Option<MemberSummary>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeHoverSubject {
    TypeName {
        name: String,
        tag: Option<TagKind>,
    },
    /// A variable, parameter or member whose declaration was found, or a global found by name.
    Variable {
        type_text: String,
        core_name: Option<String>,
        confidence: Confidence,
        /// In the file asked about; `None` for a global found elsewhere by name.
        declaration: Option<Range<Point>>,
    },
    /// `a->b.c`, followed member by member.
    Member {
        root_type: Option<String>,
        steps: Vec<ChainStepSummary>,
        /// Every member was found.
        complete: bool,
        /// Where `root_type` was read from, so that a caller holding files back can judge the
        /// declarations it came from; `None` from a host too old to say.
        root_origin: Option<ChainRootOrigin>,
        /// The variable or function the root names ([`ChainRootOrigin::Global`],
        /// [`ChainRootOrigin::Call`]).
        root_name: Option<String>,
    },
    Macro {
        macros: Vec<MacroSummary>,
    },
}

/// What [`SymbolIndexStore::describe_type_at`] found at a position: a name match, not what a
/// language server resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeHover {
    pub name: String,
    /// The name at the position, in the file asked about.
    pub range: Range<Point>,
    pub subject: TypeHoverSubject,
    /// The definitions of the subject's type. Empty for builtin and function types, macros,
    /// names not found, and a member whose type is defined in place (see the last step's
    /// [`MemberSummary::inline`]).
    pub types: ResolvedTypes,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MemberSelector {
    /// Looked up the way C does, through anonymous structs and unions.
    Name(String),
    /// A [`MemberSummary::index_path`].
    Path(Vec<u32>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemberExpansionTarget {
    /// The owner, or the member in it, was not found (the file changed, or is hidden now).
    NotFound,
    /// A builtin, function pointer or unparsed member.
    NotExpandable,
    Inline(Box<TypeSummary>),
    Types(ResolvedTypes),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberExpansion {
    pub member: Option<MemberSummary>,
    pub target: MemberExpansionTarget,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TypeTreeSubject {
    Ref(TypeRef),
    /// The best definition of the name.
    Name {
        name: String,
        tag: Option<TagKind>,
        near: Option<ProjectPath>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TypeTreeOptions {
    /// Levels of named types expanded below the root.
    pub max_depth: usize,
    /// Pointers followed on the way down.
    pub max_pointer_hops: usize,
    /// Members and enumerators in the whole tree.
    pub max_nodes: usize,
    pub max_chars: usize,
}

impl Default for TypeTreeOptions {
    fn default() -> Self {
        Self {
            max_depth: DEFAULT_TYPE_TREE_DEPTH,
            max_pointer_hops: DEFAULT_TYPE_TREE_POINTER_HOPS,
            max_nodes: DEFAULT_TYPE_TREE_NODES,
            max_chars: DEFAULT_TYPE_TREE_CHARS,
        }
    }
}

impl TypeTreeOptions {
    /// Within the limits the host holds any request to.
    pub fn clamped(self) -> Self {
        Self {
            max_depth: self.max_depth.min(MAX_TYPE_TREE_DEPTH),
            max_pointer_hops: self.max_pointer_hops.min(MAX_TYPE_TREE_POINTER_HOPS),
            max_nodes: self.max_nodes.clamp(1, MAX_TYPE_TREE_NODES),
            max_chars: self.max_chars.clamp(64, MAX_TYPE_TREE_CHARS),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeTreeText {
    /// Indented members, as [`symbol_index::render_text`] writes them.
    pub text: String,
    pub truncated: bool,
    /// Every definition the text names, with its 0-based row.
    pub definitions: Vec<(ProjectPath, u32)>,
}

/// The name [`SymbolIndexStore::resolve_type`] should look up for a type as a language server
/// writes it (`const struct foo *`, `foo_t [4]`), and its tag.
pub fn type_name_of_text(
    text: &str,
    language: SourceLanguage,
) -> Option<(String, Option<TagKind>)> {
    let parsed = parse_type_text(text, language)?;
    if !parsed.is_expandable() {
        return None;
    }
    Some((parsed.core_name?, parsed.tag))
}

fn clamp_read_limit(limit: usize) -> usize {
    limit.clamp(1, MAX_TYPE_READ_LIMIT)
}

fn clip_text(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((end, _)) => {
            let mut clipped = text.get(..end).unwrap_or_default().to_string();
            clipped.push('…');
            clipped
        }
        None => text.to_string(),
    }
}

/// Picks whole rows around `cursor_row` within `first_row..=last_row` whose lines (and their
/// newlines) fit in `max_bytes`, favoring the rows above, where declarations are. `None` when
/// the cursor's own line does not fit.
pub(crate) fn window_rows(
    first_row: u32,
    last_row: u32,
    cursor_row: u32,
    max_bytes: usize,
    line_len: impl Fn(u32) -> usize,
) -> Option<(u32, u32)> {
    if cursor_row < first_row || cursor_row > last_row {
        return None;
    }
    let mut used = line_len(cursor_row) + 1;
    if used > max_bytes {
        return None;
    }
    let (mut start, mut end) = (cursor_row, cursor_row);
    let upward_share = max_bytes / 4 * 3;
    while start > first_row {
        let next = line_len(start - 1) + 1;
        if used + next > upward_share {
            break;
        }
        used += next;
        start -= 1;
    }
    while end < last_row {
        let next = line_len(end + 1) + 1;
        if used + next > max_bytes {
            break;
        }
        used += next;
        end += 1;
    }
    while start > first_row {
        let next = line_len(start - 1) + 1;
        if used + next > max_bytes {
            break;
        }
        used += next;
        start -= 1;
    }
    Some((start, end))
}

/// The lines of the function around `position` in `snapshot` (or, outside one, the lines
/// around it), cut to [`MAX_TYPE_WINDOW_BYTES`], and the row they start at.
pub(crate) fn buffer_window(
    snapshot: &language::BufferSnapshot,
    position: Point,
) -> Option<(String, u32)> {
    let position = snapshot.clip_point(position, text::Bias::Left);
    let offset = snapshot.point_to_offset(position);
    let mut rows = (0, snapshot.max_point().row);
    if let Some(layer) = snapshot.syntax_layer_at(offset) {
        let mut node = layer.node().descendant_for_byte_range(offset, offset);
        while let Some(current) = node {
            if current.kind() == "function_definition" {
                let start = u32::try_from(current.start_position().row).unwrap_or(0);
                let end = u32::try_from(current.end_position().row).unwrap_or(u32::MAX);
                rows = (start, end.min(snapshot.max_point().row));
                break;
            }
            node = current.parent();
        }
    }
    let (start, end) = window_rows(rows.0, rows.1, position.row, MAX_TYPE_WINDOW_BYTES, |row| {
        snapshot.line_len(row) as usize
    })?;
    let range = Point::new(start, 0)..Point::new(end, snapshot.line_len(end));
    Some((snapshot.text_for_range(range).collect(), start))
}

/// A window a client sent, cut to this host's limit around the cursor.
fn clip_window(window: &str, cursor_row_in_window: u32) -> Option<String> {
    if window.len() <= MAX_TYPE_WINDOW_BYTES {
        return Some(window.to_string());
    }
    let lines = window.split('\n').collect::<Vec<_>>();
    let last_row = u32::try_from(lines.len().saturating_sub(1)).ok()?;
    let (start, end) = window_rows(
        0,
        last_row,
        cursor_row_in_window,
        MAX_TYPE_WINDOW_BYTES,
        |row| lines.get(row as usize).map_or(0, |line| line.len()),
    )?;
    Some(lines.get(start as usize..=end as usize)?.join("\n")).map(|clipped| {
        // Rows before the kept part are gone; keep them as empty lines so that rows stay put.
        let mut padded = "\n".repeat(start as usize);
        padded.push_str(&clipped);
        padded
    })
}

fn byte_of_point(source: &[u8], point: Point) -> Option<usize> {
    let mut line_start = 0usize;
    for _ in 0..point.row {
        let newline = source.get(line_start..)?.iter().position(|&c| c == b'\n')?;
        line_start += newline + 1;
    }
    let line_end = source
        .get(line_start..)?
        .iter()
        .position(|&c| c == b'\n')
        .map_or(source.len(), |offset| line_start + offset);
    Some((line_start + point.column as usize).min(line_end))
}

/// The buffer's text as the file on disk has it (CRLF where the file uses CRLF), so that the
/// index's byte offsets into the same file still fit, and the cursor's offset in it.
fn buffer_source(snapshot: &language::BufferSnapshot, position: Point) -> (Vec<u8>, usize) {
    let position = snapshot.clip_point(position, text::Bias::Left);
    let text = snapshot.text();
    let offset = snapshot.point_to_offset(position);
    if snapshot.line_ending() == text::LineEnding::Windows {
        (
            text.replace('\n', "\r\n").into_bytes(),
            offset + position.row as usize,
        )
    } else {
        (text.into_bytes(), offset)
    }
}

fn point_of(position: symbol_index::Position) -> Point {
    Point::new(position.row, position.column)
}

fn shift_rows(range: symbol_index::Range, rows: u32) -> Range<Point> {
    let range = range_to_points(range);
    Point::new(range.start.row.saturating_add(rows), range.start.column)
        ..Point::new(range.end.row.saturating_add(rows), range.end.column)
}

fn summary_kind(kind: TypeKind) -> TypeSummaryKind {
    match kind {
        TypeKind::Struct => TypeSummaryKind::Struct,
        TypeKind::Union => TypeSummaryKind::Union,
        TypeKind::Enum => TypeSummaryKind::Enum,
        TypeKind::Class => TypeSummaryKind::Class,
        TypeKind::Typedef => TypeSummaryKind::Typedef,
    }
}

fn member_kind(kind: MemberKind) -> MemberSummaryKind {
    match kind {
        MemberKind::Field => MemberSummaryKind::Field,
        MemberKind::FunctionPointer => MemberSummaryKind::FunctionPointer,
        MemberKind::Method => MemberSummaryKind::Method,
        MemberKind::AnonymousAggregate => MemberSummaryKind::AnonymousAggregate,
        MemberKind::NestedType => MemberSummaryKind::NestedType,
        MemberKind::Unparsed => MemberSummaryKind::Unparsed,
    }
}

/// The name a definition can be found again by.
fn locating_name(layout: &TypeLayout) -> Option<String> {
    layout.tag_name.clone().or_else(|| {
        layout
            .typedef_names
            .first()
            .map(|typedef| typedef.name.clone())
    })
}

/// `struct foo` -> `foo`; `foo_t` -> `foo_t`; nothing for an anonymous type.
fn name_of_display_name(display_name: &str) -> Option<String> {
    if display_name.ends_with("(anonymous)") {
        return None;
    }
    let name = ["struct ", "union ", "enum ", "class "]
        .iter()
        .find_map(|keyword| display_name.strip_prefix(keyword))
        .unwrap_or(display_name);
    (!name.is_empty()).then(|| name.to_string())
}

/// Turns what the engine found into the project's types, keeping to the answer's size limit.
struct Converter {
    worktree_id: WorktreeId,
    budget: usize,
    truncated: bool,
}

impl Converter {
    fn new(worktree_id: WorktreeId) -> Self {
        Self {
            worktree_id,
            budget: MAX_TYPE_RESPONSE_BYTES,
            truncated: false,
        }
    }

    fn project_path(&self, path: &str) -> Option<ProjectPath> {
        Some(ProjectPath {
            worktree_id: self.worktree_id,
            path: RelPath::from_unix_str(path).ok()?.into_arc(),
        })
    }

    fn text(&mut self, text: &str) -> String {
        let clipped = clip_text(text, MAX_TYPE_TEXT_CHARS);
        self.budget = self.budget.saturating_sub(clipped.len());
        clipped
    }

    fn optional_text(&mut self, text: Option<&String>) -> Option<String> {
        text.map(|text| self.text(text))
    }

    /// Whether another member fits; once one does not, the answer is marked truncated.
    fn room_for_member(&mut self, members_so_far: &mut usize) -> bool {
        if self.budget == 0 || *members_so_far >= MAX_TYPE_MEMBERS {
            self.truncated = true;
            return false;
        }
        *members_so_far += 1;
        true
    }

    fn resolved(&mut self, resolved: &ResolvedType) -> Option<TypeSummary> {
        let path = self.project_path(&resolved.path)?;
        let layout = &resolved.layout;
        let type_ref = locating_name(layout).map(|name| TypeRef {
            path: path.clone(),
            name,
            position: point_of(layout.range.start),
        });
        let typedef_chain = resolved
            .typedef_chain
            .iter()
            .filter_map(|step| {
                Some(TypedefStepSummary {
                    name: step.name.clone(),
                    path: self.project_path(&step.path)?,
                    range: range_to_points(step.range),
                    target: clip_text(&step.target.text, MAX_TYPE_TEXT_CHARS),
                })
            })
            .collect::<Vec<_>>();
        if typedef_chain.len() != resolved.typedef_chain.len() {
            return None;
        }
        let mut summary = self.layout_summary(layout, &path, type_ref, &[], 0);
        summary.typedef_chain = typedef_chain;
        summary.condition = self.optional_text(resolved.condition.as_ref());
        Some(summary)
    }

    fn layout_summary(
        &mut self,
        layout: &TypeLayout,
        path: &ProjectPath,
        type_ref: Option<TypeRef>,
        index_prefix: &[u32],
        depth: usize,
    ) -> TypeSummary {
        let mut summary = TypeSummary {
            name: layout.display_name(),
            kind: summary_kind(layout.kind),
            path: path.clone(),
            range: range_to_points(layout.name_range.unwrap_or(layout.range)),
            type_ref: type_ref.clone(),
            typedef_chain: Vec::new(),
            condition: None,
            alias: layout
                .alias
                .as_ref()
                .map(|alias| clip_text(&alias.text, MAX_TYPE_TEXT_CHARS)),
            base_classes: layout
                .base_classes
                .iter()
                .map(|base| clip_text(base, MAX_TYPE_TEXT_CHARS))
                .collect(),
            members: Vec::new(),
            enumerators: Vec::new(),
            members_read: true,
            truncated: layout.truncated,
            is_forward_declaration: layout.is_forward_declaration,
        };
        let mut count = 0;
        for (index, member) in layout.members.iter().enumerate() {
            if !self.room_for_member(&mut count) {
                summary.truncated = true;
                break;
            }
            let mut index_path = index_prefix.to_vec();
            index_path.push(u32::try_from(index).unwrap_or(u32::MAX));
            let member = self.member(member, path, type_ref.as_ref(), index_path, depth);
            summary.members.push(member);
        }
        for enumerator in &layout.enumerators {
            if !self.room_for_member(&mut count) {
                summary.truncated = true;
                break;
            }
            let enumerator = self.enumerator(enumerator);
            summary.enumerators.push(enumerator);
        }
        summary
    }

    fn enumerator(&mut self, enumerator: &Enumerator) -> EnumeratorSummary {
        EnumeratorSummary {
            name: self.text(&enumerator.name),
            value_text: self.optional_text(enumerator.value_text.as_ref()),
            value: enumerator.value.and_then(|value| i64::try_from(value).ok()),
            value_is_inferred: enumerator.value_is_inferred,
            range: range_to_points(enumerator.name_range),
            comment: self.optional_text(enumerator.comment.as_ref()),
            condition: self.optional_text(enumerator.condition.as_ref()),
        }
    }

    fn member(
        &mut self,
        member: &Member,
        path: &ProjectPath,
        owner: Option<&TypeRef>,
        index_path: Vec<u32>,
        depth: usize,
    ) -> MemberSummary {
        let kind = member_kind(member.kind);
        let text = self.text(&member.text);
        let type_text = self.text(&member.ty.text);
        let is_function_pointer =
            member.ty.is_function_pointer || kind == MemberSummaryKind::FunctionPointer;
        let has_signature =
            is_function_pointer || kind == MemberSummaryKind::Method || member.ty.is_function;
        let inline = match &member.nested {
            Some(nested) if depth < MAX_INLINE_DEPTH => Some(Box::new(self.layout_summary(
                nested,
                path,
                owner.cloned(),
                &index_path,
                depth + 1,
            ))),
            _ => None,
        };
        MemberSummary {
            name: member.name.clone(),
            kind,
            signature: has_signature.then(|| type_text.clone()),
            unparsed_text: (kind == MemberSummaryKind::Unparsed).then(|| text.clone()),
            text,
            type_text,
            core_name: member.ty.core_name.clone(),
            tag: member.ty.tag,
            pointer_depth: member.ty.pointer_depth,
            array: member
                .ty
                .array_dimensions
                .iter()
                .map(|dimension| self.text(dimension))
                .collect(),
            bit_width: self.optional_text(member.bit_width.as_ref()),
            is_function_pointer,
            comment: self.optional_text(member.comment.as_ref()),
            condition: self.optional_text(member.condition.as_ref()),
            range: range_to_points(member.name_range.unwrap_or(member.range)),
            expandable: member.nested.is_some()
                || (member.kind != MemberKind::Unparsed && member.ty.is_expandable()),
            index_path,
            inline,
        }
    }

    /// A candidate listed by its location only.
    fn unread(&mut self, definition: &Definition) -> Option<TypeSummary> {
        let path = self.project_path(&definition.path)?;
        let kind = match definition.kind {
            DefinitionKind::Struct => TypeSummaryKind::Struct,
            DefinitionKind::Union => TypeSummaryKind::Union,
            DefinitionKind::Enum => TypeSummaryKind::Enum,
            DefinitionKind::Class => TypeSummaryKind::Class,
            _ => TypeSummaryKind::Typedef,
        };
        let keyword = match kind {
            TypeSummaryKind::Struct => "struct ",
            TypeSummaryKind::Union => "union ",
            TypeSummaryKind::Enum => "enum ",
            TypeSummaryKind::Class => "class ",
            TypeSummaryKind::Typedef => "",
        };
        self.budget = self
            .budget
            .saturating_sub(definition.name.len() + definition.path.len());
        Some(TypeSummary {
            name: format!("{keyword}{}", definition.name),
            kind,
            type_ref: Some(TypeRef {
                path: path.clone(),
                name: definition.name.clone(),
                position: point_of(definition.item_range.start),
            }),
            path,
            range: range_to_points(definition.name_range),
            typedef_chain: Vec::new(),
            condition: None,
            alias: None,
            base_classes: Vec::new(),
            members: Vec::new(),
            enumerators: Vec::new(),
            members_read: false,
            truncated: false,
            is_forward_declaration: definition.is_declaration,
        })
    }

    /// `read` (in its order) followed by the other candidates of `name` by location.
    fn resolved_types(
        &mut self,
        resolver: &TypeResolver,
        read: &[ResolvedType],
        name: &str,
        tag: Option<TagKind>,
        near: &NearFile,
    ) -> ResolvedTypes {
        let mut result = ResolvedTypes::default();
        // Rows each listed definition spans: `typedef struct foo { ... } foo_t;` is one
        // definition, read as `struct foo` from its first row, while the index has `foo_t`
        // at the row of its name, below the body.
        let mut listed: Vec<(Arc<str>, RangeInclusive<u32>)> = Vec::new();
        for resolved in read {
            for step in &resolved.typedef_chain {
                listed.push((step.path.clone(), step.range.start.row..=step.range.end.row));
            }
            listed.push((
                resolved.path.clone(),
                resolved.layout.range.start.row..=resolved.layout.range.end.row,
            ));
            if let Some(summary) = self.resolved(resolved) {
                result.types.push(summary);
            }
        }
        let mut unlisted = 0;
        for definition in resolver.type_candidates(name, tag, near) {
            let (item, name_range) = (&definition.item_range, &definition.name_range);
            let rows =
                item.start.row.min(name_range.start.row)..=item.end.row.max(name_range.end.row);
            let already_listed = listed.iter().any(|(path, listed_rows)| {
                *path == definition.path
                    && listed_rows.start() <= rows.end()
                    && rows.start() <= listed_rows.end()
            });
            if already_listed {
                continue;
            }
            listed.push((definition.path.clone(), rows));
            if result.types.len() >= MAX_TYPE_CANDIDATES || self.budget == 0 {
                unlisted += 1;
                continue;
            }
            if let Some(summary) = self.unread(&definition) {
                result.types.push(summary);
            }
        }
        result.total = result.types.len() + unlisted;
        result.truncated = unlisted > 0;
        result
    }

    fn macro_summary(&mut self, found: &symbol_index::MacroText) -> Option<MacroSummary> {
        let text = clip_text(&found.text, MAX_MACRO_TEXT_CHARS);
        self.budget = self.budget.saturating_sub(text.len());
        Some(MacroSummary {
            name: found.name.clone(),
            path: self.project_path(&found.path)?,
            range: range_to_points(found.name_range),
            is_function_like: found.is_function_like,
            truncated: found.truncated || text.len() < found.text.len(),
            text,
        })
    }

    /// `row_shift` moves positions in the file asked about, for a hover made in a window
    /// of it.
    fn hover(
        &mut self,
        resolver: &TypeResolver,
        hover: symbol_index::TypeHover,
        request_path: &str,
        near: &NearFile,
        read_limit: usize,
        row_shift: u32,
    ) -> TypeHover {
        let looked_up = match &hover.subject {
            symbol_index::HoverSubject::TypeName { name, tag } => Some((name.clone(), *tag)),
            symbol_index::HoverSubject::Variable(variable) => variable
                .ty
                .is_expandable()
                .then(|| {
                    variable
                        .ty
                        .core_name
                        .clone()
                        .map(|name| (name, variable.ty.tag))
                })
                .flatten(),
            symbol_index::HoverSubject::Member(chain) => chain
                .final_member()
                .filter(|member| member.nested.is_none() && member.ty.is_expandable())
                .and_then(|member| Some((member.ty.core_name.clone()?, member.ty.tag))),
            symbol_index::HoverSubject::Macro(_) => None,
        };
        let read = hover
            .types
            .iter()
            .take(read_limit)
            .cloned()
            .collect::<Vec<_>>();
        let types = match &looked_up {
            Some((name, tag)) => self.resolved_types(resolver, &read, name, *tag, near),
            None => ResolvedTypes::default(),
        };
        let subject = match hover.subject {
            symbol_index::HoverSubject::TypeName { name, tag } => {
                TypeHoverSubject::TypeName { name, tag }
            }
            symbol_index::HoverSubject::Variable(variable) => TypeHoverSubject::Variable {
                type_text: self.text(&variable.ty.text),
                core_name: variable.ty.core_name.clone(),
                confidence: variable.confidence,
                declaration: variable
                    .declaration
                    .filter(|_| variable.confidence != Confidence::NameOnly)
                    .map(|range| shift_rows(range, row_shift)),
            },
            symbol_index::HoverSubject::Member(chain) => {
                let mut steps = Vec::new();
                for step in &chain.steps {
                    let owner = step.owner.as_ref().map(|owner| {
                        let path = owner
                            .path
                            .as_deref()
                            .and_then(|path| self.project_path(path));
                        ChainOwnerSummary {
                            display_name: owner.display_name.clone(),
                            type_ref: path.clone().and_then(|path| {
                                Some(TypeRef {
                                    path,
                                    name: name_of_display_name(&owner.display_name)?,
                                    position: point_of(owner.range.start),
                                })
                            }),
                            path,
                            range: range_to_points(owner.range),
                            candidates: owner.candidates,
                        }
                    });
                    let owner_path = owner
                        .as_ref()
                        .and_then(|owner| owner.path.clone())
                        .or_else(|| self.project_path(request_path));
                    let member = match (&step.member, owner_path) {
                        (Some(member), Some(path)) => {
                            let owner_ref = owner.as_ref().and_then(|owner| owner.type_ref.clone());
                            Some(self.member(member, &path, owner_ref.as_ref(), Vec::new(), 0))
                        }
                        _ => None,
                    };
                    steps.push(ChainStepSummary {
                        name: step.name.clone(),
                        owner,
                        member,
                    });
                }
                TypeHoverSubject::Member {
                    root_type: chain.root_type.as_ref().map(|root| self.text(&root.text)),
                    steps,
                    complete: chain.complete,
                    root_origin: Some(chain.root_origin),
                    root_name: chain.root_name.as_ref().map(|name| self.text(name)),
                }
            }
            symbol_index::HoverSubject::Macro(macros) => TypeHoverSubject::Macro {
                macros: macros
                    .iter()
                    .filter_map(|found| self.macro_summary(found))
                    .collect(),
            },
        };
        TypeHover {
            name: hover.name,
            range: shift_rows(hover.range, row_shift),
            subject,
            types,
        }
    }
}

impl ResolvedTypes {
    /// Leaves out every definition `shown` refuses, with what it would have counted.
    pub fn retain_shown(&mut self, shown: &mut dyn FnMut(&ProjectPath) -> bool) {
        let before = self.types.len();
        self.types.retain_mut(|summary| summary.retain_shown(shown));
        self.total = self.total.saturating_sub(before - self.types.len());
    }
}

impl TypeSummary {
    /// Whether `shown` lets the definition (and the typedefs that lead to it) be shown; bodies
    /// defined in place are held to it too.
    fn retain_shown(&mut self, shown: &mut dyn FnMut(&ProjectPath) -> bool) -> bool {
        if !shown(&self.path)
            || self
                .type_ref
                .as_ref()
                .is_some_and(|type_ref| !shown(&type_ref.path))
            || self.typedef_chain.iter().any(|step| !shown(&step.path))
        {
            return false;
        }
        for member in &mut self.members {
            member.retain_shown(shown);
        }
        true
    }
}

impl MemberSummary {
    fn retain_shown(&mut self, shown: &mut dyn FnMut(&ProjectPath) -> bool) {
        if let Some(inline) = &mut self.inline
            && !inline.retain_shown(shown)
        {
            self.inline = None;
        }
    }
}

impl TypeHover {
    pub fn retain_shown(&mut self, shown: &mut dyn FnMut(&ProjectPath) -> bool) {
        self.types.retain_shown(shown);
        match &mut self.subject {
            TypeHoverSubject::Macro { macros } => macros.retain(|found| shown(&found.path)),
            TypeHoverSubject::Member {
                steps, complete, ..
            } => {
                let hidden_from = steps.iter_mut().position(|step| {
                    let owner_hidden = step.owner.as_ref().is_some_and(|owner| {
                        owner.path.as_ref().is_some_and(|path| !shown(path))
                            || owner
                                .type_ref
                                .as_ref()
                                .is_some_and(|type_ref| !shown(&type_ref.path))
                    });
                    if let Some(member) = &mut step.member {
                        member.retain_shown(shown);
                    }
                    owner_hidden
                });
                if let Some(hidden_from) = hidden_from {
                    steps.truncate(hidden_from);
                    *complete = false;
                }
            }
            TypeHoverSubject::TypeName { .. } | TypeHoverSubject::Variable { .. } => {}
        }
    }
}

impl MemberExpansion {
    pub fn retain_shown(&mut self, shown: &mut dyn FnMut(&ProjectPath) -> bool) {
        if let Some(member) = &mut self.member {
            member.retain_shown(shown);
        }
        match &mut self.target {
            MemberExpansionTarget::Inline(inline) => {
                if !inline.retain_shown(shown) {
                    self.target = MemberExpansionTarget::NotFound;
                    self.member = None;
                }
            }
            MemberExpansionTarget::Types(types) => types.retain_shown(shown),
            MemberExpansionTarget::NotFound | MemberExpansionTarget::NotExpandable => {}
        }
    }
}

impl TypeTreeText {
    /// The text cannot be taken apart, so a tree naming any definition `shown` refuses is not
    /// shown at all. The host already left such definitions out; this catches settings that
    /// changed meanwhile, and a client's own settings.
    fn is_shown(&self, shown: &mut dyn FnMut(&ProjectPath) -> bool) -> bool {
        self.definitions.iter().all(|(path, _)| shown(path))
    }
}

/// Edited buffers a type request reads instead of their files on disk; more are read from disk.
const MAX_EDITED_BUFFERS: usize = 32;

/// A [`FileSource`] that stops reading once the request has taken [`TYPE_QUERY_TIME_LIMIT`]
/// (a slow network drive), so that the engine finishes with what it has, and that reads the
/// project's edited buffers rather than their files.
struct DeadlineFileSource {
    inner: WorktreeFileSource,
    /// By the worktree path, in the unix form the engine asks with.
    edited_buffers: HashMap<String, text::BufferSnapshot>,
    deadline: Instant,
    timed_out: AtomicBool,
}

impl FileSource for DeadlineFileSource {
    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        if Instant::now() >= self.deadline {
            self.timed_out.store(true, Ordering::SeqCst);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the type lookup ran out of time",
            ));
        }
        if let Some(buffer) = self.edited_buffers.get(path) {
            // The buffer's name may be a link to a file the index must not read (private or
            // excluded under its own name); that file is refused here as on disk.
            let relative = RelPath::from_unix_str(path).map_err(|error| {
                io::Error::new(io::ErrorKind::InvalidInput, format!("{error:#}"))
            })?;
            self.inner.resolve(relative)?;
            // With the file's own line endings, as the index's offsets into it count them.
            let mut contents = buffer.text_with_line_endings().into_bytes();
            let limit = IndexConfig::default().max_file_bytes.saturating_add(1);
            contents.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
            return Ok(contents);
        }
        self.inner.read(path)
    }
}

/// What a type request on one worktree needs, taken on the foreground and used on a
/// background thread.
struct TypeEnvironment {
    worktree_id: WorktreeId,
    index: Arc<SymbolIndex>,
    source: WorktreeFileSource,
    edited_buffers: HashMap<String, text::BufferSnapshot>,
    snapshot: Snapshot,
    include_ignored: bool,
}

/// A [`TypeEnvironment`] whose root was resolved, ready to answer.
struct PreparedEnvironment {
    worktree_id: WorktreeId,
    index: Arc<SymbolIndex>,
    files: DeadlineFileSource,
    snapshot: Snapshot,
    include_ignored: bool,
}

impl TypeEnvironment {
    fn prepare(mut self, deadline: Instant) -> Option<PreparedEnvironment> {
        let canonical_root = self.source.canonicalize(&self.source.root).log_err()?;
        self.source.canonical_root = Some(canonical_root);
        Some(PreparedEnvironment {
            worktree_id: self.worktree_id,
            index: self.index,
            files: DeadlineFileSource {
                inner: self.source,
                edited_buffers: self.edited_buffers,
                deadline,
                timed_out: AtomicBool::new(false),
            },
            snapshot: self.snapshot,
            include_ignored: self.include_ignored,
        })
    }
}

impl PreparedEnvironment {
    /// Mirrors [`is_result_shown`] with what the background thread has: the rules of the
    /// settings and the worktree's entries.
    fn shows(&self, path: &str) -> bool {
        let Ok(relative) = RelPath::from_unix_str(path) else {
            return false;
        };
        !self.files.inner.rules.hides(relative)
            && self.snapshot.entry_for_path(relative).is_some_and(|entry| {
                entry.is_file()
                    && !entry.is_private
                    && !entry.is_external
                    && (self.include_ignored || !entry.is_ignored)
            })
    }

    fn timed_out(&self) -> bool {
        self.files.timed_out.load(Ordering::SeqCst)
    }

    fn with_resolver<R>(&self, read_limit: usize, f: impl FnOnce(&TypeResolver) -> R) -> R {
        let filter = |path: &str| self.shows(path);
        let resolver = TypeResolver::new(&self.index, &self.files)
            .with_path_filter(&filter)
            .with_candidate_limit(read_limit);
        f(&resolver)
    }
}

/// Where the text to describe comes from.
enum DescribeSource {
    /// The editor's buffer, whole.
    Buffer(language::BufferSnapshot),
    /// Lines a client of this host sent, starting at `start_row`.
    Window {
        text: String,
        start_row: u32,
    },
    Disk,
}

fn describe_in(
    environment: &PreparedEnvironment,
    path: &str,
    position: Point,
    source: DescribeSource,
    language: Option<SourceLanguage>,
    read_limit: usize,
) -> Option<TypeHover> {
    environment.with_resolver(read_limit, |resolver| {
        let mut converter = Converter::new(environment.worktree_id);
        let mut hover = match source {
            DescribeSource::Window { text, start_row } => {
                let row_in_window = position.row.checked_sub(start_row)?;
                let text = clip_window(&text, row_in_window)?;
                let bytes = text.as_bytes();
                let offset = byte_of_point(bytes, Point::new(row_in_window, position.column))?;
                let language = match language {
                    Some(language) => language,
                    None => {
                        let contents = environment.files.read(path).ok()?;
                        symbol_index::source_language(path, &contents)?
                    }
                };
                let near = resolver.near(path);
                let hover = resolver.describe_in_window(path, bytes, offset, language)?;
                converter.hover(resolver, hover, path, &near, read_limit, start_row)
            }
            DescribeSource::Buffer(snapshot) => {
                let (bytes, offset) = buffer_source(&snapshot, position);
                let language = language.or_else(|| symbol_index::source_language(path, &bytes))?;
                let near = NearFile::new(path, &bytes);
                let hover = resolver.describe_at(path, &bytes, offset, language)?;
                converter.hover(resolver, hover, path, &near, read_limit, 0)
            }
            DescribeSource::Disk => {
                let bytes = environment.files.read(path).ok()?;
                let offset = byte_of_point(&bytes, position)?;
                let language = language.or_else(|| symbol_index::source_language(path, &bytes))?;
                let near = NearFile::new(path, &bytes);
                let hover = resolver.describe_at(path, &bytes, offset, language)?;
                converter.hover(resolver, hover, path, &near, read_limit, 0)
            }
        };
        hover.types.truncated |= converter.truncated || environment.timed_out();
        Some(hover)
    })
}

fn resolve_in(
    environment: &PreparedEnvironment,
    name: &str,
    tag: Option<TagKind>,
    near_path: Option<&str>,
    read_limit: usize,
) -> ResolvedTypes {
    environment.with_resolver(read_limit.max(1), |resolver| {
        let mut converter = Converter::new(environment.worktree_id);
        let near = near_path.map_or_else(NearFile::default, |path| resolver.near(path));
        let read = if read_limit == 0 {
            Vec::new()
        } else {
            resolver.resolve_type_limited(name, tag, &near, read_limit)
        };
        let mut types = converter.resolved_types(resolver, &read, name, tag, &near);
        types.truncated |= converter.truncated || environment.timed_out();
        types
    })
}

fn select_member<'a>(layout: &'a TypeLayout, selector: &MemberSelector) -> Option<&'a Member> {
    match selector {
        MemberSelector::Name(name) => layout.find_member(name),
        MemberSelector::Path(indices) => {
            let (first, rest) = indices.split_first()?;
            let mut member = layout.members.get(*first as usize)?;
            for index in rest {
                member = member.nested.as_ref()?.members.get(*index as usize)?;
            }
            Some(member)
        }
    }
}

fn expand_in(
    environment: &PreparedEnvironment,
    owner: &TypeRef,
    selector: &MemberSelector,
    read_limit: usize,
) -> MemberExpansion {
    let not_found = MemberExpansion {
        member: None,
        target: MemberExpansionTarget::NotFound,
    };
    let owner_path = owner.path.path.as_unix_str();
    if !environment.shows(owner_path) {
        return not_found;
    }
    environment.with_resolver(read_limit, |resolver| {
        let mut converter = Converter::new(environment.worktree_id);
        let Some(root) = resolver.locate_type(
            owner_path,
            owner.position.row,
            owner.position.column,
            &owner.name,
        ) else {
            return not_found;
        };
        let Some(root_path) = converter.project_path(&root.path) else {
            return not_found;
        };
        let root_ref = locating_name(&root.layout).map(|name| TypeRef {
            path: root_path.clone(),
            name,
            position: point_of(root.layout.range.start),
        });
        let Some(member) = select_member(&root.layout, selector) else {
            return not_found;
        };
        let index_path = match selector {
            MemberSelector::Path(indices) => indices.clone(),
            MemberSelector::Name(_) => Vec::new(),
        };
        let summary = converter.member(member, &root_path, root_ref.as_ref(), index_path, 0);
        let target = if let Some(inline) = summary.inline.clone() {
            MemberExpansionTarget::Inline(inline)
        } else if !summary.expandable {
            MemberExpansionTarget::NotExpandable
        } else {
            match member.ty.core_name.as_deref() {
                Some(name) => {
                    let near = resolver.near(&root.path);
                    let read =
                        resolver.resolve_type_limited(name, member.ty.tag, &near, read_limit);
                    let mut types =
                        converter.resolved_types(resolver, &read, name, member.ty.tag, &near);
                    types.truncated |= converter.truncated || environment.timed_out();
                    MemberExpansionTarget::Types(types)
                }
                None => MemberExpansionTarget::NotExpandable,
            }
        };
        MemberExpansion {
            member: Some(summary),
            target,
        }
    })
}

fn collect_tree_definitions(
    node: &symbol_index::TypeNode,
    converter: &Converter,
    definitions: &mut Vec<(ProjectPath, u32)>,
) {
    if let (Some(path), Some(range)) = (&node.path, node.range)
        && let Some(path) = converter.project_path(path)
        && !definitions.contains(&(path.clone(), range.start.row))
    {
        definitions.push((path, range.start.row));
    }
    for step in &node.typedef_chain {
        if let Some(path) = converter.project_path(&step.path)
            && !definitions.contains(&(path.clone(), step.range.start.row))
        {
            definitions.push((path, step.range.start.row));
        }
    }
    for member in &node.members {
        if let Some(child) = &member.child {
            collect_tree_definitions(child, converter, definitions);
        }
    }
}

fn tree_in(
    environment: &PreparedEnvironment,
    subject: &TypeTreeSubject,
    options: TypeTreeOptions,
) -> Option<TypeTreeText> {
    environment.with_resolver(1, |resolver| {
        let root = match subject {
            TypeTreeSubject::Ref(type_ref) => {
                let path = type_ref.path.path.as_unix_str();
                if !environment.shows(path) {
                    return None;
                }
                resolver.locate_type(
                    path,
                    type_ref.position.row,
                    type_ref.position.column,
                    &type_ref.name,
                )?
            }
            TypeTreeSubject::Name { name, tag, near } => {
                let near = near
                    .as_ref()
                    .filter(|near| near.worktree_id == environment.worktree_id)
                    .map_or_else(NearFile::default, |near| {
                        resolver.near(near.path.as_unix_str())
                    });
                resolver
                    .resolve_type_limited(name, *tag, &near, 1)
                    .into_iter()
                    .next()?
            }
        };
        let tree = resolver.expand(
            &root,
            symbol_index::ExpandOptions {
                max_depth: options.max_depth,
                max_pointer_hops: options.max_pointer_hops,
                max_nodes: options.max_nodes,
            },
        );
        let text = symbol_index::render_text(&tree, options.max_chars);
        let converter = Converter::new(environment.worktree_id);
        let mut definitions = Vec::new();
        collect_tree_definitions(&tree.root, &converter, &mut definitions);
        Some(TypeTreeText {
            truncated: tree.truncated
                || text
                    .trim_end()
                    .ends_with(symbol_index::RENDER_TRUNCATED_MARKER)
                || environment.timed_out(),
            text,
            definitions,
        })
    })
}

impl SymbolIndexStore {
    /// What the name at `position` in `path` is (a type name, a variable, a member in
    /// `a->b.c`, a macro) and the definitions of its type, the first `limit` of them (capped
    /// at [`MAX_TYPE_READ_LIMIT`]) with their members. `contents` is the editor's buffer, with
    /// edits not saved yet; without it the file on disk is read. A client of a remote server
    /// sends only the function around the position ([`MAX_TYPE_WINDOW_BYTES`] at most), and
    /// the host finds file-scope declarations through its index and the file on disk.
    /// `language` is taken from the path and contents when not given.
    ///
    /// Blocks a background thread for as long as files take to read (up to
    /// [`TYPE_QUERY_TIME_LIMIT`]); debounce requests and drop the task to cancel.
    pub fn describe_type_at(
        &mut self,
        path: ProjectPath,
        position: Point,
        contents: Option<language::BufferSnapshot>,
        language: Option<SourceLanguage>,
        limit: usize,
        cx: &mut Context<Self>,
    ) -> Task<Result<Option<TypeHover>>> {
        let read_limit = clamp_read_limit(limit);
        match &self.state {
            StoreState::Local(_) => {
                let source = match contents {
                    Some(snapshot) => DescribeSource::Buffer(snapshot),
                    None => DescribeSource::Disk,
                };
                self.describe_local(path, position, source, language, read_limit, cx)
            }
            StoreState::Remote(remote) => {
                let Some((client, project_id)) = remote.client_for_queries() else {
                    return Task::ready(Ok(None));
                };
                let window = contents
                    .as_ref()
                    .and_then(|snapshot| buffer_window(snapshot, position));
                let request = client.request(proto::SymbolIndexDescribeType {
                    project_id,
                    worktree_id: path.worktree_id.to_proto(),
                    path: path.path.as_unix_str().to_string(),
                    row: position.row,
                    column: position.column,
                    window_start_row: window.as_ref().map_or(0, |(_, start)| *start),
                    window: window.map(|(text, _)| text),
                    is_cpp: language.map(|language| language == SourceLanguage::Cpp),
                    limit: read_limit as u32,
                });
                let timeout = cx.background_executor().timer(REMOTE_REQUEST_TIMEOUT);
                cx.spawn(async move |this, cx| {
                    let response = with_timeout(request, timeout).await?;
                    let Some(hover) = response.hover.and_then(hover_from_proto) else {
                        return Ok(None);
                    };
                    this.update(cx, |this, cx| {
                        let mut hover = hover;
                        hover.retain_shown(&mut |path| this.remote_result_shown(path, cx));
                        Some(hover)
                    })
                })
            }
            StoreState::Unavailable(_) => Task::ready(Ok(None)),
        }
    }

    /// The definitions of the type `name` (`foo` of `struct foo` when `tag` is given), best
    /// first, ranked by closeness to `near`; typedefs are followed to what they name. The
    /// members of the first `limit` (capped at [`MAX_TYPE_READ_LIMIT`]) are read, the rest are
    /// listed by location. For a type a language server reported as text, see
    /// [`type_name_of_text`].
    pub fn resolve_type(
        &mut self,
        name: &str,
        tag: Option<TagKind>,
        near: Option<ProjectPath>,
        limit: usize,
        cx: &mut Context<Self>,
    ) -> Task<Result<ResolvedTypes>> {
        let read_limit = clamp_read_limit(limit);
        match &self.state {
            StoreState::Local(_) => {
                let mut worktree_ids = self.local_index_worktrees();
                if let Some(near) = &near {
                    worktree_ids.sort_by_key(|worktree_id| *worktree_id != near.worktree_id);
                }
                let environments = worktree_ids
                    .into_iter()
                    .filter_map(|worktree_id| self.type_environment(worktree_id, cx))
                    .collect::<Vec<_>>();
                let name = name.to_string();
                let lookup = cx.background_spawn(async move {
                    let deadline = Instant::now() + TYPE_QUERY_TIME_LIMIT;
                    let mut merged = ResolvedTypes::default();
                    let mut remaining = read_limit;
                    for environment in environments {
                        let Some(environment) = environment.prepare(deadline) else {
                            continue;
                        };
                        let near_path = near
                            .as_ref()
                            .filter(|near| near.worktree_id == environment.worktree_id)
                            .map(|near| near.path.as_unix_str().to_string());
                        let found =
                            resolve_in(&environment, &name, tag, near_path.as_deref(), remaining);
                        remaining = remaining.saturating_sub(
                            found
                                .types
                                .iter()
                                .filter(|summary| summary.members_read)
                                .count(),
                        );
                        merged.total += found.total;
                        merged.truncated |= found.truncated;
                        for summary in found.types {
                            if merged.types.len() < MAX_TYPE_CANDIDATES {
                                merged.types.push(summary);
                            } else {
                                merged.truncated = true;
                            }
                        }
                    }
                    merged
                });
                cx.spawn(async move |this, cx| {
                    let mut types = lookup.await;
                    this.update(cx, |this, cx| {
                        types.retain_shown(&mut |path| this.is_local_result_shown(path, cx));
                        types
                    })
                })
            }
            StoreState::Remote(remote) => {
                let Some((client, project_id)) = remote.client_for_queries() else {
                    return Task::ready(Ok(ResolvedTypes::default()));
                };
                let request = client.request(proto::SymbolIndexResolveType {
                    project_id,
                    name: name.to_string(),
                    tag: tag_to_proto(tag) as i32,
                    near_worktree_id: near.as_ref().map(|near| near.worktree_id.to_proto()),
                    near_path: near
                        .as_ref()
                        .map(|near| near.path.as_unix_str().to_string()),
                    limit: read_limit as u32,
                });
                let timeout = cx.background_executor().timer(REMOTE_REQUEST_TIMEOUT);
                cx.spawn(async move |this, cx| {
                    let response = with_timeout(request, timeout).await?;
                    let mut types = response
                        .types
                        .map(resolved_types_from_proto)
                        .unwrap_or_default();
                    this.update(cx, |this, cx| {
                        types.retain_shown(&mut |path| this.remote_result_shown(path, cx));
                        types
                    })
                })
            }
            StoreState::Unavailable(_) => Task::ready(Ok(ResolvedTypes::default())),
        }
    }

    /// One level below a member of `owner`: the body it defines in place, or the definitions
    /// of its type (the first `limit` with members, capped at [`MAX_TYPE_READ_LIMIT`]).
    pub fn expand_member(
        &mut self,
        owner: TypeRef,
        member: MemberSelector,
        limit: usize,
        cx: &mut Context<Self>,
    ) -> Task<Result<MemberExpansion>> {
        let read_limit = clamp_read_limit(limit);
        let not_found = MemberExpansion {
            member: None,
            target: MemberExpansionTarget::NotFound,
        };
        match &self.state {
            StoreState::Local(_) => {
                if !self.is_local_result_shown(&owner.path, cx) {
                    return Task::ready(Ok(not_found));
                }
                let Some(environment) = self.type_environment(owner.path.worktree_id, cx) else {
                    return Task::ready(Ok(not_found));
                };
                let lookup = cx.background_spawn(async move {
                    let deadline = Instant::now() + TYPE_QUERY_TIME_LIMIT;
                    match environment.prepare(deadline) {
                        Some(environment) => expand_in(&environment, &owner, &member, read_limit),
                        None => not_found,
                    }
                });
                cx.spawn(async move |this, cx| {
                    let mut expansion = lookup.await;
                    this.update(cx, |this, cx| {
                        expansion.retain_shown(&mut |path| this.is_local_result_shown(path, cx));
                        expansion
                    })
                })
            }
            StoreState::Remote(remote) => {
                let Some((client, project_id)) = remote.client_for_queries() else {
                    return Task::ready(Ok(not_found));
                };
                let (member_name, member_path) = match member {
                    MemberSelector::Name(name) => (Some(name), Vec::new()),
                    MemberSelector::Path(path) => (None, path),
                };
                let request = client.request(proto::SymbolIndexExpandMember {
                    project_id,
                    owner: Some(type_ref_to_proto(&owner)),
                    member_name,
                    member_path,
                    limit: read_limit as u32,
                });
                let timeout = cx.background_executor().timer(REMOTE_REQUEST_TIMEOUT);
                cx.spawn(async move |this, cx| {
                    let response = with_timeout(request, timeout).await?;
                    let mut expansion = expansion_from_proto(response);
                    this.update(cx, |this, cx| {
                        expansion.retain_shown(&mut |path| this.remote_result_shown(path, cx));
                        expansion
                    })
                })
            }
            StoreState::Unavailable(_) => Task::ready(Ok(not_found)),
        }
    }

    /// A type's members as indented text, `options.max_depth` levels of named types down,
    /// for tools; see [`symbol_index::render_text`]. `None` when the type was not found.
    pub fn type_tree_text(
        &mut self,
        subject: TypeTreeSubject,
        options: TypeTreeOptions,
        cx: &mut Context<Self>,
    ) -> Task<Result<Option<TypeTreeText>>> {
        let options = options.clamped();
        match &self.state {
            StoreState::Local(_) => {
                let worktree_ids = match &subject {
                    TypeTreeSubject::Ref(type_ref) => {
                        if !self.is_local_result_shown(&type_ref.path, cx) {
                            return Task::ready(Ok(None));
                        }
                        vec![type_ref.path.worktree_id]
                    }
                    TypeTreeSubject::Name { near, .. } => {
                        let mut worktree_ids = self.local_index_worktrees();
                        if let Some(near) = near {
                            worktree_ids
                                .sort_by_key(|worktree_id| *worktree_id != near.worktree_id);
                        }
                        worktree_ids
                    }
                };
                let environments = worktree_ids
                    .into_iter()
                    .filter_map(|worktree_id| self.type_environment(worktree_id, cx))
                    .collect::<Vec<_>>();
                let lookup = cx.background_spawn(async move {
                    let deadline = Instant::now() + TYPE_QUERY_TIME_LIMIT;
                    environments.into_iter().find_map(|environment| {
                        tree_in(&environment.prepare(deadline)?, &subject, options)
                    })
                });
                cx.spawn(async move |this, cx| {
                    let tree = lookup.await;
                    this.update(cx, |this, cx| {
                        tree.filter(|tree| {
                            tree.is_shown(&mut |path| this.is_local_result_shown(path, cx))
                        })
                    })
                })
            }
            StoreState::Remote(remote) => {
                let Some((client, project_id)) = remote.client_for_queries() else {
                    return Task::ready(Ok(None));
                };
                let mut request = proto::SymbolIndexTypeTree {
                    project_id,
                    max_depth: options.max_depth as u32,
                    max_pointer_hops: options.max_pointer_hops as u32,
                    max_nodes: options.max_nodes as u32,
                    max_chars: options.max_chars as u32,
                    ..Default::default()
                };
                match &subject {
                    TypeTreeSubject::Ref(type_ref) => {
                        request.type_ref = Some(type_ref_to_proto(type_ref));
                    }
                    TypeTreeSubject::Name { name, tag, near } => {
                        request.name = Some(name.clone());
                        request.tag = tag_to_proto(*tag) as i32;
                        request.near_worktree_id =
                            near.as_ref().map(|near| near.worktree_id.to_proto());
                        request.near_path = near
                            .as_ref()
                            .map(|near| near.path.as_unix_str().to_string());
                    }
                }
                let request = client.request(request);
                let timeout = cx.background_executor().timer(REMOTE_REQUEST_TIMEOUT);
                cx.spawn(async move |this, cx| {
                    let response = with_timeout(request, timeout).await?;
                    if !response.found {
                        return Ok(None);
                    }
                    let tree = TypeTreeText {
                        text: clip_text(&response.text, MAX_TYPE_TREE_CHARS),
                        truncated: response.truncated,
                        definitions: response
                            .definitions
                            .into_iter()
                            .filter_map(|location| {
                                let (path, range) = location_from_proto(Some(location))?;
                                Some((path, range.start.row))
                            })
                            .collect(),
                    };
                    this.update(cx, |this, cx| {
                        tree.is_shown(&mut |path| this.remote_result_shown(path, cx))
                            .then_some(tree)
                    })
                })
            }
            StoreState::Unavailable(_) => Task::ready(Ok(None)),
        }
    }

    fn describe_local(
        &mut self,
        path: ProjectPath,
        position: Point,
        source: DescribeSource,
        language: Option<SourceLanguage>,
        read_limit: usize,
        cx: &mut Context<Self>,
    ) -> Task<Result<Option<TypeHover>>> {
        if !self.is_local_result_shown(&path, cx) {
            return Task::ready(Ok(None));
        }
        let Some(environment) = self.type_environment(path.worktree_id, cx) else {
            return Task::ready(Ok(None));
        };
        let request_path = path.path.as_unix_str().to_string();
        let lookup = cx.background_spawn(async move {
            let deadline = Instant::now() + TYPE_QUERY_TIME_LIMIT;
            let environment = environment.prepare(deadline)?;
            describe_in(
                &environment,
                &request_path,
                position,
                source,
                language,
                read_limit,
            )
        });
        cx.spawn(async move |this, cx| {
            let hover = lookup.await;
            this.update(cx, |this, cx| {
                hover.map(|mut hover| {
                    hover.retain_shown(&mut |path| this.is_local_result_shown(path, cx));
                    hover
                })
            })
        })
    }

    fn local_index_worktrees(&self) -> Vec<WorktreeId> {
        match &self.state {
            StoreState::Local(local) => local
                .open_indexes()
                .into_iter()
                .map(|(worktree_id, _)| worktree_id)
                .collect(),
            _ => Vec::new(),
        }
    }

    fn is_local_result_shown(&self, path: &ProjectPath, cx: &App) -> bool {
        self.allowed_path(path.worktree_id, path.path.as_unix_str(), cx)
            .is_some()
    }

    fn type_environment(&self, worktree_id: WorktreeId, cx: &App) -> Option<TypeEnvironment> {
        let StoreState::Local(local) = &self.state else {
            return None;
        };
        let entry = local.worktrees.get(&worktree_id)?;
        let index = entry.index.clone()?;
        let worktree = entry.worktree.upgrade()?;
        let worktree = worktree.read(cx);
        let include_ignored = ProjectSettings::get(
            Some(SettingsLocation {
                worktree_id,
                path: RelPath::empty(),
            }),
            cx,
        )
        .symbol_index
        .include_ignored;
        Some(TypeEnvironment {
            worktree_id,
            index,
            source: WorktreeFileSource {
                read_directly: !local.fs.is_fake(),
                fs: local.fs.clone(),
                root: worktree.abs_path(),
                canonical_root: None,
                rules: Arc::new(PathRules::for_worktree(worktree_id, cx)),
            },
            edited_buffers: self.edited_buffers(worktree_id, local, cx),
            snapshot: worktree.snapshot(),
            include_ignored,
        })
    }

    /// The worktree's C and C++ buffers with edits not saved yet (on a remote server, a
    /// client's edits are in the host's buffers too), that may be shown; past the limit, the
    /// most recently edited ones.
    fn edited_buffers(
        &self,
        worktree_id: WorktreeId,
        local: &LocalStore,
        cx: &App,
    ) -> HashMap<String, text::BufferSnapshot> {
        let Some(buffer_store) = local.buffer_store.upgrade() else {
            return HashMap::default();
        };
        let mut candidates = Vec::new();
        for buffer in buffer_store.read(cx).buffers() {
            let buffer = buffer.read(cx);
            if !buffer.is_dirty() {
                continue;
            }
            let Some(file) = buffer.file() else {
                continue;
            };
            if file.worktree_id(cx) != worktree_id || !is_indexable_path(file.path().as_unix_str())
            {
                continue;
            }
            let last_edit = local
                .buffer_edits
                .get(&buffer.remote_id())
                .copied()
                .unwrap_or(0);
            candidates.push((last_edit, buffer));
        }
        candidates.sort_by_key(|(last_edit, _)| std::cmp::Reverse(*last_edit));
        let mut edited = HashMap::default();
        for (_, buffer) in candidates {
            if edited.len() >= MAX_EDITED_BUFFERS {
                break;
            }
            let Some(file) = buffer.file() else {
                continue;
            };
            let path = file.path().as_unix_str();
            if self.allowed_path(worktree_id, path, cx).is_none() {
                continue;
            }
            edited.insert(path.to_string(), buffer.text_snapshot());
        }
        edited
    }

    pub(super) async fn handle_describe_type(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::SymbolIndexDescribeType>,
        mut cx: AsyncApp,
    ) -> Result<proto::SymbolIndexDescribeTypeResponse> {
        let this = wait_if_imitating_an_older_server(this, &cx).await;
        let payload = envelope.payload;
        let path = ProjectPath {
            worktree_id: WorktreeId::from_proto(payload.worktree_id),
            path: RelPath::from_unix_str(&payload.path)?.into_arc(),
        };
        let source = match payload.window {
            Some(text) => DescribeSource::Window {
                text,
                start_row: payload.window_start_row,
            },
            None => DescribeSource::Disk,
        };
        let language = payload.is_cpp.map(|is_cpp| {
            if is_cpp {
                SourceLanguage::Cpp
            } else {
                SourceLanguage::C
            }
        });
        let read_limit = clamp_read_limit(payload.limit as usize);
        let position = Point::new(payload.row, payload.column);
        let hover = this
            .update(&mut cx, |this, cx| {
                this.describe_local(path, position, source, language, read_limit, cx)
            })
            .await?;
        Ok(proto::SymbolIndexDescribeTypeResponse {
            hover: hover.as_ref().map(hover_to_proto),
        })
    }

    pub(super) async fn handle_resolve_type(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::SymbolIndexResolveType>,
        mut cx: AsyncApp,
    ) -> Result<proto::SymbolIndexResolveTypeResponse> {
        let this = wait_if_imitating_an_older_server(this, &cx).await;
        let payload = envelope.payload;
        let tag = tag_from_proto(payload.tag());
        let near = match (payload.near_worktree_id, payload.near_path) {
            (Some(worktree_id), Some(path)) => Some(ProjectPath {
                worktree_id: WorktreeId::from_proto(worktree_id),
                path: RelPath::from_unix_str(&path)?.into_arc(),
            }),
            _ => None,
        };
        let types = this
            .update(&mut cx, |this, cx| {
                this.resolve_type(&payload.name, tag, near, payload.limit as usize, cx)
            })
            .await?;
        Ok(proto::SymbolIndexResolveTypeResponse {
            types: Some(resolved_types_to_proto(&types)),
        })
    }

    pub(super) async fn handle_expand_member(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::SymbolIndexExpandMember>,
        mut cx: AsyncApp,
    ) -> Result<proto::SymbolIndexExpandMemberResponse> {
        let this = wait_if_imitating_an_older_server(this, &cx).await;
        let payload = envelope.payload;
        let owner = payload
            .owner
            .and_then(type_ref_from_proto)
            .ok_or_else(|| anyhow!("no type to expand a member of"))?;
        let selector = match payload.member_name {
            Some(name) => MemberSelector::Name(name),
            None => MemberSelector::Path(payload.member_path),
        };
        let expansion = this
            .update(&mut cx, |this, cx| {
                this.expand_member(owner, selector, payload.limit as usize, cx)
            })
            .await?;
        Ok(expansion_to_proto(&expansion))
    }

    pub(super) async fn handle_type_tree(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::SymbolIndexTypeTree>,
        mut cx: AsyncApp,
    ) -> Result<proto::SymbolIndexTypeTreeResponse> {
        let this = wait_if_imitating_an_older_server(this, &cx).await;
        let payload = envelope.payload;
        let subject = match (payload.type_ref.clone(), payload.name.clone()) {
            (Some(type_ref), _) => TypeTreeSubject::Ref(
                type_ref_from_proto(type_ref)
                    .ok_or_else(|| anyhow!("an invalid type reference"))?,
            ),
            (None, Some(name)) => TypeTreeSubject::Name {
                name,
                tag: tag_from_proto(payload.tag()),
                near: match (payload.near_worktree_id, payload.near_path.as_deref()) {
                    (Some(worktree_id), Some(path)) => Some(ProjectPath {
                        worktree_id: WorktreeId::from_proto(worktree_id),
                        path: RelPath::from_unix_str(path)?.into_arc(),
                    }),
                    _ => None,
                },
            },
            (None, None) => return Err(anyhow!("no type to expand")),
        };
        let options = TypeTreeOptions {
            max_depth: payload.max_depth as usize,
            max_pointer_hops: payload.max_pointer_hops as usize,
            max_nodes: payload.max_nodes as usize,
            max_chars: payload.max_chars as usize,
        };
        let tree = this
            .update(&mut cx, |this, cx| {
                this.type_tree_text(subject, options, cx)
            })
            .await?;
        Ok(match tree {
            Some(tree) => proto::SymbolIndexTypeTreeResponse {
                found: true,
                text: tree.text,
                truncated: tree.truncated,
                definitions: tree
                    .definitions
                    .iter()
                    .map(|(path, row)| {
                        location_to_proto(path, &(Point::new(*row, 0)..Point::new(*row, 0)))
                    })
                    .collect(),
            },
            None => proto::SymbolIndexTypeTreeResponse::default(),
        })
    }
}

fn tag_to_proto(tag: Option<TagKind>) -> proto::SymbolIndexTagKind {
    use proto::SymbolIndexTagKind as Tag;
    match tag {
        None => Tag::SymbolIndexTagNone,
        Some(TagKind::Struct) => Tag::SymbolIndexTagStruct,
        Some(TagKind::Union) => Tag::SymbolIndexTagUnion,
        Some(TagKind::Enum) => Tag::SymbolIndexTagEnum,
        Some(TagKind::Class) => Tag::SymbolIndexTagClass,
    }
}

fn tag_from_proto(tag: proto::SymbolIndexTagKind) -> Option<TagKind> {
    use proto::SymbolIndexTagKind as Tag;
    match tag {
        Tag::SymbolIndexTagNone => None,
        Tag::SymbolIndexTagStruct => Some(TagKind::Struct),
        Tag::SymbolIndexTagUnion => Some(TagKind::Union),
        Tag::SymbolIndexTagEnum => Some(TagKind::Enum),
        Tag::SymbolIndexTagClass => Some(TagKind::Class),
    }
}

fn type_kind_to_proto(kind: TypeSummaryKind) -> proto::SymbolIndexTypeKind {
    use proto::SymbolIndexTypeKind as Kind;
    match kind {
        TypeSummaryKind::Struct => Kind::SymbolIndexTypeStruct,
        TypeSummaryKind::Union => Kind::SymbolIndexTypeUnion,
        TypeSummaryKind::Enum => Kind::SymbolIndexTypeEnum,
        TypeSummaryKind::Class => Kind::SymbolIndexTypeClass,
        TypeSummaryKind::Typedef => Kind::SymbolIndexTypeTypedef,
    }
}

fn type_kind_from_proto(kind: proto::SymbolIndexTypeKind) -> TypeSummaryKind {
    use proto::SymbolIndexTypeKind as Kind;
    match kind {
        Kind::SymbolIndexTypeStruct => TypeSummaryKind::Struct,
        Kind::SymbolIndexTypeUnion => TypeSummaryKind::Union,
        Kind::SymbolIndexTypeEnum => TypeSummaryKind::Enum,
        Kind::SymbolIndexTypeClass => TypeSummaryKind::Class,
        Kind::SymbolIndexTypeTypedef => TypeSummaryKind::Typedef,
    }
}

fn member_kind_to_proto(kind: MemberSummaryKind) -> proto::SymbolIndexMemberKind {
    use proto::SymbolIndexMemberKind as Kind;
    match kind {
        MemberSummaryKind::Field => Kind::SymbolIndexMemberField,
        MemberSummaryKind::FunctionPointer => Kind::SymbolIndexMemberFunctionPointer,
        MemberSummaryKind::Method => Kind::SymbolIndexMemberMethod,
        MemberSummaryKind::AnonymousAggregate => Kind::SymbolIndexMemberAnonymousAggregate,
        MemberSummaryKind::NestedType => Kind::SymbolIndexMemberNestedType,
        MemberSummaryKind::Unparsed => Kind::SymbolIndexMemberUnparsed,
    }
}

fn member_kind_from_proto(kind: proto::SymbolIndexMemberKind) -> MemberSummaryKind {
    use proto::SymbolIndexMemberKind as Kind;
    match kind {
        Kind::SymbolIndexMemberField => MemberSummaryKind::Field,
        Kind::SymbolIndexMemberFunctionPointer => MemberSummaryKind::FunctionPointer,
        Kind::SymbolIndexMemberMethod => MemberSummaryKind::Method,
        Kind::SymbolIndexMemberAnonymousAggregate => MemberSummaryKind::AnonymousAggregate,
        Kind::SymbolIndexMemberNestedType => MemberSummaryKind::NestedType,
        Kind::SymbolIndexMemberUnparsed => MemberSummaryKind::Unparsed,
    }
}

fn confidence_to_proto(confidence: Confidence) -> proto::SymbolIndexConfidence {
    use proto::SymbolIndexConfidence as Proto;
    match confidence {
        Confidence::Declaration => Proto::Declaration,
        Confidence::Local => Proto::Local,
        Confidence::Parameter => Proto::Parameter,
        Confidence::FileScope => Proto::FileScope,
        Confidence::NameOnly => Proto::NameOnly,
    }
}

fn confidence_from_proto(confidence: proto::SymbolIndexConfidence) -> Confidence {
    use proto::SymbolIndexConfidence as Proto;
    match confidence {
        Proto::Declaration => Confidence::Declaration,
        Proto::Local => Confidence::Local,
        Proto::Parameter => Confidence::Parameter,
        Proto::FileScope => Confidence::FileScope,
        Proto::NameOnly => Confidence::NameOnly,
    }
}

fn root_origin_to_proto(origin: Option<ChainRootOrigin>) -> proto::SymbolIndexChainRootOrigin {
    use proto::SymbolIndexChainRootOrigin as Proto;
    match origin {
        None => Proto::Unspecified,
        Some(ChainRootOrigin::InFile) => Proto::InFile,
        Some(ChainRootOrigin::Global) => Proto::Global,
        Some(ChainRootOrigin::Call) => Proto::Call,
    }
}

fn root_origin_from_proto(origin: proto::SymbolIndexChainRootOrigin) -> Option<ChainRootOrigin> {
    use proto::SymbolIndexChainRootOrigin as Proto;
    match origin {
        Proto::Unspecified => None,
        Proto::InFile => Some(ChainRootOrigin::InFile),
        Proto::Global => Some(ChainRootOrigin::Global),
        Proto::Call => Some(ChainRootOrigin::Call),
    }
}

fn project_path_to_location(
    path: &ProjectPath,
    range: &Range<Point>,
) -> proto::SymbolIndexLocation {
    location_to_proto(path, range)
}

fn type_ref_to_proto(type_ref: &TypeRef) -> proto::SymbolIndexTypeRef {
    proto::SymbolIndexTypeRef {
        worktree_id: type_ref.path.worktree_id.to_proto(),
        path: type_ref.path.path.as_unix_str().to_string(),
        name: type_ref.name.clone(),
        row: type_ref.position.row,
        column: type_ref.position.column,
    }
}

fn type_ref_from_proto(type_ref: proto::SymbolIndexTypeRef) -> Option<TypeRef> {
    Some(TypeRef {
        path: ProjectPath {
            worktree_id: WorktreeId::from_proto(type_ref.worktree_id),
            path: RelPath::from_unix_str(&type_ref.path).log_err()?.into_arc(),
        },
        name: type_ref.name,
        position: Point::new(type_ref.row, type_ref.column),
    })
}

fn summary_to_proto(summary: &TypeSummary) -> proto::SymbolIndexTypeSummary {
    proto::SymbolIndexTypeSummary {
        name: summary.name.clone(),
        kind: type_kind_to_proto(summary.kind) as i32,
        location: Some(project_path_to_location(&summary.path, &summary.range)),
        type_ref: summary.type_ref.as_ref().map(type_ref_to_proto),
        typedef_chain: summary
            .typedef_chain
            .iter()
            .map(|step| proto::SymbolIndexTypedefStep {
                name: step.name.clone(),
                location: Some(project_path_to_location(&step.path, &step.range)),
                target: step.target.clone(),
            })
            .collect(),
        condition: summary.condition.clone(),
        alias: summary.alias.clone(),
        base_classes: summary.base_classes.clone(),
        members: summary.members.iter().map(member_to_proto).collect(),
        enumerators: summary
            .enumerators
            .iter()
            .map(|enumerator| proto::SymbolIndexEnumeratorSummary {
                name: enumerator.name.clone(),
                value_text: enumerator.value_text.clone(),
                value: enumerator.value,
                value_is_inferred: enumerator.value_is_inferred,
                range: Some(range_to_proto(&enumerator.range)),
                comment: enumerator.comment.clone(),
                condition: enumerator.condition.clone(),
            })
            .collect(),
        members_read: summary.members_read,
        truncated: summary.truncated,
        is_forward_declaration: summary.is_forward_declaration,
    }
}

/// Bodies defined in place are nested no deeper than this on the way in either.
fn summary_from_proto(summary: proto::SymbolIndexTypeSummary, depth: usize) -> Option<TypeSummary> {
    if depth > MAX_INLINE_DEPTH {
        return None;
    }
    let kind = type_kind_from_proto(summary.kind());
    let (path, range) = location_from_proto(summary.location)?;
    let mut typedef_chain = Vec::new();
    for step in summary.typedef_chain {
        let (step_path, step_range) = location_from_proto(step.location)?;
        typedef_chain.push(TypedefStepSummary {
            name: step.name,
            path: step_path,
            range: step_range,
            target: step.target,
        });
    }
    Some(TypeSummary {
        name: summary.name,
        kind,
        path,
        range,
        type_ref: summary.type_ref.and_then(type_ref_from_proto),
        typedef_chain,
        condition: summary.condition,
        alias: summary.alias,
        base_classes: summary.base_classes,
        members: summary
            .members
            .into_iter()
            .filter_map(|member| member_from_proto(member, depth))
            .take(MAX_TYPE_MEMBERS)
            .collect(),
        enumerators: summary
            .enumerators
            .into_iter()
            .filter_map(|enumerator| {
                Some(EnumeratorSummary {
                    name: enumerator.name,
                    value_text: enumerator.value_text,
                    value: enumerator.value,
                    value_is_inferred: enumerator.value_is_inferred,
                    range: range_from_proto(enumerator.range)?,
                    comment: enumerator.comment,
                    condition: enumerator.condition,
                })
            })
            .take(MAX_TYPE_MEMBERS)
            .collect(),
        members_read: summary.members_read,
        truncated: summary.truncated,
        is_forward_declaration: summary.is_forward_declaration,
    })
}

fn member_to_proto(member: &MemberSummary) -> proto::SymbolIndexMemberSummary {
    proto::SymbolIndexMemberSummary {
        name: member.name.clone(),
        kind: member_kind_to_proto(member.kind) as i32,
        text: member.text.clone(),
        type_text: member.type_text.clone(),
        core_name: member.core_name.clone(),
        tag: tag_to_proto(member.tag) as i32,
        pointer_depth: member.pointer_depth,
        array: member.array.clone(),
        bit_width: member.bit_width.clone(),
        is_function_pointer: member.is_function_pointer,
        signature: member.signature.clone(),
        comment: member.comment.clone(),
        condition: member.condition.clone(),
        range: Some(range_to_proto(&member.range)),
        expandable: member.expandable,
        unparsed_text: member.unparsed_text.clone(),
        index_path: member.index_path.clone(),
        inline: member.inline.as_deref().map(summary_to_proto),
    }
}

fn member_from_proto(
    member: proto::SymbolIndexMemberSummary,
    depth: usize,
) -> Option<MemberSummary> {
    let kind = member_kind_from_proto(member.kind());
    let tag = tag_from_proto(member.tag());
    Some(MemberSummary {
        name: member.name,
        kind,
        text: member.text,
        type_text: member.type_text,
        core_name: member.core_name,
        tag,
        pointer_depth: member.pointer_depth,
        array: member.array,
        bit_width: member.bit_width,
        is_function_pointer: member.is_function_pointer,
        signature: member.signature,
        comment: member.comment,
        condition: member.condition,
        range: range_from_proto(member.range)?,
        expandable: member.expandable,
        unparsed_text: member.unparsed_text,
        index_path: member.index_path,
        inline: member
            .inline
            .and_then(|inline| summary_from_proto(inline, depth + 1))
            .map(Box::new),
    })
}

fn resolved_types_to_proto(types: &ResolvedTypes) -> proto::SymbolIndexResolvedTypes {
    proto::SymbolIndexResolvedTypes {
        types: types.types.iter().map(summary_to_proto).collect(),
        total: types.total as u64,
        truncated: types.truncated,
    }
}

fn resolved_types_from_proto(types: proto::SymbolIndexResolvedTypes) -> ResolvedTypes {
    let listed = types.types.len();
    let summaries = types
        .types
        .into_iter()
        .filter_map(|summary| summary_from_proto(summary, 0))
        .take(MAX_TYPE_CANDIDATES)
        .collect::<Vec<_>>();
    ResolvedTypes {
        total: usize::try_from(types.total)
            .unwrap_or(usize::MAX)
            .saturating_sub(listed - summaries.len())
            .max(summaries.len()),
        truncated: types.truncated || summaries.len() < listed,
        types: summaries,
    }
}

fn hover_to_proto(hover: &TypeHover) -> proto::SymbolIndexTypeHover {
    use proto::symbol_index_type_hover::Subject;
    let subject = match &hover.subject {
        TypeHoverSubject::TypeName { name, tag } => {
            Subject::TypeName(proto::SymbolIndexHoverTypeName {
                name: name.clone(),
                tag: tag_to_proto(*tag) as i32,
            })
        }
        TypeHoverSubject::Variable {
            type_text,
            core_name,
            confidence,
            declaration,
        } => Subject::Variable(proto::SymbolIndexHoverVariable {
            type_text: type_text.clone(),
            core_name: core_name.clone(),
            confidence: confidence_to_proto(*confidence) as i32,
            declaration: declaration.as_ref().map(range_to_proto),
        }),
        TypeHoverSubject::Member {
            root_type,
            steps,
            complete,
            root_origin,
            root_name,
        } => Subject::Member(proto::SymbolIndexHoverMember {
            root_type: root_type.clone(),
            root_origin: root_origin_to_proto(*root_origin) as i32,
            root_name: root_name.clone(),
            steps: steps
                .iter()
                .map(|step| proto::SymbolIndexChainStep {
                    name: step.name.clone(),
                    owner: step
                        .owner
                        .as_ref()
                        .map(|owner| proto::SymbolIndexChainOwner {
                            display_name: owner.display_name.clone(),
                            location: owner
                                .path
                                .as_ref()
                                .map(|path| project_path_to_location(path, &owner.range)),
                            range: Some(range_to_proto(&owner.range)),
                            type_ref: owner.type_ref.as_ref().map(type_ref_to_proto),
                            candidates: owner.candidates as u64,
                        }),
                    member: step.member.as_ref().map(member_to_proto),
                })
                .collect(),
            complete: *complete,
        }),
        TypeHoverSubject::Macro { macros } => Subject::MacroText(proto::SymbolIndexHoverMacro {
            macro_definitions: macros
                .iter()
                .map(|found| proto::SymbolIndexMacroSummary {
                    name: found.name.clone(),
                    location: Some(project_path_to_location(&found.path, &found.range)),
                    is_function_like: found.is_function_like,
                    text: found.text.clone(),
                    truncated: found.truncated,
                })
                .collect(),
        }),
    };
    proto::SymbolIndexTypeHover {
        name: hover.name.clone(),
        range: Some(range_to_proto(&hover.range)),
        subject: Some(subject),
        types: Some(resolved_types_to_proto(&hover.types)),
    }
}

fn hover_from_proto(hover: proto::SymbolIndexTypeHover) -> Option<TypeHover> {
    use proto::symbol_index_type_hover::Subject;
    let subject = match hover.subject? {
        Subject::TypeName(type_name) => TypeHoverSubject::TypeName {
            tag: tag_from_proto(type_name.tag()),
            name: type_name.name,
        },
        Subject::Variable(variable) => TypeHoverSubject::Variable {
            confidence: confidence_from_proto(variable.confidence()),
            type_text: variable.type_text,
            core_name: variable.core_name,
            declaration: range_from_proto(variable.declaration),
        },
        Subject::Member(member) => TypeHoverSubject::Member {
            root_origin: root_origin_from_proto(member.root_origin()),
            root_name: member.root_name.clone(),
            root_type: member.root_type,
            steps: member
                .steps
                .into_iter()
                .map(|step| ChainStepSummary {
                    name: step.name,
                    owner: step.owner.and_then(|owner| {
                        Some(ChainOwnerSummary {
                            display_name: owner.display_name,
                            path: owner
                                .location
                                .and_then(|location| location_from_proto(Some(location)))
                                .map(|(path, _)| path),
                            range: range_from_proto(owner.range)?,
                            type_ref: owner.type_ref.and_then(type_ref_from_proto),
                            candidates: usize::try_from(owner.candidates).unwrap_or(usize::MAX),
                        })
                    }),
                    member: step.member.and_then(|member| member_from_proto(member, 0)),
                })
                .collect(),
            complete: member.complete,
        },
        Subject::MacroText(macros) => TypeHoverSubject::Macro {
            macros: macros
                .macro_definitions
                .into_iter()
                .filter_map(|found| {
                    let (path, range) = location_from_proto(found.location)?;
                    Some(MacroSummary {
                        name: found.name,
                        path,
                        range,
                        is_function_like: found.is_function_like,
                        text: found.text,
                        truncated: found.truncated,
                    })
                })
                .collect(),
        },
    };
    Some(TypeHover {
        name: hover.name,
        range: range_from_proto(hover.range)?,
        subject,
        types: hover
            .types
            .map(resolved_types_from_proto)
            .unwrap_or_default(),
    })
}

fn expansion_to_proto(expansion: &MemberExpansion) -> proto::SymbolIndexExpandMemberResponse {
    use proto::SymbolIndexExpansionKind as Kind;
    let mut response = proto::SymbolIndexExpandMemberResponse {
        member: expansion.member.as_ref().map(member_to_proto),
        ..Default::default()
    };
    match &expansion.target {
        MemberExpansionTarget::NotFound => response.set_kind(Kind::SymbolIndexExpansionNotFound),
        MemberExpansionTarget::NotExpandable => {
            response.set_kind(Kind::SymbolIndexExpansionNotExpandable)
        }
        MemberExpansionTarget::Inline(inline) => {
            response.set_kind(Kind::SymbolIndexExpansionInline);
            response.inline = Some(summary_to_proto(inline));
        }
        MemberExpansionTarget::Types(types) => {
            response.set_kind(Kind::SymbolIndexExpansionTypes);
            response.types = Some(resolved_types_to_proto(types));
        }
    }
    response
}

fn expansion_from_proto(response: proto::SymbolIndexExpandMemberResponse) -> MemberExpansion {
    use proto::SymbolIndexExpansionKind as Kind;
    let target = match response.kind() {
        Kind::SymbolIndexExpansionNotFound => MemberExpansionTarget::NotFound,
        Kind::SymbolIndexExpansionNotExpandable => MemberExpansionTarget::NotExpandable,
        Kind::SymbolIndexExpansionInline => response
            .inline
            .clone()
            .and_then(|inline| summary_from_proto(inline, 0))
            .map_or(MemberExpansionTarget::NotFound, |inline| {
                MemberExpansionTarget::Inline(Box::new(inline))
            }),
        Kind::SymbolIndexExpansionTypes => MemberExpansionTarget::Types(
            response
                .types
                .clone()
                .map(resolved_types_from_proto)
                .unwrap_or_default(),
        ),
    };
    MemberExpansion {
        member: response
            .member
            .and_then(|member| member_from_proto(member, 0)),
        target,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_window_rows_keep_the_cursor_and_favor_the_lines_above() {
        let lengths = [10usize; 100];
        let line_len = |row: u32| lengths.get(row as usize).copied().unwrap_or(0);
        // 11 bytes a line: 10 lines fit in 110 bytes, 7 of them (77 <= 82) above or at the
        // cursor first.
        let (start, end) = window_rows(0, 99, 50, 110, line_len).expect("the cursor line fits");
        assert!(start <= 50 && 50 <= end);
        assert_eq!(end - start + 1, 10);
        assert!(
            50 - start > end - 50,
            "more lines above than below: {start}..={end}"
        );
        assert_eq!(
            window_rows(0, 99, 50, 5, line_len),
            None,
            "the cursor line alone is too long"
        );
        assert_eq!(
            window_rows(60, 99, 50, 1000, line_len),
            None,
            "outside the rows"
        );
        assert_eq!(window_rows(40, 55, 50, 100_000, line_len), Some((40, 55)));
    }

    #[test]
    fn test_a_long_window_is_cut_around_the_cursor_and_keeps_its_rows() {
        let line = "x".repeat(99);
        let window = std::iter::repeat_n(line.as_str(), 2000)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(window.len() > MAX_TYPE_WINDOW_BYTES);
        let clipped = clip_window(&window, 1500).expect("the cursor line fits");
        assert!(clipped.len() <= MAX_TYPE_WINDOW_BYTES + 2000);
        let lines = clipped.split('\n').collect::<Vec<_>>();
        assert_eq!(
            lines.get(1500),
            Some(&line.as_str()),
            "row 1500 is still row 1500"
        );
        assert_eq!(lines.first(), Some(&""), "rows far above are emptied");
        let kept = lines.iter().filter(|line| !line.is_empty()).count();
        assert!(kept * 100 <= MAX_TYPE_WINDOW_BYTES, "{kept} lines kept");
    }

    #[test]
    fn test_type_name_of_text_takes_the_core_name() {
        assert_eq!(
            type_name_of_text("const struct file_operations *", SourceLanguage::C),
            Some(("file_operations".to_string(), Some(TagKind::Struct)))
        );
        assert_eq!(
            type_name_of_text("foo_t [4]", SourceLanguage::C),
            Some(("foo_t".to_string(), None))
        );
        assert_eq!(type_name_of_text("unsigned long", SourceLanguage::C), None);
        assert_eq!(type_name_of_text("int (*)(int)", SourceLanguage::C), None);
    }

    #[test]
    fn test_clip_text_cuts_by_characters() {
        assert_eq!(clip_text("abc", 3), "abc");
        assert_eq!(clip_text("日本語テキスト", 3), "日本語…");
    }
}
