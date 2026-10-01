//! Reads the layout of C and C++ type definitions (members, enumerators, typedef targets) from
//! source text, and guesses the type of the name under a cursor.
//!
//! Type bodies are read with a small lexical declaration parser rather than the tree-sitter
//! grammar: annotation macros (`__rcu`, `__user`, `_In_`, `DECLSPEC_ALIGN(16)`) and members
//! declared through macros (`DECLARE_BITMAP(bits, 64);`) turn into ERROR nodes that swallow
//! neighbouring members, while a declaration-at-a-time parser keeps every other member and
//! reports the one it cannot read. Only the definition's own range is read, so a large header
//! is not parsed as a whole. Finding what the cursor is on needs scopes and expressions, which
//! the grammar handles well, so [`type_at`] parses the file with tree-sitter.

use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    ops::ControlFlow,
    time::Instant,
};

use tree_sitter::{Language, Node, ParseOptions, ParseState, Parser, Point, Tree};

use crate::extract::{Position, Range, SourceLanguage};

/// Definitions longer than this are read up to the limit and marked truncated.
pub const MAX_DEFINITION_BYTES: usize = 1024 * 1024;
/// Anonymous aggregates nested deeper than this are kept as unparsed members.
pub const MAX_NESTING_DEPTH: usize = 12;
/// Members (and enumerators) read from one definition, counting nested ones.
pub const MAX_MEMBERS: usize = 4096;
/// Member comments are cut to this many characters.
pub const MAX_COMMENT_CHARS: usize = 160;
/// [`type_at`] does not parse larger files.
pub const MAX_TYPE_AT_SOURCE_BYTES: usize = 16 * 1024 * 1024;
/// [`type_at`] gives up on a parse that takes longer.
const TYPE_AT_PARSE_TIMEOUT_MILLIS: u128 = 1500;
/// Bytes read past the end of an index range, for `} name_t, *pname_t;` after a struct body.
const RANGE_SLACK_BYTES: usize = 4096;
const MAX_DECLARATOR_DEPTH: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TagKind {
    Struct,
    Union,
    Enum,
    Class,
}

impl TagKind {
    pub fn keyword(self) -> &'static str {
        match self {
            TagKind::Struct => "struct",
            TagKind::Union => "union",
            TagKind::Enum => "enum",
            TagKind::Class => "class",
        }
    }

    fn from_keyword(word: &[u8], language: SourceLanguage) -> Option<Self> {
        match word {
            b"struct" => Some(TagKind::Struct),
            b"union" => Some(TagKind::Union),
            b"enum" => Some(TagKind::Enum),
            b"class" if language == SourceLanguage::Cpp => Some(TagKind::Class),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TypeKind {
    Struct,
    Union,
    Enum,
    Class,
    /// A typedef whose target is not defined in the same declaration (`typedef struct foo
    /// foo_t;`, `typedef unsigned long size_t;`); see [`TypeLayout::alias`].
    Typedef,
}

impl From<TagKind> for TypeKind {
    fn from(tag: TagKind) -> Self {
        match tag {
            TagKind::Struct => TypeKind::Struct,
            TagKind::Union => TypeKind::Union,
            TagKind::Enum => TypeKind::Enum,
            TagKind::Class => TypeKind::Class,
        }
    }
}

/// A type as written in a declaration, with the parts needed to look it up and show it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TypeText {
    /// The declaration without the declared name, whitespace collapsed and attributes such as
    /// `__attribute__((packed))` removed: `const struct file_operations *`, `char [16]`,
    /// `int (*)(struct inode *, struct file *)`. Annotation macros (`__rcu`) are kept.
    pub text: String,
    /// The name to look up for the next level: the tag name of `struct foo`, a typedef name
    /// (`u32`, `foo_t`), or the builtin type (`unsigned long`). `None` for an anonymous
    /// aggregate, a function type and `typeof(...)`.
    pub core_name: Option<String>,
    pub tag: Option<TagKind>,
    pub is_builtin: bool,
    /// Pointer levels; for a function pointer, the levels in front of the function.
    pub pointer_depth: u32,
    /// C++ `&` or `&&`.
    pub is_reference: bool,
    /// Dimension expressions as written, outermost first; an empty string for `[]`.
    pub array_dimensions: Vec<String>,
    /// Qualifiers and annotation macros in front of or inside the declarator: `const`,
    /// `volatile`, `__rcu`, `__user`.
    pub qualifiers: Vec<String>,
    /// A pointer to a function (or an array of them).
    pub is_function_pointer: bool,
    /// A function type: a method or prototype declared as a member.
    pub is_function: bool,
}

impl TypeText {
    /// Whether looking the core name up can lead to members.
    pub fn is_expandable(&self) -> bool {
        self.core_name.is_some()
            && !self.is_builtin
            && !self.is_function
            && !self.is_function_pointer
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberKind {
    Field,
    FunctionPointer,
    /// A C++ member function.
    Method,
    /// A C11 anonymous struct or union, whose members are reached directly.
    AnonymousAggregate,
    /// A named struct, union or enum defined inside the body without declaring a member.
    NestedType,
    /// A declaration that could not be read (usually a macro); see [`Member::text`].
    Unparsed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub name: Option<String>,
    pub kind: MemberKind,
    pub ty: TypeText,
    /// This member's declaration alone, whitespace collapsed:
    /// `const struct file_operations *f_op`, `unsigned int flags : 3`, `DECLARE_BITMAP(bits, 64)`.
    pub text: String,
    pub bit_width: Option<String>,
    /// The body of a struct, union or enum defined in place (`struct { ... } u;`).
    pub nested: Option<Box<TypeLayout>>,
    pub name_range: Option<Range>,
    /// The whole declaration statement.
    pub range: Range,
    /// The first line of the comment in front of the member, or else of the comment after it on
    /// the same line.
    pub comment: Option<String>,
    /// The `#if` conditions the member is under, inside the body, as written:
    /// `CONFIG_SMP && !CONFIG_PREEMPT`. Conditions are not evaluated; all branches are read.
    pub condition: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Enumerator {
    pub name: String,
    /// The value expression as written.
    pub value_text: Option<String>,
    /// The value, when written as an integer literal or counted from one.
    pub value: Option<i128>,
    /// `value` was counted from the previous enumerator rather than written.
    pub value_is_inferred: bool,
    pub name_range: Range,
    pub comment: Option<String>,
    pub condition: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypedefName {
    pub name: String,
    /// `1` for `*PFOO` in `typedef struct _FOO { ... } FOO, *PFOO;`.
    pub pointer_depth: u32,
    pub name_range: Range,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeLayout {
    pub kind: TypeKind,
    pub language: SourceLanguage,
    /// `foo` in `struct foo`.
    pub tag_name: Option<String>,
    /// Names declared by a `typedef` around the definition.
    pub typedef_names: Vec<TypedefName>,
    /// From the first token (`typedef` or the tag keyword) to the terminating `;` (or the
    /// closing brace of a nested body).
    pub range: Range,
    /// The tag name, or else the first typedef name.
    pub name_range: Option<Range>,
    /// `struct foo;` without a body.
    pub is_forward_declaration: bool,
    pub members: Vec<Member>,
    pub enumerators: Vec<Enumerator>,
    /// What a [`TypeKind::Typedef`] names.
    pub alias: Option<TypeText>,
    /// C++ base classes as written, without access specifiers: `Base`, `ns::Mixin<int>`.
    pub base_classes: Vec<String>,
    /// The definition was longer than a limit and not everything was read.
    pub truncated: bool,
}

impl TypeLayout {
    /// `struct foo`, `foo_t` for a typedef of an anonymous struct, `union (anonymous)`.
    pub fn display_name(&self) -> String {
        match (&self.tag_name, self.typedef_names.first()) {
            (Some(tag), _) if self.kind != TypeKind::Typedef => {
                format!("{} {tag}", self.kind_keyword())
            }
            (_, Some(typedef)) => typedef.name.clone(),
            (Some(tag), None) => tag.clone(),
            (None, None) => format!("{} (anonymous)", self.kind_keyword()),
        }
    }

    pub fn kind_keyword(&self) -> &'static str {
        match self.kind {
            TypeKind::Struct => "struct",
            TypeKind::Union => "union",
            TypeKind::Enum => "enum",
            TypeKind::Class => "class",
            TypeKind::Typedef => "typedef",
        }
    }

    /// Whether the definition carries members or enumerators rather than naming another type.
    pub fn has_body(&self) -> bool {
        !self.is_forward_declaration && self.kind != TypeKind::Typedef
    }

    /// Finds a member by name, looking into anonymous structs and unions the way C does.
    pub fn find_member(&self, name: &str) -> Option<&Member> {
        find_member_in(&self.members, name, 0)
    }
}

fn find_member_in<'a>(members: &'a [Member], name: &str, depth: usize) -> Option<&'a Member> {
    if depth > MAX_NESTING_DEPTH {
        return None;
    }
    if let Some(member) = members
        .iter()
        .find(|member| member.name.as_deref() == Some(name))
    {
        return Some(member);
    }
    members
        .iter()
        .filter(|member| member.kind == MemberKind::AnonymousAggregate)
        .filter_map(|member| member.nested.as_deref())
        .find_map(|nested| find_member_in(&nested.members, name, depth + 1))
}

/// Members and enumerators in `layout`, counting nested bodies.
fn layout_size(layout: &TypeLayout) -> usize {
    layout.enumerators.len().max(1)
        + layout
            .members
            .iter()
            .map(|member| 1 + member.nested.as_deref().map_or(0, layout_size))
            .sum::<usize>()
}

/// Where the definition to read is.
#[derive(Clone, Copy, Debug)]
pub enum TypeLocator<'a> {
    /// The range the index recorded for a struct, union, enum, class or typedef definition.
    /// A `typedef` just in front of a struct range is included.
    Range(Range),
    /// Search the source for a definition of this name; a definition with a body is preferred
    /// over a forward declaration.
    Name { name: &'a str, tag: Option<TagKind> },
}

/// Reads a type definition from `source`.
pub fn parse_type_definition(
    source: &str,
    locator: TypeLocator,
    language: SourceLanguage,
) -> Option<TypeLayout> {
    parse_type_definition_bytes(source.as_bytes(), locator, language)
}

/// [`parse_type_definition`] on bytes; files that are not valid UTF-8 keep their offsets.
pub fn parse_type_definition_bytes(
    source: &[u8],
    locator: TypeLocator,
    language: SourceLanguage,
) -> Option<TypeLayout> {
    if u32::try_from(source.len()).is_err() {
        return None;
    }
    match locator {
        TypeLocator::Range(range) => parse_at_range(source, range, language),
        TypeLocator::Name { name, tag } => parse_named(source, name, tag, language),
    }
}

/// Reads a type as written on its own (`const struct foo *`, `int (*)(int)`), such as the type a
/// language server reports for a variable.
pub fn parse_type_text(text: &str, language: SourceLanguage) -> Option<TypeText> {
    let source = text.as_bytes();
    let lexed = lex(source, 0, source.len(), Position::default(), true);
    let context = Context::new(source, language, &lexed);
    let tokens = context.strip_attributes(&lexed.tokens);
    let declaration = context.parse_declaration(&tokens)?;
    match declaration.declarators.first() {
        Some(declarator) => Some(context.type_text(&tokens, &declaration, declarator)),
        None => Some(context.specifier_type_text(&tokens, &declaration)),
    }
}

/// A declared name and its type, from a declaration statement such as a variable, parameter or
/// function prototype.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclaredName {
    pub name: String,
    pub ty: TypeText,
    /// For a function, what it returns.
    pub return_type: Option<TypeText>,
    pub name_range: Range,
}

/// Reads the names declared by one declaration (`struct foo *a, b[4] = {0};`,
/// `static int f(void)`, a parameter). Positions are within `text`.
pub fn parse_declaration_text(text: &str, language: SourceLanguage) -> Vec<DeclaredName> {
    declared_names_in_file(text.as_bytes(), Position::default(), text.len(), language)
}

fn parse_at_range(source: &[u8], range: Range, language: SourceLanguage) -> Option<TypeLayout> {
    let start_byte = usize::try_from(range.start.byte).ok()?;
    if start_byte >= source.len() {
        return None;
    }
    let mut start = checked_position(source, range.start);
    // `typedef struct foo { ... } foo_t;`: the index records the struct from its keyword.
    if let Some(typedef_start) = preceding_typedef(source, start_byte) {
        start = position_before(source, start, typedef_start);
    }
    let start_byte = start.byte as usize;
    let hinted_end = usize::try_from(range.end.byte)
        .unwrap_or(usize::MAX)
        .saturating_add(RANGE_SLACK_BYTES);
    let limit = source
        .len()
        .min(start_byte.saturating_add(MAX_DEFINITION_BYTES));
    let mut window_end = hinted_end
        .max(start_byte.saturating_add(RANGE_SLACK_BYTES))
        .min(limit);
    loop {
        let lexed = lex(source, start_byte, window_end, start, false);
        let context = Context::new(source, language, &lexed);
        match context.parse_type_statement(&lexed.tokens, 0) {
            (Some(mut layout), terminated) => {
                if terminated || window_end >= limit {
                    layout.truncated |= !terminated && window_end < source.len();
                    return Some(layout);
                }
            }
            // A whole statement that is not a type definition does not become one with more
            // text after it.
            (None, true) => return None,
            (None, false) if window_end >= limit => return None,
            (None, false) => {}
        }
        window_end = window_end
            .saturating_mul(2)
            .saturating_sub(start_byte)
            .min(limit);
        if window_end <= start_byte {
            return None;
        }
    }
}

fn parse_named(
    source: &[u8],
    name: &str,
    tag: Option<TagKind>,
    language: SourceLanguage,
) -> Option<TypeLayout> {
    let end = source.len().min(MAX_TYPE_AT_SOURCE_BYTES);
    let searched = source.get(..end).unwrap_or_default();
    if name.is_empty()
        || !searched
            .windows(name.len())
            .any(|window| window == name.as_bytes())
    {
        return None;
    }
    let lexed = lex(source, 0, end, Position::default(), true);
    let context = Context::new(source, language, &lexed);
    let tokens = &lexed.tokens;
    let mut best: Option<(u8, TypeLayout)> = None;
    for (index, token) in tokens.iter().enumerate() {
        if context.exhausted() {
            break;
        }
        let word = context.bytes(token);
        let is_typedef = word == b"typedef";
        let tag_here = TagKind::from_keyword(word, language);
        if !is_typedef && tag_here.is_none() {
            continue;
        }
        if tag_here.is_some() && index > 0 {
            // Part of a typedef that is handled from its `typedef` token.
            if tokens
                .get(index - 1)
                .is_some_and(|previous| context.bytes(previous) == b"typedef")
            {
                continue;
            }
        }
        if tag_here.is_some() && !context.names_tag(tokens, index, name) {
            continue;
        }
        let Some(rest) = tokens.get(index..) else {
            continue;
        };
        let (layout, terminated) = context.parse_type_statement(rest, 0);
        // A statement without an end runs to the end of the file, and so would every later
        // one: reading each of them again makes the search quadratic.
        let stop = !terminated;
        let Some(layout) = layout else {
            if stop {
                break;
            }
            continue;
        };
        let tag_matches = layout.tag_name.as_deref() == Some(name)
            && tag.is_none_or(|tag| tags_compatible(tag, layout.kind, language));
        let typedef_matches = tag.is_none()
            && layout
                .typedef_names
                .iter()
                .any(|typedef| typedef.name == name);
        if !tag_matches && !typedef_matches {
            continue;
        }
        let rank = if layout.is_forward_declaration {
            2
        } else if layout.kind == TypeKind::Typedef {
            1
        } else {
            0
        };
        if best.as_ref().is_none_or(|(best_rank, _)| rank < *best_rank) {
            best = Some((rank, layout));
        }
        if stop {
            break;
        }
    }
    best.map(|(_, mut layout)| {
        alias_for_name(source, &mut layout, name);
        layout
    })
}

/// `typedef struct a *a_ptr, a_val;` names two different types; [`TypeLayout::alias`] is read
/// from the first declarator, so for another name it is read again from that name's own one.
pub(crate) fn alias_for_name(source: &[u8], layout: &mut TypeLayout, name: &str) {
    if layout.kind != TypeKind::Typedef
        || layout
            .typedef_names
            .first()
            .is_none_or(|first| first.name == name)
    {
        return;
    }
    if let Some(declared) = declared_names_in_file(
        source,
        layout.range.start,
        layout.range.end.byte as usize,
        layout.language,
    )
    .into_iter()
    .find(|declared| declared.name == name)
    {
        layout.alias = Some(declared.ty);
    }
}

/// Whether a type of `kind` can be what `tag` refers to.
pub(crate) fn tags_compatible(tag: TagKind, kind: TypeKind, language: SourceLanguage) -> bool {
    match (tag, kind) {
        (TagKind::Struct, TypeKind::Struct)
        | (TagKind::Union, TypeKind::Union)
        | (TagKind::Enum, TypeKind::Enum)
        | (TagKind::Class, TypeKind::Class) => true,
        (TagKind::Struct, TypeKind::Class) | (TagKind::Class, TypeKind::Struct) => {
            language == SourceLanguage::Cpp
        }
        _ => false,
    }
}

/// The start of a `typedef` keyword that directly precedes `byte`, across whitespace only.
fn preceding_typedef(source: &[u8], byte: usize) -> Option<usize> {
    let before = source.get(..byte)?;
    let end = before
        .iter()
        .rposition(|character| !character.is_ascii_whitespace())?
        + 1;
    let word_start = before
        .get(..end)?
        .iter()
        .rposition(|&character| !is_identifier_byte(character))
        .map_or(0, |position| position + 1);
    (before.get(word_start..end)? == b"typedef").then_some(word_start)
}

/// Returns `position` if its column is consistent with `source`, or recomputes it.
fn checked_position(source: &[u8], position: Position) -> Position {
    let byte = position.byte as usize;
    let column = position.column as usize;
    let consistent = byte.checked_sub(column).is_some_and(|line_start| {
        source
            .get(line_start..byte)
            .is_some_and(|line| !line.contains(&b'\n'))
            && (line_start == 0 || source.get(line_start - 1) == Some(&b'\n'))
    });
    if consistent {
        position
    } else {
        position_of_byte(source, byte)
    }
}

/// The position of `byte`, which lies before `anchor`, computed by scanning back from it.
fn position_before(source: &[u8], anchor: Position, byte: usize) -> Position {
    let anchor_byte = anchor.byte as usize;
    let newlines = source
        .get(byte..anchor_byte)
        .map_or(0, |between| between.iter().filter(|&&c| c == b'\n').count());
    let line_start = source
        .get(..byte)
        .and_then(|before| before.iter().rposition(|&c| c == b'\n'))
        .map_or(0, |position| position + 1);
    Position {
        byte: to_u32(byte),
        row: anchor.row.saturating_sub(to_u32(newlines)),
        column: to_u32(byte - line_start),
    }
}

/// The position of `byte`, counted from the start of `source`.
pub(crate) fn position_of_byte(source: &[u8], byte: usize) -> Position {
    let byte = byte.min(source.len());
    let before = source.get(..byte).unwrap_or_default();
    let row = before.iter().filter(|&&c| c == b'\n').count();
    let line_start = before
        .iter()
        .rposition(|&c| c == b'\n')
        .map_or(0, |position| position + 1);
    Position {
        byte: to_u32(byte),
        row: to_u32(row),
        column: to_u32(byte - line_start),
    }
}

fn to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$' || byte >= 0x80
}

// ---------------------------------------------------------------------------------------------
// Lexer

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TokenKind {
    Identifier,
    Number,
    Literal,
    Punctuation,
    /// A whole preprocessor line, continuations included.
    Directive,
}

#[derive(Clone, Copy, Debug)]
struct Token {
    kind: TokenKind,
    start: Position,
    end: Position,
    /// Whitespace or a comment separates this token from the previous one.
    space_before: bool,
}

impl Token {
    fn range(&self) -> Range {
        Range {
            start: self.start,
            end: self.end,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Comment {
    start: Position,
    end: Position,
    /// Only whitespace precedes the comment on its line.
    starts_line: bool,
}

struct Lexed {
    tokens: Vec<Token>,
    comments: Vec<Comment>,
}

/// Splits `source[start..end]` into tokens. `start_position` is the position of `start`.
/// `line_start` tells whether `start` is at the beginning of a line for directive detection;
/// otherwise the text before `start` on its line decides.
fn lex(
    source: &[u8],
    start: usize,
    end: usize,
    start_position: Position,
    line_start: bool,
) -> Lexed {
    let end = end.min(source.len());
    let mut tokens = Vec::new();
    let mut comments = Vec::new();
    let mut index = start;
    let mut row = start_position.row;
    let mut current_line_start = start.saturating_sub(start_position.column as usize);
    let mut at_line_start = line_start
        || source
            .get(current_line_start..start)
            .is_some_and(|before| before.iter().all(|c| c.is_ascii_whitespace()));
    let mut space = false;
    let position = |index: usize, row: u32, line_start: usize| Position {
        byte: to_u32(index),
        row,
        column: to_u32(index.saturating_sub(line_start)),
    };
    let at = |index: usize| source.get(index).copied().filter(|_| index < end);
    while let Some(byte) = at(index) {
        if byte == b'\n' {
            index += 1;
            row += 1;
            current_line_start = index;
            at_line_start = true;
            space = true;
            continue;
        }
        if byte == b' ' || byte == b'\t' || byte == b'\r' || byte == 0x0b || byte == 0x0c {
            index += 1;
            space = true;
            continue;
        }
        if byte == b'\\' && matches!(at(index + 1), Some(b'\n' | b'\r')) {
            index += 1;
            if at(index) == Some(b'\r') {
                index += 1;
            }
            if at(index) == Some(b'\n') {
                index += 1;
                row += 1;
                current_line_start = index;
            }
            space = true;
            continue;
        }
        if byte == b'/' && at(index + 1) == Some(b'/') {
            let comment_start = position(index, row, current_line_start);
            while let Some(inner) = at(index) {
                if inner == b'\n' {
                    break;
                }
                index += 1;
            }
            let mut comment_end = index;
            if comment_end > comment_start.byte as usize
                && source.get(comment_end - 1) == Some(&b'\r')
            {
                comment_end -= 1;
            }
            comments.push(Comment {
                start: comment_start,
                end: position(comment_end, row, current_line_start),
                starts_line: at_line_start,
            });
            space = true;
            continue;
        }
        if byte == b'/' && at(index + 1) == Some(b'*') {
            let comment_start = position(index, row, current_line_start);
            index += 2;
            loop {
                match at(index) {
                    None => break,
                    Some(b'*') if at(index + 1) == Some(b'/') => {
                        index += 2;
                        break;
                    }
                    Some(b'\n') => {
                        index += 1;
                        row += 1;
                        current_line_start = index;
                    }
                    Some(_) => index += 1,
                }
            }
            comments.push(Comment {
                start: comment_start,
                end: position(index, row, current_line_start),
                starts_line: at_line_start,
            });
            space = true;
            continue;
        }
        let token_start = position(index, row, current_line_start);
        let kind = if byte == b'#' && at_line_start {
            loop {
                match at(index) {
                    None => break,
                    Some(b'\\') if matches!(at(index + 1), Some(b'\n' | b'\r')) => {
                        index += 1;
                        if at(index) == Some(b'\r') {
                            index += 1;
                        }
                        if at(index) == Some(b'\n') {
                            index += 1;
                            row += 1;
                            current_line_start = index;
                        }
                    }
                    Some(b'/') if at(index + 1) == Some(b'*') => {
                        index += 2;
                        loop {
                            match at(index) {
                                None => break,
                                Some(b'*') if at(index + 1) == Some(b'/') => {
                                    index += 2;
                                    break;
                                }
                                Some(b'\n') => {
                                    index += 1;
                                    row += 1;
                                    current_line_start = index;
                                }
                                Some(_) => index += 1,
                            }
                        }
                    }
                    Some(b'\n') => break,
                    Some(_) => index += 1,
                }
            }
            TokenKind::Directive
        } else if is_identifier_byte(byte) && !byte.is_ascii_digit() {
            while at(index).is_some_and(is_identifier_byte) {
                index += 1;
            }
            TokenKind::Identifier
        } else if byte.is_ascii_digit()
            || (byte == b'.' && at(index + 1).is_some_and(|next| next.is_ascii_digit()))
        {
            index += 1;
            while let Some(inner) = at(index) {
                if matches!(inner, b'e' | b'E' | b'p' | b'P')
                    && matches!(at(index + 1), Some(b'+' | b'-'))
                {
                    index += 2;
                } else if is_identifier_byte(inner) || inner == b'.' || inner == b'\'' {
                    index += 1;
                } else {
                    break;
                }
            }
            TokenKind::Number
        } else if byte == b'"' || byte == b'\'' {
            index += 1;
            while let Some(inner) = at(index) {
                if inner == b'\\' {
                    index += 2;
                    continue;
                }
                if inner == b'\n' {
                    break;
                }
                index += 1;
                if inner == byte {
                    break;
                }
            }
            index = index.min(end);
            TokenKind::Literal
        } else {
            let length = punctuation_length(source.get(index..end).unwrap_or_default());
            index += length;
            TokenKind::Punctuation
        };
        let mut token_end = index;
        if kind == TokenKind::Directive
            && token_end > token_start.byte as usize
            && source.get(token_end - 1) == Some(&b'\r')
        {
            token_end -= 1;
        }
        tokens.push(Token {
            kind,
            start: token_start,
            end: position(token_end, row, current_line_start),
            space_before: space,
        });
        at_line_start = false;
        space = false;
    }
    Lexed { tokens, comments }
}

fn punctuation_length(rest: &[u8]) -> usize {
    const LONG: [&[u8]; 17] = [
        b"...", b"->", b"::", b"&&", b"||", b"##", b"==", b"!=", b"<=", b">=", b"++", b"--", b"+=",
        b"-=", b"*=", b"/=", b"|=",
    ];
    for candidate in LONG {
        if rest.starts_with(candidate) {
            return candidate.len();
        }
    }
    // A multi-byte character outside identifiers is still one token.
    match rest.first() {
        Some(&first) if first >= 0x80 => rest
            .iter()
            .skip(1)
            .take_while(|&&byte| byte & 0xC0 == 0x80)
            .count()
            .saturating_add(1),
        _ => 1,
    }
}

// ---------------------------------------------------------------------------------------------
// Word classes

const TYPE_KEYWORDS: &[&[u8]] = &[
    b"void",
    b"char",
    b"short",
    b"int",
    b"long",
    b"float",
    b"double",
    b"signed",
    b"unsigned",
    b"_Bool",
    b"bool",
    b"_Complex",
    b"_Imaginary",
    b"__int8",
    b"__int16",
    b"__int32",
    b"__int64",
    b"__int128",
    b"wchar_t",
    b"char8_t",
    b"char16_t",
    b"char32_t",
    b"__wchar_t",
    b"_Float16",
    b"_Float32",
    b"_Float64",
    b"_Float128",
    b"__float128",
    b"__fp16",
    b"__signed__",
    b"__signed",
    b"__unsigned",
    b"auto",
];

const QUALIFIER_KEYWORDS: &[&[u8]] = &[
    b"const",
    b"volatile",
    b"restrict",
    b"__restrict",
    b"__restrict__",
    b"_Atomic",
    b"__const",
    b"__volatile",
    b"__volatile__",
    b"static",
    b"extern",
    b"register",
    b"inline",
    b"__inline",
    b"__inline__",
    b"_Thread_local",
    b"thread_local",
    b"__thread",
    b"mutable",
    b"constexpr",
    b"consteval",
    b"constinit",
    b"virtual",
    b"explicit",
    b"friend",
    b"_Noreturn",
    b"__extension__",
    b"typename",
];

/// Qualifier keywords that say how something is stored or declared rather than what type it has.
const STORAGE_KEYWORDS: &[&[u8]] = &[
    b"typedef",
    b"static",
    b"extern",
    b"register",
    b"inline",
    b"__inline",
    b"__inline__",
    b"_Thread_local",
    b"thread_local",
    b"__thread",
    b"mutable",
    b"constexpr",
    b"consteval",
    b"constinit",
    b"virtual",
    b"explicit",
    b"friend",
    b"_Noreturn",
    b"__extension__",
];

const STATEMENT_KEYWORDS: &[&[u8]] = &[
    b"return",
    b"if",
    b"else",
    b"while",
    b"for",
    b"do",
    b"switch",
    b"case",
    b"default",
    b"goto",
    b"break",
    b"continue",
    b"sizeof",
    b"_Alignof",
    b"alignof",
    b"delete",
    b"new",
    b"throw",
    b"co_return",
    b"co_await",
    b"co_yield",
];

/// Macros that annotate a declaration without being part of the type name.
const KNOWN_ANNOTATIONS: &[&[u8]] = &[
    b"__rcu",
    b"__user",
    b"__iomem",
    b"__percpu",
    b"__force",
    b"__bitwise",
    b"__nocast",
    b"__kernel",
    b"__safe",
    b"__must_check",
    b"__always_inline",
    b"__init",
    b"__initdata",
    b"__initconst",
    b"__exit",
    b"__exitdata",
    b"__cold",
    b"__hot",
    b"__weak",
    b"__packed",
    b"__randomize_layout",
    b"__no_randomize_layout",
    b"__private",
    b"__noreturn",
    b"__maybe_unused",
    b"__used",
    b"__unused",
    b"__read_mostly",
    b"__ro_after_init",
    b"__cacheline_aligned",
    b"__cacheline_aligned_in_smp",
    b"____cacheline_aligned",
    b"____cacheline_aligned_in_smp",
    b"__deprecated",
    b"__latent_entropy",
    b"__noinline",
    b"noinline",
    b"__visible",
    b"__data_racy",
    b"__nonstring",
    b"__aligned_largest",
    b"__designated_init",
    b"__attribute_const__",
    b"__pure",
    b"__malloc",
    b"asmlinkage",
    b"notrace",
    b"__sched",
    b"__lockfunc",
    b"__tagged",
    b"__rcu_bh",
    b"__ref",
    b"__meminit",
    b"__devinit",
    b"__cpuinit",
    b"__always_unused",
    b"__flatten",
    b"WINAPI",
    b"APIENTRY",
    b"CALLBACK",
    b"NTAPI",
    b"WSAAPI",
    b"PASCAL",
    b"FAR",
    b"NEAR",
    b"STDMETHODCALLTYPE",
    b"STDAPICALLTYPE",
    b"__stdcall",
    b"__cdecl",
    b"__fastcall",
    b"__thiscall",
    b"__vectorcall",
    b"_stdcall",
    b"_cdecl",
    b"__ptr32",
    b"__ptr64",
    b"__unaligned",
    b"UNALIGNED",
    b"UNALIGNED64",
    b"CONST",
    b"POINTER_32",
    b"POINTER_64",
    b"__RPC_FAR",
    b"RPC_FAR",
    b"IN",
    b"OUT",
    b"OPTIONAL",
    b"__far",
    b"__near",
    b"__huge",
    b"_Nonnull",
    b"_Nullable",
    b"_Null_unspecified",
    b"__nonnull",
    b"__nullable",
    b"BEGIN_INTERFACE",
    b"END_INTERFACE",
];

/// Names that take a parenthesized argument and only annotate the declaration.
const ATTRIBUTE_CALLS: &[&[u8]] = &[
    b"__attribute__",
    b"__attribute",
    b"__declspec",
    b"_declspec",
    b"alignas",
    b"_Alignas",
    b"__aligned",
    b"__section",
    b"__printf",
    b"__scanf",
    b"__alloc_size",
    b"__counted_by",
    b"__assume_aligned",
    b"__cleanup",
    b"__acquires",
    b"__releases",
    b"__must_hold",
    b"__guarded_by",
    b"__pt_guarded_by",
    b"DECLSPEC_ALIGN",
    b"__pragma",
    b"_Pragma",
    b"__asm__",
    b"__asm",
    b"asm",
    b"__free",
    b"__cond_acquires",
    b"__counted_by_le",
    b"__counted_by_be",
    b"__sized_by",
    b"__no_sanitize_address",
    b"__diagnose_as",
];

/// Typedefs from the C and POSIX headers, shown like builtin types rather than looked up (the
/// system headers are usually outside the project).
const STANDARD_TYPEDEFS: &[&str] = &[
    "size_t",
    "ssize_t",
    "ptrdiff_t",
    "intptr_t",
    "uintptr_t",
    "intmax_t",
    "uintmax_t",
    "int8_t",
    "int16_t",
    "int32_t",
    "int64_t",
    "uint8_t",
    "uint16_t",
    "uint32_t",
    "uint64_t",
    "int_least8_t",
    "int_least16_t",
    "int_least32_t",
    "int_least64_t",
    "uint_least8_t",
    "uint_least16_t",
    "uint_least32_t",
    "uint_least64_t",
    "int_fast8_t",
    "int_fast16_t",
    "int_fast32_t",
    "int_fast64_t",
    "uint_fast8_t",
    "uint_fast16_t",
    "uint_fast32_t",
    "uint_fast64_t",
    "off_t",
    "time_t",
    "pid_t",
    "uid_t",
    "gid_t",
    "mode_t",
    "va_list",
    "__builtin_va_list",
    "max_align_t",
    "nullptr_t",
    "sig_atomic_t",
    "clock_t",
    "wint_t",
    "FILE",
];

fn is_word(word: &[u8], list: &[&[u8]]) -> bool {
    list.contains(&word)
}

/// SAL annotations: `_In_`, `_Out_writes_bytes_`, `_Field_size_`.
fn is_sal_annotation(word: &[u8]) -> bool {
    word.len() >= 3
        && word.first() == Some(&b'_')
        && word.get(1).is_some_and(|c| c.is_ascii_uppercase())
        && word.last() == Some(&b'_')
}

fn is_known_annotation(word: &[u8]) -> bool {
    is_word(word, KNOWN_ANNOTATIONS) || is_sal_annotation(word)
}

fn is_attribute_call(word: &[u8]) -> bool {
    is_word(word, ATTRIBUTE_CALLS)
        || is_sal_annotation(word)
        || word.starts_with(b"__drv_")
        || word.starts_with(b"_IRQL_")
}

fn is_typeof(word: &[u8]) -> bool {
    matches!(
        word,
        b"typeof"
            | b"__typeof__"
            | b"__typeof"
            | b"typeof_unqual"
            | b"__typeof_unqual__"
            | b"decltype"
    )
}

fn is_all_caps(word: &[u8]) -> bool {
    word.iter().any(|c| c.is_ascii_uppercase())
        && word
            .iter()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'_')
}

// ---------------------------------------------------------------------------------------------
// Declarations

#[derive(Clone, Debug, PartialEq, Eq)]
enum DeclarationOperation {
    Pointer,
    Reference,
    Array(String),
    Function,
}

#[derive(Clone, Debug)]
enum Base {
    None,
    Builtin(String),
    Named(String),
    Tag {
        kind: TagKind,
        name: Option<String>,
        name_token: Option<usize>,
        /// Token indices of the `{` and `}` of a body.
        body: Option<(usize, usize)>,
        base_classes: Vec<String>,
    },
    Opaque,
}

#[derive(Clone, Debug)]
struct Declaration {
    base: Base,
    is_typedef: bool,
    qualifiers: Vec<String>,
    /// Tokens `0..specifier_end` are the specifiers.
    specifier_end: usize,
    declarators: Vec<Declarator>,
}

#[derive(Clone, Debug)]
struct Declarator {
    name: Option<String>,
    name_token: Option<usize>,
    /// From the name outwards: `*a[4]` is `[Array, Pointer]`.
    operations: Vec<DeclarationOperation>,
    qualifiers: Vec<String>,
    /// Tokens of the declarator itself, without a bit-field width or initializer.
    start: usize,
    end: usize,
    bit_width: Option<String>,
}

enum Piece {
    Token(usize),
    /// A token left out; its leading space carries over to the next token.
    Skipped(usize),
    Elided(&'static str),
}

struct Context<'a> {
    source: &'a [u8],
    language: SourceLanguage,
    comments: &'a [Comment],
    /// Tokens the scans may still step over. Well-formed code steps over each token a few
    /// times per nesting level; unbalanced brackets or statements without an end would make
    /// some scans quadratic, so they stop (and the result is marked truncated) when it runs out.
    budget: Cell<usize>,
}

/// Token steps allowed per lexed token; see [`Context::budget`].
const SCAN_STEPS_PER_TOKEN: usize = 48;

impl<'a> Context<'a> {
    fn new(source: &'a [u8], language: SourceLanguage, lexed: &'a Lexed) -> Self {
        Self {
            source,
            language,
            comments: &lexed.comments,
            budget: Cell::new(
                lexed
                    .tokens
                    .len()
                    .saturating_mul(SCAN_STEPS_PER_TOKEN)
                    .saturating_add(1 << 16),
            ),
        }
    }

    /// Takes `steps` from the budget; `false` once it has run out.
    fn spend(&self, steps: usize) -> bool {
        let left = self.budget.get();
        self.budget.set(left.saturating_sub(steps));
        left >= steps
    }

    fn exhausted(&self) -> bool {
        self.budget.get() == 0
    }

    fn bytes(&self, token: &Token) -> &'a [u8] {
        self.source
            .get(token.start.byte as usize..token.end.byte as usize)
            .unwrap_or_default()
    }

    fn text(&self, token: &Token) -> Cow<'a, str> {
        String::from_utf8_lossy(self.bytes(token))
    }

    fn is(&self, token: Option<&Token>, text: &str) -> bool {
        token.is_some_and(|token| {
            token.kind != TokenKind::Literal
                && token.kind != TokenKind::Directive
                && self.bytes(token) == text.as_bytes()
        })
    }

    fn identifier(&self, token: Option<&Token>) -> Option<&'a [u8]> {
        token
            .filter(|token| token.kind == TokenKind::Identifier)
            .map(|token| self.bytes(token))
    }

    /// Joins tokens, putting one space where the source had whitespace or comments, and one
    /// where skipped tokens had it.
    fn join(&self, tokens: &[Token], pieces: impl IntoIterator<Item = Piece>) -> String {
        let mut output = String::new();
        let mut pending_space = false;
        for piece in pieces {
            match piece {
                Piece::Token(index) => {
                    let Some(token) = tokens.get(index) else {
                        continue;
                    };
                    if !output.is_empty() && (pending_space || token.space_before) {
                        output.push(' ');
                    }
                    output.push_str(&self.text(token));
                    pending_space = false;
                }
                Piece::Skipped(index) => {
                    if tokens.get(index).is_some_and(|token| token.space_before) {
                        pending_space = true;
                    }
                }
                Piece::Elided(text) => {
                    if !output.is_empty() {
                        output.push(' ');
                    }
                    output.push_str(text);
                    pending_space = true;
                }
            }
        }
        output
    }

    fn join_range(&self, tokens: &[Token], range: std::ops::Range<usize>) -> String {
        self.join(tokens, range.map(Piece::Token))
    }

    /// Index of the token closing the group opened at `open`, or `None` if unbalanced.
    fn matching(&self, tokens: &[Token], open: usize) -> Option<usize> {
        let opener = tokens.get(open)?;
        let (open_text, close_text): (&[u8], &[u8]) = match self.bytes(opener) {
            b"(" => (b"(", b")"),
            b"[" => (b"[", b"]"),
            b"{" => (b"{", b"}"),
            b"<" => (b"<", b">"),
            _ => return None,
        };
        let mut depth = 0usize;
        for (index, token) in tokens.iter().enumerate().skip(open) {
            if !self.spend(1) {
                return None;
            }
            if token.kind != TokenKind::Punctuation {
                continue;
            }
            let text = self.bytes(token);
            if text == open_text {
                depth += 1;
            } else if text == close_text {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(index);
                }
            } else if open_text == b"<" && (text == b";" || text == b"{" || text == b"}") {
                return None;
            }
        }
        None
    }

    /// Drops attributes (`__attribute__((...))`, `[[...]]`, `__declspec(...)`, SAL calls) that
    /// do not take part in the type.
    fn strip_attributes(&self, tokens: &[Token]) -> Vec<Token> {
        let mut output = Vec::with_capacity(tokens.len());
        let mut index = 0;
        while let Some(token) = tokens.get(index) {
            if let Some(word) = self.identifier(Some(token))
                && is_attribute_call(word)
                && self.is(tokens.get(index + 1), "(")
                && let Some(close) = self.matching(tokens, index + 1)
            {
                index = close + 1;
                continue;
            }
            let statement_start = output.is_empty();
            if self.is(Some(token), "[")
                && (self.is(tokens.get(index + 1), "[") || statement_start)
                && let Some(close) = self.matching(tokens, index)
            {
                index = close + 1;
                continue;
            }
            output.push(*token);
            index += 1;
        }
        // A token after a removed attribute keeps the gap.
        let mut previous_end: Option<u32> = None;
        for token in &mut output {
            if let Some(previous_end) = previous_end
                && previous_end != token.start.byte
            {
                let between = self
                    .source
                    .get(previous_end as usize..token.start.byte as usize)
                    .unwrap_or_default();
                if !between.is_empty() && !token.space_before {
                    token.space_before = true;
                }
            }
            previous_end = Some(token.end.byte);
        }
        output
    }

    /// A qualified name (`ns::Type`, with template arguments) starting at `index`. Returns the
    /// last component and the index after the name.
    fn qualified_name(&self, tokens: &[Token], mut index: usize) -> Option<(String, usize, usize)> {
        if self.is(tokens.get(index), "::") {
            index += 1;
        }
        let mut last_token = index;
        let mut name = String::from_utf8_lossy(self.identifier(tokens.get(index))?).into_owned();
        index += 1;
        loop {
            if self.language == SourceLanguage::Cpp
                && self.is(tokens.get(index), "<")
                && let Some(close) = self.matching(tokens, index)
            {
                index = close + 1;
                continue;
            }
            if self.is(tokens.get(index), "::")
                && let Some(next) = self.identifier(tokens.get(index + 1))
            {
                name = String::from_utf8_lossy(next).into_owned();
                last_token = index + 1;
                index += 2;
                continue;
            }
            break;
        }
        Some((name, last_token, index))
    }

    fn parse_declaration(&self, tokens: &[Token]) -> Option<Declaration> {
        let mut index = 0;
        let mut is_typedef = false;
        let mut qualifiers = Vec::new();
        let mut builtin_words: Vec<String> = Vec::new();
        let mut tag: Option<Base> = None;
        let mut opaque = false;
        // Plain identifiers in specifier position: (start token, last-component token, text).
        let mut run: Vec<(usize, usize, String)> = Vec::new();
        let mut after_body = false;
        while let Some(token) = tokens.get(index) {
            if token.kind == TokenKind::Directive {
                index += 1;
                continue;
            }
            if self.is(Some(token), "::") && self.identifier(tokens.get(index + 1)).is_some() {
                let (name, last, next) = self.qualified_name(tokens, index)?;
                run.push((index, last, name));
                index = next;
                continue;
            }
            let Some(word) = self.identifier(Some(token)) else {
                break;
            };
            if word == b"typedef" {
                is_typedef = true;
                index += 1;
            } else if is_word(word, STATEMENT_KEYWORDS) {
                return None;
            } else if is_word(word, QUALIFIER_KEYWORDS) {
                qualifiers.push(self.text(token).into_owned());
                index += 1;
            } else if is_word(word, TYPE_KEYWORDS) {
                builtin_words.push(self.text(token).into_owned());
                index += 1;
            } else if let Some(kind) = TagKind::from_keyword(word, self.language) {
                if tag.is_some() || !builtin_words.is_empty() {
                    return None;
                }
                let (parsed, next) = self.parse_tag(tokens, index, kind)?;
                after_body = matches!(parsed, Base::Tag { body: Some(_), .. });
                tag = Some(parsed);
                index = next;
            } else if is_typeof(word) && self.is(tokens.get(index + 1), "(") {
                index = self.matching(tokens, index + 1)? + 1;
                opaque = true;
            } else if is_known_annotation(word) || (after_body && word.starts_with(b"__")) {
                qualifiers.push(self.text(token).into_owned());
                index += 1;
            } else if self.language == SourceLanguage::Cpp && word == b"operator" {
                break;
            } else if run.is_empty()
                && builtin_words.is_empty()
                && tag.is_none()
                && self.is(tokens.get(index + 1), "(")
                && let Some(close) = self.matching(tokens, index + 1)
                && let Some(following) = self.identifier(tokens.get(close + 1))
                && !is_word(following, TYPE_KEYWORDS)
                && !is_word(following, QUALIFIER_KEYWORDS)
                && tokens.get(close + 2).is_none_or(|token| {
                    token.kind == TokenKind::Punctuation
                        && matches!(self.bytes(token), b";" | b"," | b"[" | b"=" | b":")
                })
            {
                // A type made by a macro, such as `Array(char) characters;` or
                // `SLIST_ENTRY(node) link;`.
                index = close + 1;
                opaque = true;
            } else if is_all_caps(word)
                && self.is(tokens.get(index + 1), "(")
                && let Some(close) = self.matching(tokens, index + 1)
                && self.identifier(tokens.get(close + 1)).is_some()
            {
                // A macro call in front of the rest of the declaration, such as
                // `DECLSPEC_XFGVIRT(IUnknown, AddRef) ULONG (...)`: it annotates what follows.
                qualifiers.push(self.join_range(tokens, index..close + 1));
                index = close + 1;
            } else {
                let (name, last, next) = self.qualified_name(tokens, index)?;
                run.push((index, last, name));
                index = next;
            }
        }

        let next = tokens.get(index);
        let ends_run = next.is_none_or(|token| {
            token.kind == TokenKind::Punctuation
                && matches!(
                    self.bytes(token),
                    b";" | b"," | b"[" | b":" | b"=" | b"(" | b")" | b"{"
                )
        });
        let has_base = !builtin_words.is_empty() || tag.is_some() || opaque;
        // A C++ constructor declares a name without a type.
        let constructor = self.language == SourceLanguage::Cpp
            && !has_base
            && run.len() == 1
            && self.is(next, "(");
        let parenthesized_declarator =
            self.is(next, "(") && self.is_nested_declarator(tokens, index);
        let mut declarator_start = index;
        if ends_run
            && !parenthesized_declarator
            && (has_base || run.len() >= 2 || constructor)
            && let Some(&(start, _, _)) = run.last()
        {
            declarator_start = start;
            run.pop();
        }

        let base = if let Some(tag) = tag {
            qualifiers.extend(run.iter().map(|(_, _, name)| name.clone()));
            tag
        } else if opaque {
            Base::Opaque
        } else if !builtin_words.is_empty() {
            qualifiers.extend(run.iter().map(|(_, _, name)| name.clone()));
            Base::Builtin(builtin_words.join(" "))
        } else if run.is_empty() {
            Base::None
        } else {
            let chosen = choose_base(&run);
            for (position, (_, _, name)) in run.iter().enumerate() {
                if position != chosen {
                    qualifiers.push(name.clone());
                }
            }
            match run.get(chosen) {
                Some((_, _, name)) => Base::Named(name.clone()),
                None => Base::None,
            }
        };

        let mut declarators = Vec::new();
        let mut index = declarator_start;
        while index < tokens.len() {
            let (mut declarator, next) = self.parse_declarator(tokens, index, 0)?;
            index = next;
            while let Some(word) = self.identifier(tokens.get(index))
                && is_known_annotation(word)
            {
                declarator
                    .qualifiers
                    .push(String::from_utf8_lossy(word).into_owned());
                index += 1;
            }
            if self.is(tokens.get(index), ":") {
                let width_start = index + 1;
                index = self.skip_to_comma(tokens, width_start);
                declarator.bit_width = Some(self.join_range(tokens, width_start..index));
            }
            if self.is(tokens.get(index), "=") || self.is(tokens.get(index), "{") {
                index = self.skip_to_comma(tokens, index);
            }
            declarators.push(declarator);
            if self.is(tokens.get(index), ",") {
                index += 1;
                continue;
            }
            if index < tokens.len() {
                return None;
            }
        }
        Some(Declaration {
            base,
            is_typedef,
            qualifiers,
            specifier_end: declarator_start,
            declarators,
        })
    }

    fn skip_to_comma(&self, tokens: &[Token], mut index: usize) -> usize {
        while let Some(token) = tokens.get(index) {
            if token.kind == TokenKind::Punctuation {
                match self.bytes(token) {
                    b"," => return index,
                    b"(" | b"[" | b"{" => {
                        match self.matching(tokens, index) {
                            Some(close) => index = close + 1,
                            None => return tokens.len(),
                        }
                        continue;
                    }
                    _ => {}
                }
            }
            index += 1;
        }
        index
    }

    fn parse_tag(&self, tokens: &[Token], index: usize, kind: TagKind) -> Option<(Base, usize)> {
        let mut index = index + 1;
        if self.language == SourceLanguage::Cpp
            && kind == TagKind::Enum
            && (self.is(tokens.get(index), "class") || self.is(tokens.get(index), "struct"))
        {
            index += 1;
        }
        loop {
            match self.identifier(tokens.get(index)) {
                Some(word) if is_known_annotation(word) => index += 1,
                // A tag name is never followed by `(`: this is a macro such as
                // `DECLSPEC_ALIGN(16)` or `__aligned(8)`.
                Some(_) if self.is(tokens.get(index + 1), "(") => {
                    match self.matching(tokens, index + 1) {
                        Some(close) => index = close + 1,
                        None => break,
                    }
                }
                _ => break,
            }
        }
        let mut name = None;
        let mut name_token = None;
        if self.identifier(tokens.get(index)).is_some() || self.is(tokens.get(index), "::") {
            let (text, last, next) = self.qualified_name(tokens, index)?;
            name = Some(text);
            name_token = Some(last);
            index = next;
            // Annotations after the name (`struct foo __rcu *p`) are qualifiers of what is
            // declared; the declaration parser records them.
            if self.is(tokens.get(index), "final") {
                index += 1;
            }
        }
        let mut base_classes = Vec::new();
        if self.is(tokens.get(index), ":") {
            // Base classes, or an enum's underlying type; a bit-field width otherwise.
            let mut end = index + 1;
            while let Some(token) = tokens.get(end) {
                if self.is(Some(token), "{") || self.is(Some(token), ";") {
                    break;
                }
                if self.is(Some(token), "<")
                    && let Some(close) = self.matching(tokens, end)
                {
                    end = close + 1;
                    continue;
                }
                end += 1;
            }
            if self.is(tokens.get(end), "{") {
                if kind != TagKind::Enum {
                    base_classes = self.base_classes(tokens, index + 1, end);
                }
                index = end;
            }
        }
        let mut body = None;
        if self.is(tokens.get(index), "{") {
            let close = self
                .matching(tokens, index)
                .unwrap_or(tokens.len().saturating_sub(1));
            body = Some((index, close));
            index = close + 1;
        }
        Some((
            Base::Tag {
                kind,
                name,
                name_token,
                body,
                base_classes,
            },
            index,
        ))
    }

    fn base_classes(&self, tokens: &[Token], start: usize, end: usize) -> Vec<String> {
        let mut classes = Vec::new();
        let mut piece_start = start;
        let mut index = start;
        let push = |from: usize, to: usize, classes: &mut Vec<String>| {
            let mut from = from;
            while let Some(word) = self.identifier(tokens.get(from))
                && from < to
                && matches!(word, b"public" | b"private" | b"protected" | b"virtual")
            {
                from += 1;
            }
            let text = self.join_range(tokens, from..to);
            if !text.is_empty() {
                classes.push(text);
            }
        };
        while index < end {
            if self.is(tokens.get(index), "<")
                && let Some(close) = self.matching(tokens, index)
            {
                index = close + 1;
                continue;
            }
            if self.is(tokens.get(index), ",") {
                push(piece_start, index, &mut classes);
                piece_start = index + 1;
            }
            index += 1;
        }
        push(piece_start, end, &mut classes);
        classes
    }

    fn is_nested_declarator(&self, tokens: &[Token], open: usize) -> bool {
        let next = tokens.get(open + 1);
        if ["*", "^", "&", "&&"].iter().any(|text| self.is(next, text)) {
            return true;
        }
        if let Some(word) = self.identifier(next) {
            let after = tokens.get(open + 2);
            if is_known_annotation(word) && (self.is(after, "*") || self.is(after, "^")) {
                return true;
            }
            if self.language == SourceLanguage::Cpp && self.is(after, "::") {
                return tokens
                    .get(open + 1..)
                    .unwrap_or_default()
                    .iter()
                    .take(8)
                    .any(|token| self.is(Some(token), "*"));
            }
        }
        false
    }

    fn parse_declarator(
        &self,
        tokens: &[Token],
        start: usize,
        depth: usize,
    ) -> Option<(Declarator, usize)> {
        if depth > MAX_DECLARATOR_DEPTH {
            return None;
        }
        let mut index = start;
        let mut prefix = Vec::new();
        let mut qualifiers = Vec::new();
        loop {
            let token = tokens.get(index);
            if self.is(token, "*") || self.is(token, "^") {
                prefix.push(DeclarationOperation::Pointer);
                index += 1;
            } else if self.is(token, "&") || self.is(token, "&&") {
                prefix.push(DeclarationOperation::Reference);
                index += 1;
            } else if let Some(word) = self.identifier(token)
                && (is_word(word, QUALIFIER_KEYWORDS) || is_known_annotation(word))
            {
                qualifiers.push(String::from_utf8_lossy(word).into_owned());
                index += 1;
            } else if self.language == SourceLanguage::Cpp
                && self.identifier(token).is_some()
                && self.is(tokens.get(index + 1), "::")
                && self.is(tokens.get(index + 2), "*")
            {
                prefix.push(DeclarationOperation::Pointer);
                index += 3;
            } else {
                break;
            }
        }

        let mut name = None;
        let mut name_token = None;
        let mut inner_operations = Vec::new();
        if self.is(tokens.get(index), "(") && self.is_nested_declarator(tokens, index) {
            let (inner, next) = self.parse_declarator(tokens, index + 1, depth + 1)?;
            if !self.is(tokens.get(next), ")") {
                return None;
            }
            index = next + 1;
            name = inner.name;
            name_token = inner.name_token;
            inner_operations = inner.operations;
            qualifiers.extend(inner.qualifiers);
        } else if self.language == SourceLanguage::Cpp && self.is(tokens.get(index), "~") {
            let word = self.identifier(tokens.get(index + 1))?;
            name = Some(format!("~{}", String::from_utf8_lossy(word)));
            name_token = Some(index + 1);
            index += 2;
        } else if self.language == SourceLanguage::Cpp && self.is(tokens.get(index), "operator") {
            let mut end = index + 1;
            while let Some(token) = tokens.get(end) {
                if self.is(Some(token), "(") && end > index + 1 {
                    break;
                }
                end += 1;
            }
            name = Some(self.join_range(tokens, index..end));
            name_token = Some(index);
            index = end;
        } else if let Some(word) = self.identifier(tokens.get(index))
            && !is_word(word, TYPE_KEYWORDS)
            && !is_word(word, STATEMENT_KEYWORDS)
        {
            let (text, last, next) = self.qualified_name(tokens, index)?;
            name = Some(text);
            name_token = Some(last);
            index = next;
        }

        let mut suffix = Vec::new();
        loop {
            let token = tokens.get(index);
            if self.is(token, "[") {
                let close = self.matching(tokens, index)?;
                suffix.push(DeclarationOperation::Array(
                    self.join_range(tokens, index + 1..close),
                ));
                index = close + 1;
            } else if self.is(token, "(") {
                let close = self.matching(tokens, index)?;
                suffix.push(DeclarationOperation::Function);
                index = close + 1;
                if self.language == SourceLanguage::Cpp {
                    index = self.skip_function_qualifiers(tokens, index);
                }
            } else if let Some(word) = self.identifier(token)
                && is_known_annotation(word)
            {
                qualifiers.push(String::from_utf8_lossy(word).into_owned());
                index += 1;
            } else {
                break;
            }
        }

        let mut operations = inner_operations;
        operations.extend(suffix);
        operations.extend(prefix.into_iter().rev());
        Some((
            Declarator {
                name,
                name_token,
                operations,
                qualifiers,
                start,
                end: index,
                bit_width: None,
            },
            index,
        ))
    }

    fn skip_function_qualifiers(&self, tokens: &[Token], mut index: usize) -> usize {
        loop {
            let token = tokens.get(index);
            if let Some(word) = self.identifier(token)
                && matches!(
                    word,
                    b"const" | b"volatile" | b"override" | b"final" | b"noexcept" | b"throw"
                )
            {
                index += 1;
                if self.is(tokens.get(index), "(")
                    && let Some(close) = self.matching(tokens, index)
                {
                    index = close + 1;
                }
            } else if self.is(token, "&") || self.is(token, "&&") {
                index += 1;
            } else if self.is(token, "->") {
                // A trailing return type runs to the end of the declarator.
                index += 1;
                while let Some(token) = tokens.get(index) {
                    if self.is(Some(token), ",")
                        || self.is(Some(token), "=")
                        || self.is(Some(token), "{")
                        || self.is(Some(token), ";")
                    {
                        break;
                    }
                    index += 1;
                }
            } else {
                return index;
            }
        }
    }

    /// Specifier pieces for text, with a body shown as `{...}`. Storage classes (`static`,
    /// `typedef`) are left out of a type but kept in a member's declaration.
    fn specifier_pieces(
        &self,
        tokens: &[Token],
        declaration: &Declaration,
        with_storage: bool,
    ) -> Vec<Piece> {
        let body = match &declaration.base {
            Base::Tag {
                body: Some(body), ..
            } => Some(*body),
            _ => None,
        };
        let mut pieces = Vec::new();
        let mut index = 0;
        while index < declaration.specifier_end {
            if let Some((open, close)) = body
                && index == open
            {
                pieces.push(Piece::Elided("{...}"));
                index = close + 1;
                continue;
            }
            let is_storage = self
                .identifier(tokens.get(index))
                .is_some_and(|word| is_word(word, STORAGE_KEYWORDS));
            pieces.push(if is_storage && !with_storage {
                Piece::Skipped(index)
            } else {
                Piece::Token(index)
            });
            index += 1;
        }
        pieces
    }

    fn specifier_type_text(&self, tokens: &[Token], declaration: &Declaration) -> TypeText {
        let mut text = TypeText {
            text: self.join(tokens, self.specifier_pieces(tokens, declaration, false)),
            qualifiers: declaration.qualifiers.clone(),
            ..TypeText::default()
        };
        self.apply_base(&mut text, &declaration.base);
        text
    }

    fn apply_base(&self, text: &mut TypeText, base: &Base) {
        match base {
            Base::None | Base::Opaque => {}
            Base::Builtin(words) => {
                text.core_name = Some(words.clone());
                text.is_builtin = true;
            }
            Base::Named(name) => {
                text.core_name = Some(name.clone());
                text.is_builtin = STANDARD_TYPEDEFS.contains(&name.as_str());
            }
            Base::Tag { kind, name, .. } => {
                text.tag = Some(*kind);
                text.core_name = name.clone();
            }
        }
    }

    fn type_text(
        &self,
        tokens: &[Token],
        declaration: &Declaration,
        declarator: &Declarator,
    ) -> TypeText {
        let mut pieces = self.specifier_pieces(tokens, declaration, false);
        pieces.extend((declarator.start..declarator.end).map(|index| {
            if Some(index) == declarator.name_token {
                Piece::Skipped(index)
            } else {
                Piece::Token(index)
            }
        }));
        let mut text = TypeText {
            text: self.join(tokens, pieces),
            qualifiers: declaration.qualifiers.clone(),
            ..TypeText::default()
        };
        text.qualifiers
            .extend(declarator.qualifiers.iter().cloned());
        self.apply_base(&mut text, &declaration.base);
        let operations = &declarator.operations;
        let function_at = operations
            .iter()
            .position(|operation| *operation == DeclarationOperation::Function);
        let data_operations = match function_at {
            Some(position) => operations.get(..position).unwrap_or_default(),
            None => operations.as_slice(),
        };
        for operation in data_operations {
            match operation {
                DeclarationOperation::Pointer => text.pointer_depth += 1,
                DeclarationOperation::Reference => text.is_reference = true,
                DeclarationOperation::Array(dimension) => {
                    text.array_dimensions.push(dimension.clone())
                }
                DeclarationOperation::Function => {}
            }
        }
        match function_at {
            Some(0) => text.is_function = true,
            Some(_) => text.is_function_pointer = true,
            None => {}
        }
        if function_at.is_some() {
            // What matters about a function-typed member is its signature, not its return type.
            text.core_name = None;
            text.tag = None;
            text.is_builtin = false;
        }
        text
    }

    /// The return type of a function declarator: the specifiers plus the operations after the
    /// function.
    fn return_type(
        &self,
        tokens: &[Token],
        declaration: &Declaration,
        declarator: &Declarator,
    ) -> Option<TypeText> {
        let position = declarator
            .operations
            .iter()
            .position(|operation| *operation == DeclarationOperation::Function)?;
        let mut text = self.specifier_type_text(tokens, declaration);
        let after = declarator
            .operations
            .get(position + 1..)
            .unwrap_or_default();
        if after.contains(&DeclarationOperation::Function) {
            // A function returning a function pointer.
            text.core_name = None;
            text.tag = None;
            text.is_function_pointer = true;
            return Some(text);
        }
        for operation in after {
            match operation {
                DeclarationOperation::Pointer => text.pointer_depth += 1,
                DeclarationOperation::Reference => text.is_reference = true,
                DeclarationOperation::Array(_) | DeclarationOperation::Function => {}
            }
        }
        if text.pointer_depth > 0 {
            text.text = format!("{} {}", text.text, "*".repeat(text.pointer_depth as usize));
        }
        Some(text)
    }

    fn declared_names(&self, tokens: &[Token]) -> Vec<DeclaredName> {
        let tokens = self.strip_attributes(tokens);
        let end = tokens
            .iter()
            .position(|token| self.is(Some(token), ";") || self.is(Some(token), "{"))
            .unwrap_or(tokens.len());
        let Some(tokens) = tokens.get(..end) else {
            return Vec::new();
        };
        let Some(declaration) = self.parse_declaration(tokens) else {
            return Vec::new();
        };
        declaration
            .declarators
            .iter()
            .filter_map(|declarator| {
                let name = declarator.name.clone()?;
                let name_token = tokens.get(declarator.name_token?)?;
                Some(DeclaredName {
                    name,
                    ty: self.type_text(tokens, &declaration, declarator),
                    return_type: self.return_type(tokens, &declaration, declarator),
                    name_range: name_token.range(),
                })
            })
            .collect()
    }

    // -----------------------------------------------------------------------------------------
    // Type definitions

    /// Whether the tag keyword at `index` is followed by `name` (after annotations).
    fn names_tag(&self, tokens: &[Token], index: usize, name: &str) -> bool {
        let mut index = index + 1;
        for _ in 0..16 {
            match self.identifier(tokens.get(index)) {
                Some(word) if word == name.as_bytes() => return true,
                Some(word)
                    if is_known_annotation(word)
                        || word == b"class"
                        || word == b"struct"
                        || is_attribute_call(word)
                        || self.is(tokens.get(index + 1), "(") =>
                {
                    if self.is(tokens.get(index + 1), "(") {
                        match self.matching(tokens, index + 1) {
                            Some(close) => index = close + 1,
                            None => return false,
                        }
                    } else {
                        index += 1;
                    }
                }
                _ => {
                    if self.is(tokens.get(index), "[")
                        && let Some(close) = self.matching(tokens, index)
                    {
                        index = close + 1;
                        continue;
                    }
                    return false;
                }
            }
        }
        false
    }

    /// Collects one statement: tokens up to a `;` outside any group, skipping directives
    /// outside groups. Returns the tokens and whether a `;` ended it.
    fn statement(&self, tokens: &[Token]) -> (Vec<Token>, bool, usize) {
        let mut statement = Vec::new();
        let mut index = 0;
        while let Some(token) = tokens.get(index) {
            if !self.spend(1) {
                break;
            }
            if token.kind == TokenKind::Directive {
                index += 1;
                continue;
            }
            if token.kind == TokenKind::Punctuation {
                match self.bytes(token) {
                    b";" => return (statement, true, index + 1),
                    b"(" | b"[" | b"{" => {
                        let close = self.matching(tokens, index).unwrap_or(tokens.len() - 1);
                        let group = tokens.get(index..=close).unwrap_or_default();
                        if !self.spend(group.len()) {
                            break;
                        }
                        statement.extend(group);
                        index = close + 1;
                        continue;
                    }
                    _ => {}
                }
            }
            statement.push(*token);
            index += 1;
        }
        (statement, false, index)
    }

    /// Reads the type definition at the start of `tokens`, and tells whether a `;` ended its
    /// statement.
    fn parse_type_statement(&self, tokens: &[Token], depth: usize) -> (Option<TypeLayout>, bool) {
        let (statement, terminated, _) = self.statement(tokens);
        (
            self.type_statement_layout(tokens, &statement, terminated, depth),
            terminated,
        )
    }

    fn type_statement_layout(
        &self,
        tokens: &[Token],
        statement: &[Token],
        terminated: bool,
        depth: usize,
    ) -> Option<TypeLayout> {
        let first = statement.first()?;
        let last = statement.last()?;
        let semicolon_end = if terminated {
            tokens
                .iter()
                .find(|token| token.start.byte >= last.end.byte && self.is(Some(token), ";"))
                .map_or(last.end, |token| token.end)
        } else {
            last.end
        };
        let range = Range {
            start: first.start,
            end: semicolon_end,
        };
        let filtered = self.strip_attributes(statement);
        let declaration = self.parse_declaration(&filtered)?;
        let mut budget = MAX_MEMBERS;
        let mut layout =
            self.layout_from_declaration(&filtered, &declaration, range, depth, None, &mut budget)?;
        layout.truncated |= self.exhausted();
        Some(layout)
    }

    fn typedef_names(&self, tokens: &[Token], declaration: &Declaration) -> Vec<TypedefName> {
        if !declaration.is_typedef {
            return Vec::new();
        }
        declaration
            .declarators
            .iter()
            .filter_map(|declarator| {
                let name = declarator.name.clone()?;
                let token = tokens.get(declarator.name_token?)?;
                let pointer_depth = declarator
                    .operations
                    .iter()
                    .filter(|operation| **operation == DeclarationOperation::Pointer)
                    .count();
                Some(TypedefName {
                    name,
                    pointer_depth: to_u32(pointer_depth),
                    name_range: token.range(),
                })
            })
            .collect()
    }

    fn layout_from_declaration(
        &self,
        tokens: &[Token],
        declaration: &Declaration,
        range: Range,
        depth: usize,
        outer_condition: Option<&str>,
        budget: &mut usize,
    ) -> Option<TypeLayout> {
        let typedef_names = self.typedef_names(tokens, declaration);
        match &declaration.base {
            Base::Tag {
                kind,
                name,
                name_token,
                body,
                base_classes,
            } => {
                let name_range = name_token
                    .and_then(|index| tokens.get(index))
                    .map(Token::range)
                    .or_else(|| typedef_names.first().map(|typedef| typedef.name_range));
                let mut layout = TypeLayout {
                    kind: TypeKind::from(*kind),
                    language: self.language,
                    tag_name: name.clone(),
                    typedef_names,
                    range,
                    name_range,
                    is_forward_declaration: body.is_none(),
                    members: Vec::new(),
                    enumerators: Vec::new(),
                    alias: None,
                    base_classes: base_classes.clone(),
                    truncated: false,
                };
                match body {
                    Some((open, close)) => {
                        let body_tokens = tokens.get(open + 1..*close).unwrap_or_default();
                        let body_start = tokens.get(*open).map_or(range.start, |token| token.end);
                        if *kind == TagKind::Enum {
                            layout.enumerators = self.parse_enumerators(
                                body_tokens,
                                outer_condition,
                                body_start,
                                budget,
                            );
                        } else {
                            layout.members = self.parse_members(
                                body_tokens,
                                depth,
                                outer_condition,
                                body_start,
                                budget,
                            );
                        }
                        layout.truncated = *budget == 0;
                    }
                    None => {
                        if declaration.is_typedef {
                            // `typedef struct foo foo_t;` names the struct defined elsewhere.
                            layout.kind = TypeKind::Typedef;
                            layout.is_forward_declaration = false;
                            layout.name_range = layout
                                .typedef_names
                                .first()
                                .map(|typedef| typedef.name_range);
                            layout.alias = declaration
                                .declarators
                                .first()
                                .map(|declarator| self.type_text(tokens, declaration, declarator));
                        } else if !declaration.declarators.is_empty() {
                            return None;
                        }
                    }
                }
                Some(layout)
            }
            _ if declaration.is_typedef => {
                let first = declaration.declarators.first()?;
                Some(TypeLayout {
                    kind: TypeKind::Typedef,
                    language: self.language,
                    tag_name: None,
                    name_range: typedef_names.first().map(|typedef| typedef.name_range),
                    typedef_names,
                    range,
                    is_forward_declaration: false,
                    members: Vec::new(),
                    enumerators: Vec::new(),
                    alias: Some(self.type_text(tokens, declaration, first)),
                    base_classes: Vec::new(),
                    truncated: false,
                })
            }
            _ => None,
        }
    }

    fn leading_comment(&self, previous_end: Position, start: Position) -> Option<String> {
        let first = self
            .comments
            .partition_point(|comment| comment.start.byte < previous_end.byte);
        let candidates = self.comments.get(first..)?;
        let last = candidates
            .iter()
            .take_while(|comment| comment.end.byte <= start.byte)
            .filter(|comment| comment.starts_line || comment.start.row > previous_end.row)
            .last()?;
        comment_first_line(self.source, last)
    }

    fn trailing_comment(&self, end: Position, next_start: Option<Position>) -> Option<String> {
        let first = self
            .comments
            .partition_point(|comment| comment.start.byte < end.byte);
        let comment = self.comments.get(first)?;
        if comment.start.row != end.row
            || next_start.is_some_and(|next| next.byte < comment.start.byte)
        {
            return None;
        }
        comment_first_line(self.source, comment)
    }

    fn parse_members(
        &self,
        tokens: &[Token],
        depth: usize,
        outer_condition: Option<&str>,
        body_start: Position,
        budget: &mut usize,
    ) -> Vec<Member> {
        let mut members = Vec::new();
        let mut conditions = ConditionStack::default();
        let mut previous_end = body_start;
        let mut index = 0;
        while let Some(token) = tokens.get(index) {
            if self.exhausted() {
                // Marks the layout truncated.
                *budget = 0;
            }
            if *budget == 0 {
                break;
            }
            if token.kind == TokenKind::Directive {
                conditions.apply(&self.directive_text(token));
                index += 1;
                continue;
            }
            if self.is(Some(token), ";") {
                index += 1;
                continue;
            }
            if let Some(word) = self.identifier(Some(token))
                && matches!(word, b"public" | b"private" | b"protected")
                && self.is(tokens.get(index + 1), ":")
            {
                index += 2;
                continue;
            }
            let condition = combine_conditions(outer_condition, conditions.current().as_deref());
            let mut statement = Vec::new();
            let mut end_token: Option<Token> = None;
            while let Some(token) = tokens.get(index) {
                if !self.spend(1) {
                    break;
                }
                if token.kind == TokenKind::Directive {
                    conditions.apply(&self.directive_text(token));
                    index += 1;
                    continue;
                }
                if token.kind == TokenKind::Punctuation {
                    match self.bytes(token) {
                        b";" => {
                            end_token = Some(*token);
                            index += 1;
                            break;
                        }
                        b"{" if self.spend(statement.len()) && self.is_method_body(&statement) => {
                            let close = self.matching(tokens, index).unwrap_or(tokens.len() - 1);
                            end_token = tokens.get(close).copied();
                            index = close + 1;
                            if self.is(tokens.get(index), ";") {
                                end_token = tokens.get(index).copied();
                                index += 1;
                            }
                            break;
                        }
                        b"(" | b"[" | b"{" => {
                            let close = self.matching(tokens, index).unwrap_or(tokens.len() - 1);
                            statement.extend(tokens.get(index..=close).unwrap_or_default());
                            index = close + 1;
                            continue;
                        }
                        _ => {}
                    }
                }
                statement.push(*token);
                index += 1;
            }
            if self.exhausted() {
                *budget = 0;
                break;
            }
            let (Some(first), Some(last)) = (statement.first(), statement.last()) else {
                continue;
            };
            let end = end_token.map_or(last.end, |token| token.end);
            let range = Range {
                start: first.start,
                end,
            };
            let next_start = tokens.get(index).map(|token| token.start);
            let comment = self
                .leading_comment(previous_end, first.start)
                .or_else(|| self.trailing_comment(end, next_start));
            previous_end = end;
            let new_members = self.members_from_statement(
                &statement,
                range,
                depth,
                condition.as_deref(),
                comment,
                budget,
            );
            *budget = budget.saturating_sub(new_members.len().max(1));
            members.extend(new_members);
        }
        members
    }

    fn is_method_body(&self, statement: &[Token]) -> bool {
        if self.language != SourceLanguage::Cpp {
            return false;
        }
        let Some(last) = statement.last() else {
            return false;
        };
        let has_parameters = statement.iter().any(|token| self.is(Some(token), "("));
        has_parameters
            && (self.is(Some(last), ")")
                || self.identifier(Some(last)).is_some_and(|word| {
                    matches!(
                        word,
                        b"const" | b"override" | b"final" | b"noexcept" | b"volatile"
                    )
                }))
    }

    fn directive_text(&self, token: &Token) -> String {
        let text = self.bytes(token);
        let mut output = Vec::with_capacity(text.len());
        let mut index = 0;
        while let Some(&byte) = text.get(index) {
            if byte == b'/' && text.get(index + 1) == Some(&b'*') {
                index += 2;
                while index < text.len()
                    && !(text.get(index) == Some(&b'*') && text.get(index + 1) == Some(&b'/'))
                {
                    index += 1;
                }
                index += 2;
                output.push(b' ');
                continue;
            }
            if byte == b'/' && text.get(index + 1) == Some(&b'/') {
                break;
            }
            if byte == b'\\' || byte == b'\r' || byte == b'\n' {
                output.push(b' ');
            } else {
                output.push(byte);
            }
            index += 1;
        }
        collapse_whitespace(&String::from_utf8_lossy(&output))
    }

    fn members_from_statement(
        &self,
        statement: &[Token],
        range: Range,
        depth: usize,
        condition: Option<&str>,
        comment: Option<String>,
        budget: &mut usize,
    ) -> Vec<Member> {
        let unparsed = |text: String| Member {
            name: None,
            kind: MemberKind::Unparsed,
            ty: TypeText::default(),
            text,
            bit_width: None,
            nested: None,
            name_range: None,
            range,
            comment: comment.clone(),
            condition: condition.map(str::to_owned),
        };
        let full_text = self.join_range(statement, 0..statement.len());
        let starts_with_other = self.identifier(statement.first()).is_some_and(|word| {
            matches!(
                word,
                b"typedef"
                    | b"using"
                    | b"friend"
                    | b"template"
                    | b"static_assert"
                    | b"_Static_assert"
            )
        });
        if starts_with_other {
            return vec![unparsed(full_text)];
        }
        let tokens = self.strip_attributes(statement);
        let Some(declaration) = self.parse_declaration(&tokens) else {
            return vec![unparsed(full_text)];
        };
        let mut nested = None;
        if let Base::Tag {
            kind,
            name,
            name_token,
            body: Some((open, close)),
            base_classes,
        } = &declaration.base
        {
            if depth >= MAX_NESTING_DEPTH {
                return vec![unparsed(full_text)];
            }
            let nested_range = Range {
                start: tokens.first().map_or(range.start, |token| token.start),
                end: tokens.get(*close).map_or(range.end, |token| token.end),
            };
            let body_tokens = tokens.get(open + 1..*close).unwrap_or_default();
            let body_start = tokens.get(*open).map_or(range.start, |token| token.end);
            let mut layout = TypeLayout {
                kind: TypeKind::from(*kind),
                language: self.language,
                tag_name: name.clone(),
                typedef_names: Vec::new(),
                range: nested_range,
                name_range: name_token
                    .and_then(|index| tokens.get(index))
                    .map(Token::range),
                is_forward_declaration: false,
                members: Vec::new(),
                enumerators: Vec::new(),
                alias: None,
                base_classes: base_classes.clone(),
                truncated: false,
            };
            if *kind == TagKind::Enum {
                layout.enumerators =
                    self.parse_enumerators(body_tokens, condition, body_start, budget);
            } else {
                layout.members =
                    self.parse_members(body_tokens, depth + 1, condition, body_start, budget);
            }
            layout.truncated = *budget == 0;
            if declaration.declarators.is_empty() {
                let kind = if name.is_none() {
                    MemberKind::AnonymousAggregate
                } else {
                    MemberKind::NestedType
                };
                let mut ty = self.specifier_type_text(&tokens, &declaration);
                ty.core_name = name.clone();
                return vec![Member {
                    name: None,
                    kind,
                    ty,
                    text: self.join(&tokens, self.specifier_pieces(&tokens, &declaration, true)),
                    bit_width: None,
                    name_range: layout.name_range,
                    nested: Some(Box::new(layout)),
                    range,
                    comment,
                    condition: condition.map(str::to_owned),
                }];
            }
            nested = Some(Box::new(layout));
        }
        if declaration.declarators.is_empty() {
            if matches!(declaration.base, Base::None) {
                return Vec::new();
            }
            return vec![unparsed(full_text)];
        }
        let mut members = Vec::new();
        // Every declarator of `struct { ... } a, b;` carries its own copy of the body, so the
        // copies count against the member limit; otherwise nesting multiplies them.
        let nested_size = nested.as_deref().map_or(0, layout_size);
        for (position, declarator) in declaration.declarators.iter().enumerate() {
            let nested = if position > 0 && nested.is_some() {
                if *budget > nested_size {
                    *budget -= nested_size;
                    nested.clone()
                } else {
                    *budget = 0;
                    None
                }
            } else {
                nested.clone()
            };
            let ty = self.type_text(&tokens, &declaration, declarator);
            if declarator.name.is_none() && declarator.bit_width.is_none() {
                return vec![unparsed(full_text)];
            }
            let kind = if ty.is_function {
                if self.language != SourceLanguage::Cpp {
                    // A C struct cannot declare a function; this is a macro that looks like one.
                    return vec![unparsed(full_text)];
                }
                MemberKind::Method
            } else if ty.is_function_pointer {
                MemberKind::FunctionPointer
            } else {
                MemberKind::Field
            };
            let mut pieces = self.specifier_pieces(&tokens, &declaration, true);
            pieces.extend((declarator.start..declarator.end).map(Piece::Token));
            let mut text = self.join(&tokens, pieces);
            if let Some(width) = &declarator.bit_width {
                text = format!("{text} : {width}");
            }
            members.push(Member {
                name: declarator.name.clone(),
                kind,
                ty,
                text,
                bit_width: declarator.bit_width.clone(),
                nested,
                name_range: declarator
                    .name_token
                    .and_then(|index| tokens.get(index))
                    .map(Token::range),
                range,
                comment: comment.clone(),
                condition: condition.map(str::to_owned),
            });
        }
        members
    }

    fn parse_enumerators(
        &self,
        tokens: &[Token],
        outer_condition: Option<&str>,
        body_start: Position,
        budget: &mut usize,
    ) -> Vec<Enumerator> {
        let mut enumerators = Vec::new();
        let mut conditions = ConditionStack::default();
        let mut counter = EnumCounter::default();
        let mut previous_end = body_start;
        let mut index = 0;
        while index < tokens.len() {
            if !self.spend(1) {
                *budget = 0;
            }
            if *budget == 0 {
                break;
            }
            let Some(token) = tokens.get(index) else {
                break;
            };
            if token.kind == TokenKind::Directive {
                let text = self.directive_text(token);
                conditions.apply(&text);
                counter.apply(&text);
                index += 1;
                continue;
            }
            if self.is(Some(token), ",") {
                index += 1;
                continue;
            }
            let condition = combine_conditions(outer_condition, conditions.current().as_deref());
            let start = index;
            let mut segment = Vec::new();
            while let Some(token) = tokens.get(index) {
                if token.kind == TokenKind::Directive || self.is(Some(token), ",") {
                    break;
                }
                if self.is(Some(token), "(") || self.is(Some(token), "[") {
                    let close = self.matching(tokens, index).unwrap_or(tokens.len() - 1);
                    segment.extend(tokens.get(index..=close).unwrap_or_default());
                    index = close + 1;
                    continue;
                }
                segment.push(*token);
                index += 1;
            }
            if index == start {
                index += 1;
                continue;
            }
            let segment = self.strip_attributes(&segment);
            let Some(name_token) = segment
                .first()
                .filter(|token| token.kind == TokenKind::Identifier)
            else {
                continue;
            };
            let value_text = segment
                .iter()
                .position(|token| self.is(Some(token), "="))
                .map(|equals| self.join_range(&segment, equals + 1..segment.len()))
                .filter(|text| !text.is_empty());
            let segment_end = segment.last().map_or(name_token.end, |token| token.end);
            let end_for_comment = if self.is(tokens.get(index), ",") {
                tokens.get(index).map_or(segment_end, |token| token.end)
            } else {
                segment_end
            };
            let after_comma = if self.is(tokens.get(index), ",") {
                index + 1
            } else {
                index
            };
            let next_start = tokens.get(after_comma).map(|token| token.start);
            let comment = self
                .leading_comment(previous_end, name_token.start)
                .or_else(|| self.trailing_comment(end_for_comment, next_start));
            previous_end = end_for_comment;
            let (value, value_is_inferred) = counter.next(value_text.as_deref());
            enumerators.push(Enumerator {
                name: self.text(name_token).into_owned(),
                value_text,
                value,
                value_is_inferred,
                name_range: name_token.range(),
                comment,
                condition,
            });
            *budget = budget.saturating_sub(1);
        }
        enumerators
    }
}

/// Picks the type name among identifiers in specifier position: `MEM_STATIC U16`,
/// `ZSTDLIB_API size_t`, `u32 __unknown_annotation`.
fn choose_base(run: &[(usize, usize, String)]) -> usize {
    let plain = |name: &str| !name.starts_with("__") && !is_all_caps(name.as_bytes());
    if let Some(position) = run.iter().rposition(|(_, _, name)| plain(name)) {
        return position;
    }
    if let Some(position) = run.iter().position(|(_, _, name)| name.starts_with("__")) {
        return position;
    }
    run.len().saturating_sub(1)
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn comment_first_line(source: &[u8], comment: &Comment) -> Option<String> {
    let text = source.get(comment.start.byte as usize..comment.end.byte as usize)?;
    let text = String::from_utf8_lossy(text);
    let body = if let Some(rest) = text.strip_prefix("//") {
        rest.trim_start_matches(['/', '!', '<'])
    } else {
        let rest = text.strip_prefix("/*").unwrap_or(&text);
        let rest = rest.strip_suffix("*/").unwrap_or(rest);
        rest.trim_start_matches(['*', '!', '<'])
    };
    let line = body
        .lines()
        .map(|line| line.trim().trim_start_matches('*').trim())
        .find(|line| !line.is_empty())?;
    let mut output: String = line.chars().take(MAX_COMMENT_CHARS).collect();
    if line.chars().nth(MAX_COMMENT_CHARS).is_some() {
        output.push('…');
    }
    Some(output)
}

fn combine_conditions(outer: Option<&str>, inner: Option<&str>) -> Option<String> {
    match (outer, inner) {
        (Some(outer), Some(inner)) => Some(cap_condition(format!("{outer} && {inner}"))),
        (Some(outer), None) => Some(outer.to_owned()),
        (None, Some(inner)) => Some(inner.to_owned()),
        (None, None) => None,
    }
}

fn is_simple_condition(condition: &str) -> bool {
    condition
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        || (condition.starts_with('!')
            && condition
                .bytes()
                .skip(1)
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'))
}

fn negate(condition: &str) -> String {
    if let Some(rest) = condition.strip_prefix('!')
        && is_simple_condition(rest)
    {
        rest.to_owned()
    } else if is_simple_condition(condition) {
        format!("!{condition}")
    } else {
        format!("!({condition})")
    }
}

/// The directive keyword and the rest of a preprocessor line.
fn split_directive(text: &str) -> (&str, &str) {
    let text = text.trim_start().trim_start_matches('#').trim_start();
    let keyword_end = text
        .find(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .unwrap_or(text.len());
    let (keyword, rest) = text.split_at(keyword_end);
    (keyword, rest.trim())
}

#[derive(Default)]
struct ConditionFrame {
    /// Conditions of the branches seen so far.
    branches: Vec<String>,
    current: String,
    /// `#ifndef FOO_H` followed by `#define FOO_H`: an include guard, not a condition anyone
    /// wants to see.
    is_include_guard: bool,
}

/// Tracks the `#if` branches a position is in, as text.
#[derive(Default)]
pub(crate) struct ConditionStack {
    frames: Vec<ConditionFrame>,
    /// The macro an `#ifndef` just tested, while no other directive has followed.
    guard_candidate: Option<String>,
}

impl ConditionStack {
    pub(crate) fn apply(&mut self, directive: &str) {
        let (keyword, rest) = split_directive(directive);
        let rest = cap_condition(collapse_whitespace(rest));
        let guard_candidate = self.guard_candidate.take();
        if keyword == "define" {
            let defined = rest
                .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
                .next()
                .unwrap_or_default();
            if guard_candidate.as_deref() == Some(defined)
                && let Some(frame) = self.frames.last_mut()
            {
                frame.is_include_guard = true;
            }
            return;
        }
        match keyword {
            "if" | "ifdef" | "ifndef" => {
                let tested = match keyword {
                    "ifndef" => Some(rest.clone()),
                    _ => rest
                        .strip_prefix("!defined")
                        .map(|name| {
                            name.trim()
                                .trim_start_matches('(')
                                .trim_end_matches(')')
                                .trim()
                                .to_owned()
                        })
                        .filter(|name| is_simple_condition(name)),
                };
                let condition = match keyword {
                    "ifndef" => negate(&rest),
                    _ => rest,
                };
                self.guard_candidate = tested;
                self.frames.push(ConditionFrame {
                    branches: vec![condition.clone()],
                    current: condition,
                    is_include_guard: false,
                });
            }
            "elif" | "elifdef" | "elifndef" => {
                if let Some(frame) = self.frames.last_mut() {
                    let condition = if keyword == "elifndef" {
                        negate(&rest)
                    } else {
                        rest
                    };
                    frame.branches.push(condition.clone());
                    frame.current = condition;
                }
            }
            "else" => {
                if let Some(frame) = self.frames.last_mut() {
                    frame.current = match frame.branches.as_slice() {
                        [single] => negate(single),
                        branches => cap_condition(format!(
                            "!({})",
                            join_capped(branches.iter().map(String::as_str), " || ")
                        )),
                    };
                }
            }
            "endif" => {
                self.frames.pop();
            }
            _ => {}
        }
    }

    pub(crate) fn current(&self) -> Option<String> {
        let joined = join_capped(
            self.frames
                .iter()
                .filter(|frame| !frame.is_include_guard)
                .map(|frame| frame.current.as_str()),
            " && ",
        );
        if joined.is_empty() {
            None
        } else {
            Some(joined)
        }
    }
}

/// Conditions are shown next to members; deeply nested or generated `#if` chains would
/// otherwise put a copy of a long text on every member.
pub const MAX_CONDITION_CHARS: usize = 256;

fn cap_condition(mut condition: String) -> String {
    if condition.len() > MAX_CONDITION_CHARS {
        let mut end = MAX_CONDITION_CHARS;
        while !condition.is_char_boundary(end) {
            end -= 1;
        }
        condition.truncate(end);
        condition.push('\u{2026}');
    }
    condition
}

/// Joins `parts`, stopping once the text is longer than [`MAX_CONDITION_CHARS`], so that the
/// cost does not grow with the number of parts.
fn join_capped<'a>(parts: impl Iterator<Item = &'a str>, separator: &str) -> String {
    let mut output = String::new();
    for part in parts {
        if output.len() > MAX_CONDITION_CHARS {
            break;
        }
        if !output.is_empty() {
            output.push_str(separator);
        }
        output.push_str(part);
    }
    cap_condition(output)
}

/// Counts enumerator values, following `#if` branches: every branch continues from the value
/// at its `#if`, and after `#endif` the count is kept only when all branches agree.
#[derive(Default)]
struct EnumCounter {
    next: Option<i128>,
    started: bool,
    /// Per open `#if`: the value at its start and where each finished branch ended.
    frames: Vec<(Option<i128>, Vec<Option<i128>>)>,
}

impl EnumCounter {
    fn apply(&mut self, directive: &str) {
        let (keyword, _) = split_directive(directive);
        match keyword {
            "if" | "ifdef" | "ifndef" => {
                let start = self.current();
                self.frames.push((start, Vec::new()));
            }
            "elif" | "elifdef" | "elifndef" | "else" => {
                let current = self.current();
                if let Some((start, ends)) = self.frames.last_mut() {
                    ends.push(current);
                    self.next = *start;
                }
            }
            "endif" => {
                let current = self.current();
                if let Some((_, mut ends)) = self.frames.pop() {
                    ends.push(current);
                    let first = ends.first().copied().flatten();
                    self.next = if ends.iter().all(|end| *end == first) {
                        first
                    } else {
                        None
                    };
                    self.started = true;
                }
            }
            _ => {}
        }
    }

    fn current(&self) -> Option<i128> {
        if self.started { self.next } else { Some(0) }
    }

    fn next(&mut self, value_text: Option<&str>) -> (Option<i128>, bool) {
        let (value, inferred) = match value_text {
            Some(text) => (parse_integer_literal(text), false),
            None => (self.current(), true),
        };
        self.started = true;
        self.next = value.and_then(|value| value.checked_add(1));
        (value, inferred && value.is_some())
    }
}

/// An integer literal with an optional sign, parentheses and `u`/`l` suffixes.
pub(crate) fn parse_integer_literal(text: &str) -> Option<i128> {
    let mut text = text.trim();
    while let Some(inner) = text
        .strip_prefix('(')
        .and_then(|rest| rest.strip_suffix(')'))
    {
        text = inner.trim();
    }
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest.trim_start()),
        None => (false, text.strip_prefix('+').unwrap_or(text).trim_start()),
    };
    let digits = digits
        .trim_end_matches(['u', 'U', 'l', 'L'])
        .replace('\'', "");
    if digits.is_empty() {
        return None;
    }
    let value = if let Some(hex) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        i128::from_str_radix(hex, 16).ok()?
    } else if let Some(binary) = digits
        .strip_prefix("0b")
        .or_else(|| digits.strip_prefix("0B"))
    {
        i128::from_str_radix(binary, 2).ok()?
    } else if digits.len() > 1 && digits.starts_with('0') {
        i128::from_str_radix(digits.get(1..)?, 8).ok()?
    } else {
        digits.parse::<i128>().ok()?
    };
    Some(if negative {
        value.checked_neg()?
    } else {
        value
    })
}

/// The `#if` conditions in effect at `byte`, from the directives before it in the file.
pub fn enclosing_condition(source: &[u8], byte: usize) -> Option<String> {
    let end = byte.min(source.len());
    let mut stack = ConditionStack::default();
    let mut in_comment = false;
    let mut line_start = 0;
    while line_start < end {
        let line_end = source
            .get(line_start..end)
            .and_then(|rest| rest.iter().position(|&c| c == b'\n'))
            .map_or(end, |offset| line_start + offset);
        let line = source.get(line_start..line_end).unwrap_or_default();
        let trimmed_start = line
            .iter()
            .position(|c| !c.is_ascii_whitespace())
            .unwrap_or(line.len());
        if !in_comment && line.get(trimmed_start) == Some(&b'#') {
            let mut directive = line.to_vec();
            let mut next = line_end + 1;
            while directive.last().is_some_and(|&c| c == b'\\' || c == b'\r')
                && directive.iter().rev().find(|&&c| c != b'\r') == Some(&b'\\')
                && next < end
            {
                let continuation_end = source
                    .get(next..end)
                    .and_then(|rest| rest.iter().position(|&c| c == b'\n'))
                    .map_or(end, |offset| next + offset);
                directive.push(b' ');
                directive.extend_from_slice(source.get(next..continuation_end).unwrap_or_default());
                next = continuation_end + 1;
            }
            let text = String::from_utf8_lossy(&directive);
            let text = match text.find("//") {
                Some(position) => text.get(..position).unwrap_or_default().to_owned(),
                None => text.into_owned(),
            };
            let text = strip_block_comments(&text).replace('\\', " ");
            stack.apply(&text);
            line_start = next;
            continue;
        }
        let mut index = 0;
        while index < line.len() {
            if in_comment {
                if line.get(index) == Some(&b'*') && line.get(index + 1) == Some(&b'/') {
                    in_comment = false;
                    index += 2;
                    continue;
                }
            } else if line.get(index) == Some(&b'/') {
                match line.get(index + 1) {
                    Some(b'*') => {
                        in_comment = true;
                        index += 2;
                        continue;
                    }
                    Some(b'/') => break,
                    _ => {}
                }
            }
            index += 1;
        }
        line_start = line_end + 1;
    }
    stack.current()
}

fn strip_block_comments(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("/*") {
        output.push_str(rest.get(..start).unwrap_or_default());
        output.push(' ');
        rest = match rest.get(start + 2..).and_then(|after| after.find("*/")) {
            Some(end) => rest.get(start + 2 + end + 2..).unwrap_or_default(),
            None => "",
        };
    }
    output.push_str(rest);
    output
}

/// The `#include` paths in `source`, both `"..."` and `<...>`, with `/` separators.
pub fn include_paths(source: &[u8]) -> Vec<String> {
    let mut includes = Vec::new();
    for line in source.split(|&c| c == b'\n') {
        let line = line.trim_ascii();
        let Some(rest) = line.strip_prefix(b"#") else {
            continue;
        };
        let rest = rest.trim_ascii_start();
        let Some(rest) = rest
            .strip_prefix(b"include")
            .or_else(|| rest.strip_prefix(b"import"))
        else {
            continue;
        };
        let rest = rest.trim_ascii_start();
        let close = match rest.first() {
            Some(b'"') => b'"',
            Some(b'<') => b'>',
            _ => continue,
        };
        let Some(inner) = rest.get(1..) else {
            continue;
        };
        let Some(end) = inner.iter().position(|&c| c == close) else {
            continue;
        };
        let path = String::from_utf8_lossy(inner.get(..end).unwrap_or_default()).replace('\\', "/");
        if !path.is_empty() {
            includes.push(path);
        }
    }
    includes
}

// ---------------------------------------------------------------------------------------------
// The name under the cursor

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Confidence {
    /// The cursor is on the declaration itself.
    Declaration,
    /// Declared in the enclosing function (in the innermost block that has it).
    Local,
    Parameter,
    /// Declared at file scope in the same file.
    FileScope,
    /// Only a definition with the same name elsewhere was found.
    NameOnly,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VariableType {
    pub name: String,
    pub ty: TypeText,
    /// The name in the declaration that was found.
    pub declaration: Option<Range>,
    pub confidence: Confidence,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberStep {
    pub name: String,
    pub range: Range,
}

/// Where a member access chain starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainRoot {
    Variable(VariableType),
    /// `foo()->x`: the return type of `foo`, found through the index.
    Call {
        function: String,
        range: Range,
    },
    /// A cast, a compound literal or the variable a designated initializer belongs to.
    Type(TypeText),
    /// A name not declared in this file; the index may find a global.
    Unknown {
        name: String,
        range: Range,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberAccess {
    pub root: ChainRoot,
    /// Members from the root to the one under the cursor (last).
    pub members: Vec<MemberStep>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeAt {
    /// On a type name: `foo` in `struct foo` or `foo_t`.
    TypeName {
        name: String,
        tag: Option<TagKind>,
        range: Range,
    },
    /// On a variable, parameter or member whose declaration was found.
    Variable {
        variable: VariableType,
        range: Range,
    },
    /// On a member in `a->b.c`, `.field =` or a member declaration's use.
    MemberAccess { access: MemberAccess, range: Range },
    /// On a name that is not declared in this file: a macro, function or global elsewhere.
    Identifier { name: String, range: Range },
}

thread_local! {
    static TREE_PARSERS: RefCell<TreeParsers> = RefCell::new(TreeParsers::default());
}

#[derive(Default)]
struct TreeParsers {
    c: Option<Option<Parser>>,
    cpp: Option<Option<Parser>>,
}

fn parse_tree(
    source: &[u8],
    language: SourceLanguage,
    window: Option<std::ops::Range<usize>>,
) -> Option<Tree> {
    TREE_PARSERS.with(|parsers| {
        let mut parsers = parsers.borrow_mut();
        let slot = match language {
            SourceLanguage::C => &mut parsers.c,
            SourceLanguage::Cpp => &mut parsers.cpp,
        };
        let parser = slot.get_or_insert_with(|| {
            let ts_language: Language = match language {
                SourceLanguage::C => tree_sitter_c::LANGUAGE.into(),
                SourceLanguage::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            };
            let mut parser = Parser::new();
            match parser.set_language(&ts_language) {
                Ok(()) => Some(parser),
                Err(error) => {
                    log::error!("symbol_index: cannot load the {language:?} grammar: {error}");
                    None
                }
            }
        });
        let parser = parser.as_mut()?;
        let windowed = match window {
            Some(window) => {
                let start = position_of_byte(source, window.start);
                let end = position_of_byte(source, window.end);
                let range = tree_sitter::Range {
                    start_byte: start.byte as usize,
                    end_byte: end.byte as usize,
                    start_point: Point::new(start.row as usize, start.column as usize),
                    end_point: Point::new(end.row as usize, end.column as usize),
                };
                match parser.set_included_ranges(&[range]) {
                    Ok(()) => true,
                    Err(error) => {
                        log::debug!(
                            "symbol_index: cannot limit the parse to {window:?}: {error:?}"
                        );
                        false
                    }
                }
            }
            None => false,
        };
        let started = Instant::now();
        let mut progress = |_: &ParseState| {
            if started.elapsed().as_millis() > TYPE_AT_PARSE_TIMEOUT_MILLIS {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        };
        let options = ParseOptions::new().progress_callback(&mut progress);
        let mut read = |byte: usize, _: Point| source.get(byte..).unwrap_or_default();
        let tree = parser.parse_with_options(&mut read, None, Some(options));
        if tree.is_none() {
            parser.reset();
        }
        if windowed && let Err(error) = parser.set_included_ranges(&[]) {
            log::debug!("symbol_index: cannot reset the parsed ranges: {error:?}");
        }
        tree
    })
}

fn node_range(node: Node) -> Range {
    let position = |byte: usize, point: Point| Position {
        byte: to_u32(byte),
        row: to_u32(point.row),
        column: to_u32(point.column),
    };
    Range {
        start: position(node.start_byte(), node.start_position()),
        end: position(node.end_byte(), node.end_position()),
    }
}

/// Guesses what the name at `byte_offset` refers to, for showing the contents of its type
/// without a language server. Everything is decided by names in this file; see
/// [`crate::TypeResolver::describe_at`] for the part that needs the index.
pub fn type_at(source: &str, byte_offset: usize, language: SourceLanguage) -> Option<TypeAt> {
    type_at_bytes(source.as_bytes(), byte_offset, language, None)
}

/// [`type_at`] parsing only `window` (the function around the cursor, say), for large files
/// where parsing everything takes too long for a hover. Declarations outside the window are not
/// seen, so file-scope variables come back as [`TypeAt::Identifier`].
pub fn type_at_in_window(
    source: &str,
    byte_offset: usize,
    language: SourceLanguage,
    window: std::ops::Range<usize>,
) -> Option<TypeAt> {
    type_at_bytes(source.as_bytes(), byte_offset, language, Some(window))
}

pub(crate) fn type_at_bytes(
    source: &[u8],
    byte_offset: usize,
    language: SourceLanguage,
    window: Option<std::ops::Range<usize>>,
) -> Option<TypeAt> {
    if source.len() > MAX_TYPE_AT_SOURCE_BYTES || byte_offset > source.len() {
        return None;
    }
    let window = window
        .map(|window| window.start.min(source.len())..window.end.min(source.len()))
        .filter(|window| window.start <= byte_offset && byte_offset <= window.end);
    // The same rewrite as for the index, so that both see the same declarations.
    let normalized = crate::extract::normalize_for_parsing(source);
    let tree = parse_tree(&normalized, language, window)?;
    let root = tree.root_node();
    let finder = TypeFinder {
        source,
        language,
        root,
    };
    let at = |offset: usize| {
        root.descendant_for_byte_range(offset, offset)
            .filter(|node| finder.is_name_node(*node))
    };
    let node = at(byte_offset).or_else(|| byte_offset.checked_sub(1).and_then(at))?;
    finder.describe(node)
}

struct TypeFinder<'a, 'tree> {
    source: &'a [u8],
    language: SourceLanguage,
    root: Node<'tree>,
}

/// Ancestors followed through [`Node::parent`] when the path from the root cannot be found.
const MAX_PARENT_WALK: usize = 4096;

const DECLARATOR_KINDS: &[&str] = &[
    "pointer_declarator",
    "array_declarator",
    "function_declarator",
    "parenthesized_declarator",
    "attributed_declarator",
    "init_declarator",
    "reference_declarator",
];

impl<'a, 'tree> TypeFinder<'a, 'tree> {
    fn text(&self, node: Node) -> String {
        String::from_utf8_lossy(self.source.get(node.byte_range()).unwrap_or_default()).into_owned()
    }

    fn is_name_node(&self, node: Node) -> bool {
        matches!(
            node.kind(),
            "identifier" | "type_identifier" | "field_identifier" | "namespace_identifier"
        )
    }

    fn describe(&self, node: Node) -> Option<TypeAt> {
        let name = self.text(node);
        let range = node_range(node);
        let parent = node.parent();
        match node.kind() {
            "type_identifier" => {
                let tag = parent.and_then(|parent| {
                    let kind = match parent.kind() {
                        "struct_specifier" => TagKind::Struct,
                        "union_specifier" => TagKind::Union,
                        "enum_specifier" => TagKind::Enum,
                        "class_specifier" => TagKind::Class,
                        _ => return None,
                    };
                    (parent.child_by_field_name("name") == Some(node)).then_some(kind)
                });
                // A typedef name declared right here (`typedef struct {...} foo_t;`).
                Some(TypeAt::TypeName { name, tag, range })
            }
            "field_identifier" => {
                let parent = parent?;
                match parent.kind() {
                    "field_expression" if parent.child_by_field_name("field") == Some(node) => {
                        let access = self.member_chain(parent, name, range)?;
                        Some(TypeAt::MemberAccess { access, range })
                    }
                    "field_designator" => {
                        let access = self.designated_chain(parent, name, range)?;
                        Some(TypeAt::MemberAccess { access, range })
                    }
                    _ => {
                        if let Some(variable) = self.declared_here(node, &name) {
                            return Some(TypeAt::Variable { variable, range });
                        }
                        Some(TypeAt::Identifier { name, range })
                    }
                }
            }
            "identifier" => {
                if let Some(variable) = self.declared_here(node, &name) {
                    return Some(TypeAt::Variable { variable, range });
                }
                if let Some(parent) = parent
                    && parent.kind() == "call_expression"
                    && parent.child_by_field_name("function") == Some(node)
                {
                    return Some(TypeAt::Identifier { name, range });
                }
                match self.lookup(node, &name) {
                    Some(variable) => Some(TypeAt::Variable { variable, range }),
                    None => Some(TypeAt::Identifier { name, range }),
                }
            }
            _ => None,
        }
    }

    /// When `node` is the name declared by a declaration, the declared type.
    fn declared_here(&self, node: Node, name: &str) -> Option<VariableType> {
        let mut child = node;
        let mut parent = node.parent()?;
        loop {
            let kind = parent.kind();
            if DECLARATOR_KINDS.contains(&kind) {
                let via_declarator = parent.child_by_field_name("declarator") == Some(child)
                    || kind == "parenthesized_declarator"
                    || kind == "attributed_declarator";
                if !via_declarator {
                    return None;
                }
                if kind == "function_declarator" {
                    // The name of a function, not of a variable.
                    return None;
                }
                child = parent;
                parent = parent.parent()?;
                continue;
            }
            if matches!(
                kind,
                "declaration" | "parameter_declaration" | "field_declaration"
            ) {
                let is_declarator = {
                    let mut cursor = parent.walk();
                    parent
                        .children_by_field_name("declarator", &mut cursor)
                        .any(|declarator| declarator == child)
                };
                if !is_declarator {
                    return None;
                }
                let declared = self.declared_name(parent, name)?;
                return Some(VariableType {
                    name: name.to_owned(),
                    ty: declared.ty,
                    declaration: Some(declared.name_range),
                    confidence: Confidence::Declaration,
                });
            }
            return None;
        }
    }

    fn declared_name(&self, declaration: Node, name: &str) -> Option<DeclaredName> {
        let range = node_range(declaration);
        declared_names_in_file(
            self.source,
            range.start,
            declaration.end_byte(),
            self.language,
        )
        .into_iter()
        .find(|declared| declared.name == name && !declared.ty.is_function)
    }

    /// The nodes from the root down to `node`'s parent. Each [`Node::parent`] call searches
    /// down from the root, so walking up through deeply nested blocks one parent at a time
    /// would take time quadratic in the depth; a cursor finds the whole path in one descent.
    fn ancestors(&self, node: Node<'tree>) -> Vec<Node<'tree>> {
        let target = node.byte_range();
        let mut path = Vec::new();
        let mut cursor = self.root.walk();
        loop {
            let current = cursor.node();
            if current == node {
                return path;
            }
            path.push(current);
            let mut found = cursor.goto_first_child_for_byte(target.start).is_some();
            while found {
                let child = cursor.node();
                if child.start_byte() > target.start {
                    found = false;
                } else if child == node || target.end <= child.end_byte() {
                    break;
                } else {
                    found = cursor.goto_next_sibling();
                }
            }
            if !found {
                break;
            }
        }
        let mut path = Vec::new();
        let mut current = node;
        while let Some(parent) = current.parent() {
            if path.len() >= MAX_PARENT_WALK {
                break;
            }
            path.push(parent);
            current = parent;
        }
        path.reverse();
        path
    }

    /// Finds the declaration of `name` visible at `node`, innermost scope first.
    fn lookup(&self, node: Node<'tree>, name: &str) -> Option<VariableType> {
        let use_start = node.start_byte();
        let mut in_function = false;
        for parent in self.ancestors(node).into_iter().rev() {
            match parent.kind() {
                "compound_statement" | "declaration_list" | "translation_unit" => {
                    let confidence = if parent.kind() == "translation_unit"
                        || (parent.kind() == "declaration_list" && !in_function)
                    {
                        Confidence::FileScope
                    } else {
                        Confidence::Local
                    };
                    if let Some(found) = self.search_block(parent, name, use_start, confidence) {
                        return Some(found);
                    }
                }
                "for_statement" | "for_range_loop" => {
                    if let Some(initializer) = parent
                        .child_by_field_name("initializer")
                        .filter(|initializer| initializer.kind() == "declaration")
                        && let Some(found) =
                            self.from_declaration(initializer, name, Confidence::Local)
                    {
                        return Some(found);
                    }
                }
                "function_definition" | "lambda_expression" => {
                    in_function = true;
                    if let Some(found) = self.search_parameters(parent, name) {
                        return Some(found);
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn from_declaration(
        &self,
        declaration: Node,
        name: &str,
        confidence: Confidence,
    ) -> Option<VariableType> {
        let declared = self.declared_name(declaration, name)?;
        Some(VariableType {
            name: name.to_owned(),
            ty: declared.ty,
            declaration: Some(declared.name_range),
            confidence,
        })
    }

    fn search_block(
        &self,
        block: Node,
        name: &str,
        use_start: usize,
        confidence: Confidence,
    ) -> Option<VariableType> {
        let mut found = None;
        self.visit_declarations(block, 0, &mut |declaration| {
            if declaration.start_byte() > use_start {
                return;
            }
            // Reading every declaration of a large file is what makes a lookup slow; most of
            // them cannot mention the name.
            let mentions_name = self
                .source
                .get(declaration.byte_range())
                .is_some_and(|text| {
                    text.windows(name.len().max(1))
                        .any(|window| window == name.as_bytes())
                });
            if !mentions_name {
                return;
            }
            if let Some(variable) = self.from_declaration(declaration, name, confidence) {
                found = Some(variable);
            }
        });
        found
    }

    /// Calls `visit` on the declarations directly in `block`, in order, looking through
    /// preprocessor conditionals and `extern "C"` blocks but not into nested scopes.
    fn visit_declarations(&self, block: Node, depth: usize, visit: &mut dyn FnMut(Node)) {
        if depth > 16 {
            return;
        }
        let mut cursor = block.walk();
        for child in block.named_children(&mut cursor) {
            match child.kind() {
                "declaration" => visit(child),
                "preproc_if" | "preproc_ifdef" | "preproc_else" | "preproc_elif"
                | "preproc_elifdef" => self.visit_declarations(child, depth + 1, visit),
                "linkage_specification" => {
                    if let Some(body) = child.child_by_field_name("body") {
                        if body.kind() == "declaration" {
                            visit(body);
                        } else {
                            self.visit_declarations(body, depth + 1, visit);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn search_parameters(&self, function: Node, name: &str) -> Option<VariableType> {
        let mut declarator = function.child_by_field_name("declarator")?;
        for _ in 0..16 {
            if declarator.kind() == "function_declarator"
                || declarator.kind() == "abstract_function_declarator"
            {
                break;
            }
            declarator = declarator
                .child_by_field_name("declarator")
                .or_else(|| declarator.named_child(0))?;
        }
        let parameters = declarator.child_by_field_name("parameters")?;
        let mut cursor = parameters.walk();
        for parameter in parameters.named_children(&mut cursor) {
            if !matches!(
                parameter.kind(),
                "parameter_declaration" | "optional_parameter_declaration"
            ) {
                continue;
            }
            if let Some(found) = self.from_declaration(parameter, name, Confidence::Parameter) {
                return Some(found);
            }
        }
        None
    }

    fn member_chain(
        &self,
        field_expression: Node,
        name: String,
        range: Range,
    ) -> Option<MemberAccess> {
        let mut members = vec![MemberStep { name, range }];
        let mut expression = field_expression.child_by_field_name("argument")?;
        for _ in 0..64 {
            match expression.kind() {
                "field_expression" => {
                    let field = expression.child_by_field_name("field")?;
                    members.push(MemberStep {
                        name: self.text(field),
                        range: node_range(field),
                    });
                    expression = expression.child_by_field_name("argument")?;
                }
                "parenthesized_expression" => expression = expression.named_child(0)?,
                "pointer_expression" | "subscript_expression" => {
                    expression = expression.child_by_field_name("argument")?;
                }
                "cast_expression" | "compound_literal_expression" => {
                    let descriptor = expression.child_by_field_name("type")?;
                    let ty = parse_type_text(&self.text(descriptor), self.language)?;
                    members.reverse();
                    return Some(MemberAccess {
                        root: ChainRoot::Type(ty),
                        members,
                    });
                }
                "call_expression" => {
                    let function = expression.child_by_field_name("function")?;
                    members.reverse();
                    let root = if function.kind() == "identifier" {
                        ChainRoot::Call {
                            function: self.text(function),
                            range: node_range(function),
                        }
                    } else {
                        ChainRoot::Unknown {
                            name: self.text(function),
                            range: node_range(function),
                        }
                    };
                    return Some(MemberAccess { root, members });
                }
                "identifier" => {
                    let root_name = self.text(expression);
                    let root = match self.lookup(expression, &root_name) {
                        Some(variable) => ChainRoot::Variable(variable),
                        None => ChainRoot::Unknown {
                            name: root_name,
                            range: node_range(expression),
                        },
                    };
                    members.reverse();
                    return Some(MemberAccess { root, members });
                }
                _ => {
                    members.reverse();
                    return Some(MemberAccess {
                        root: ChainRoot::Unknown {
                            name: self.text(expression),
                            range: node_range(expression),
                        },
                        members,
                    });
                }
            }
        }
        None
    }

    /// `.a.b = value` inside `struct foo x = { ... }`: the chain from the initialized type.
    fn designated_chain(
        &self,
        designator: Node,
        name: String,
        range: Range,
    ) -> Option<MemberAccess> {
        let mut members = vec![MemberStep { name, range }];
        let pair = designator.parent()?;
        {
            // Earlier designators of the same pair: `.a.b = 1` on `b`.
            let mut cursor = pair.walk();
            let designators: Vec<Node> = pair
                .children_by_field_name("designator", &mut cursor)
                .collect();
            let position = designators.iter().position(|node| *node == designator)?;
            for previous in designators.get(..position).unwrap_or_default().iter().rev() {
                if previous.kind() == "field_designator"
                    && let Some(field) = previous.named_child(0)
                {
                    members.push(MemberStep {
                        name: self.text(field),
                        range: node_range(field),
                    });
                }
            }
        }
        let mut current = pair.parent()?;
        for _ in 0..32 {
            match current.kind() {
                "initializer_list" => current = current.parent()?,
                "initializer_pair" => {
                    let mut cursor = current.walk();
                    let designators: Vec<Node> = current
                        .children_by_field_name("designator", &mut cursor)
                        .collect();
                    for designator in designators.iter().rev() {
                        if designator.kind() == "field_designator"
                            && let Some(field) = designator.named_child(0)
                        {
                            members.push(MemberStep {
                                name: self.text(field),
                                range: node_range(field),
                            });
                        }
                    }
                    current = current.parent()?;
                }
                "init_declarator" => {
                    let declaration = current.parent()?;
                    let declarator = current.child_by_field_name("declarator")?;
                    let declared_name = self.declarator_name(declarator)?;
                    let declared = self.declared_name(declaration, &declared_name)?;
                    members.reverse();
                    return Some(MemberAccess {
                        root: ChainRoot::Type(declared.ty),
                        members,
                    });
                }
                "compound_literal_expression" => {
                    let descriptor = current.child_by_field_name("type")?;
                    let ty = parse_type_text(&self.text(descriptor), self.language)?;
                    members.reverse();
                    return Some(MemberAccess {
                        root: ChainRoot::Type(ty),
                        members,
                    });
                }
                _ => return None,
            }
        }
        None
    }

    fn declarator_name(&self, declarator: Node) -> Option<String> {
        let mut node = declarator;
        for _ in 0..32 {
            match node.kind() {
                "identifier" | "field_identifier" => return Some(self.text(node)),
                "parenthesized_declarator" | "attributed_declarator" => {
                    node = node.named_child(0)?
                }
                _ => node = node.child_by_field_name("declarator")?,
            }
        }
        None
    }
}

/// The names declared by the declaration at `start..end` of a file. `start` must be a
/// consistent position of `start.byte` in `source`.
pub(crate) fn declared_names_in_file(
    source: &[u8],
    start: Position,
    end: usize,
    language: SourceLanguage,
) -> Vec<DeclaredName> {
    let lexed = lex(source, start.byte as usize, end, start, false);
    let context = Context::new(source, language, &lexed);
    context.declared_names(&lexed.tokens)
}
