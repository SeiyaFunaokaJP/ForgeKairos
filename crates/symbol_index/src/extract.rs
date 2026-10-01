use std::{borrow::Cow, cell::RefCell, ops::ControlFlow};

use tree_sitter::{Language, Node, ParseOptions, ParseState, Parser, Point, Tree};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SourceLanguage {
    C,
    Cpp,
}

/// A location in a file. `row` is 0-based and `column` counts UTF-8 bytes from the start of the
/// line, so CRLF line endings and multi-byte characters earlier in the line do not shift it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Position {
    pub byte: u32,
    pub row: u32,
    pub column: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

impl Range {
    /// The range of a single-line token starting at `start`.
    pub(crate) fn token(start: Position, length: usize) -> Self {
        let length = u32::try_from(length).unwrap_or(u32::MAX);
        Self {
            start,
            end: Position {
                byte: start.byte.saturating_add(length),
                row: start.row,
                column: start.column.saturating_add(length),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DefinitionKind {
    Function,
    Method,
    /// `#define NAME value`
    Macro,
    /// `#define NAME(args) value`
    FunctionMacro,
    Struct,
    Union,
    Enum,
    EnumConstant,
    Typedef,
    Variable,
    Class,
}

impl DefinitionKind {
    pub(crate) const ALL: [DefinitionKind; 11] = [
        DefinitionKind::Function,
        DefinitionKind::Method,
        DefinitionKind::Macro,
        DefinitionKind::FunctionMacro,
        DefinitionKind::Struct,
        DefinitionKind::Union,
        DefinitionKind::Enum,
        DefinitionKind::EnumConstant,
        DefinitionKind::Typedef,
        DefinitionKind::Variable,
        DefinitionKind::Class,
    ];

    pub(crate) fn to_byte(self) -> u8 {
        self as u8
    }

    pub(crate) fn from_byte(byte: u8) -> Option<Self> {
        Self::ALL.get(usize::from(byte)).copied()
    }

    pub fn is_function_like(self) -> bool {
        matches!(self, DefinitionKind::Function | DefinitionKind::Method)
    }

    pub fn is_macro(self) -> bool {
        matches!(self, DefinitionKind::Macro | DefinitionKind::FunctionMacro)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CallKind {
    /// `foo(...)`
    Direct,
    /// `ops->open(...)` or `object.method(...)`; the recorded name is the field name.
    Member,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AssignmentKind {
    /// `.open = foo_open` inside an initializer list.
    DesignatedInitializer,
    /// `ops->open = foo_open;`
    Assignment,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct ExtractedDefinition {
    pub name: String,
    pub kind: DefinitionKind,
    pub is_static: bool,
    pub is_declaration: bool,
    pub name_range: Range,
    pub item_range: Range,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct ExtractedCall {
    pub callee: String,
    /// Index into the file's definitions of the function (or macro, for calls found in a macro
    /// body) containing the call.
    pub caller: Option<u32>,
    pub kind: CallKind,
    /// Found by scanning the text of a `#define` body rather than from the syntax tree.
    pub in_macro_body: bool,
    pub position: Position,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct ExtractedAssignment {
    pub field: String,
    pub value: String,
    /// The struct type being initialized, when it can be read from the declaration.
    pub container: Option<String>,
    pub kind: AssignmentKind,
    /// Position of the assigned value (the function name).
    pub position: Position,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ExtractedFile {
    pub definitions: Vec<ExtractedDefinition>,
    pub calls: Vec<ExtractedCall>,
    pub assignments: Vec<ExtractedAssignment>,
    pub has_syntax_errors: bool,
    pub parse_failed: bool,
}

pub(crate) fn language_for_path(path: &str) -> Option<LanguageGuess> {
    let extension = path.rsplit_once('.')?.1;
    if extension.contains('/') {
        return None;
    }
    match extension {
        "c" => Some(LanguageGuess::Known(SourceLanguage::C)),
        "h" => Some(LanguageGuess::Header),
        "cc" | "cpp" | "cxx" | "c++" | "C" | "hh" | "hpp" | "hxx" | "h++" | "H" | "ipp" | "tcc"
        | "inl" => Some(LanguageGuess::Known(SourceLanguage::Cpp)),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LanguageGuess {
    Known(SourceLanguage),
    /// `.h` is shared by C and C++; the contents decide.
    Header,
}

impl LanguageGuess {
    pub(crate) fn resolve(self, contents: &[u8]) -> SourceLanguage {
        match self {
            LanguageGuess::Known(language) => language,
            LanguageGuess::Header => {
                // Parsing a C header with the C++ grammar mostly works, but C code that uses
                // C++ keywords as identifiers (`class`, `new`) does not, so only switch on
                // constructs that cannot appear in C.
                if contains(contents, b"namespace ")
                    || contains(contents, b"template<")
                    || contains(contents, b"template <")
                {
                    SourceLanguage::Cpp
                } else {
                    SourceLanguage::C
                }
            }
        }
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Parses `source` and extracts definitions, calls and function pointer assignments.
/// `should_abort` is polled during parsing so that cancellation and the per-file time limit
/// can stop a pathological parse.
pub(crate) fn extract(
    source: &[u8],
    language: SourceLanguage,
    should_abort: &dyn Fn() -> bool,
) -> ExtractedFile {
    let source = normalize_for_parsing(source);
    let source = source.as_ref();
    PARSERS.with(|parsers| {
        let mut parsers = parsers.borrow_mut();
        let Some(grammar) = parsers.get(language) else {
            return ExtractedFile {
                parse_failed: true,
                ..ExtractedFile::default()
            };
        };
        let Some(tree) = grammar.parse(source, should_abort) else {
            return ExtractedFile {
                parse_failed: true,
                ..ExtractedFile::default()
            };
        };
        let mut walker = Walker::new(source, language, &grammar.table);
        walker.walk(&tree);
        walker.output
    })
}

/// Rewrites what tree-sitter-c misparses, without moving any byte, so that offsets, rows and
/// columns still refer to the original text:
///
/// - A line continuation before CRLF in a `#define`. The grammar only continues a directive at
///   a backslash followed by LF, so in a CRLF file every multi-line macro would end after its
///   first line and the rest would parse as code, swallowing the function that follows. The
///   backslash and the carriage return trade places.
/// - A block comment with more of a `#define` after it. The grammar cannot continue a macro
///   body after a comment, so the comment becomes spaces. A line break inside it gets a
///   continuation: comments are removed before directives are read, so it does not end the
///   directive, but the grammar would end it there.
/// - A line holding nothing but an upper-case macro name inside a struct, union or class body,
///   such as `SUBTREE_BITS` that expands to several members with their semicolons. The grammar
///   reads it as the type of the next member, and before the closing brace as a member that
///   never ends, which takes the rest of the file into the struct. The name becomes spaces.
///
/// Comments outside `#define` directives are left alone; the grammar handles them there, and
/// turning them into spaces would break directives such as `#include` that must end right
/// after their last token.
pub(crate) fn normalize_for_parsing(source: &[u8]) -> Cow<'_, [u8]> {
    fn is_crlf_continuation(source: &[u8], index: usize) -> bool {
        source.get(index..index.saturating_add(3)) == Some(b"\\\r\n".as_slice())
    }
    fn directive_keyword(source: &[u8], hash: usize) -> &[u8] {
        let mut start = hash + 1;
        while matches!(source.get(start), Some(b' ' | b'\t')) {
            start += 1;
        }
        let mut end = start;
        while source
            .get(end)
            .is_some_and(|&byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            end += 1;
        }
        source.get(start..end).unwrap_or_default()
    }
    fn set(output: &mut Option<Vec<u8>>, source: &[u8], index: usize, byte: u8) {
        let buffer = output.get_or_insert_with(|| source.to_vec());
        if let Some(slot) = buffer.get_mut(index) {
            *slot = byte;
        }
    }
    fn swap_continuation(output: &mut Option<Vec<u8>>, source: &[u8], index: usize) {
        set(output, source, index, b'\r');
        set(output, source, index + 1, b'\\');
    }

    let mut output: Option<Vec<u8>> = None;
    let mut index = 0;
    let mut at_line_start = true;
    let mut in_define = false;
    let mut in_directive = false;
    let mut scope = BraceScope::default();
    while let Some(&byte) = source.get(index) {
        if at_line_start && !matches!(byte, b' ' | b'\t' | b'\r' | b'\n') {
            at_line_start = false;
            in_directive = byte == b'#';
            in_define = in_directive && directive_keyword(source, index) == b"define";
            if in_directive {
                scope.directive(directive_keyword(source, index));
            } else if let Some(end) = scope.lone_macro_line(source, index) {
                for position in index..end {
                    set(&mut output, source, position, b' ');
                }
                index = end;
                continue;
            }
        }
        if is_crlf_continuation(source, index) {
            if in_define {
                swap_continuation(&mut output, source, index);
            }
            index += 3;
        } else if byte == b'"' || byte == b'\'' {
            // Skipped so that `"/*"` does not start a comment. A literal ends at its closing
            // quote or, unterminated, at the end of the line.
            index += 1;
            while let Some(&inner) = source.get(index) {
                if inner == byte || inner == b'\n' || is_crlf_continuation(source, index) {
                    break;
                }
                index += if inner == b'\\' { 2 } else { 1 };
            }
            if source.get(index) == Some(&byte) {
                index += 1;
            }
            if !in_directive {
                scope.other();
            }
        } else if byte == b'/' && source.get(index + 1) == Some(&b'/') {
            while source.get(index).is_some_and(|&inner| inner != b'\n') {
                index += 1;
            }
        } else if byte == b'/' && source.get(index + 1) == Some(&b'*') {
            let start = index;
            let end = source
                .get(index + 2..)
                .and_then(|rest| rest.windows(2).position(|window| window == b"*/"))
                .map_or(source.len(), |offset| index + 2 + offset + 2);
            let mut after = end;
            while matches!(source.get(after), Some(b' ' | b'\t')) {
                after += 1;
            }
            let macro_continues = match source.get(after) {
                None | Some(b'\n') => false,
                Some(b'\r') => source.get(after + 1) != Some(&b'\n'),
                Some(_) => true,
            };
            if !(in_define && macro_continues) {
                index = end;
                continue;
            }
            while index < end {
                if is_crlf_continuation(source, index) {
                    swap_continuation(&mut output, source, index);
                    index += 3;
                    continue;
                }
                match source.get(index) {
                    Some(b'\\') if source.get(index + 1) == Some(&b'\n') => {}
                    Some(b'\r') => {}
                    Some(b'\n') if index.saturating_sub(1) > start => {
                        let before = index - 1;
                        if source.get(before) == Some(&b'\r') {
                            swap_continuation(&mut output, source, before - 1);
                        } else {
                            set(&mut output, source, before, b'\\');
                        }
                    }
                    Some(b'\n') => {}
                    _ => set(&mut output, source, index, b' '),
                }
                index += 1;
            }
        } else if !in_directive && (byte.is_ascii_alphanumeric() || byte == b'_') {
            let start = index;
            while source
                .get(index)
                .is_some_and(|&inner| inner.is_ascii_alphanumeric() || inner == b'_')
            {
                index += 1;
            }
            scope.word(source.get(start..index).unwrap_or_default());
        } else {
            let continued =
                index.checked_sub(1).and_then(|before| source.get(before)) == Some(&b'\\');
            if byte == b'\n' && !continued {
                in_define = false;
                in_directive = false;
                at_line_start = true;
            } else if !in_directive && !byte.is_ascii_whitespace() {
                scope.punctuation(byte);
            }
            index += 1;
        }
    }
    output.map_or(Cow::Borrowed(source), Cow::Owned)
}

/// Braces opened deeper than this are counted but not classified, which bounds the copies made
/// at `#if`.
const MAX_TRACKED_BRACES: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq)]
enum LastToken {
    /// `;`, `{` or `}`: the next token starts a declaration.
    Boundary,
    AggregateKeyword,
    Identifier,
    Other,
}

/// Follows the braces of a file closely enough to tell whether a line is directly inside a
/// struct, union or class body, for [`normalize_for_parsing`].
struct BraceScope {
    /// Per open brace, whether it opens a struct, union or class body.
    braces: Vec<bool>,
    untracked_braces: usize,
    /// The braces at each open `#if`: every branch starts from them.
    conditions: Vec<(Vec<bool>, usize)>,
    untracked_conditions: usize,
    /// `struct`, `union` or `class` was seen since the last `;`, `,`, `=` or brace.
    aggregate_pending: bool,
    parentheses: usize,
    last: LastToken,
}

impl Default for BraceScope {
    fn default() -> Self {
        Self {
            braces: Vec::new(),
            untracked_braces: 0,
            conditions: Vec::new(),
            untracked_conditions: 0,
            aggregate_pending: false,
            parentheses: 0,
            last: LastToken::Boundary,
        }
    }
}

impl BraceScope {
    fn word(&mut self, word: &[u8]) {
        if matches!(word, b"struct" | b"union" | b"class") {
            self.aggregate_pending = true;
            self.last = LastToken::AggregateKeyword;
        } else if word.first().is_some_and(|byte| byte.is_ascii_digit()) {
            self.last = LastToken::Other;
        } else {
            self.last = LastToken::Identifier;
        }
    }

    fn other(&mut self) {
        self.last = LastToken::Other;
    }

    fn punctuation(&mut self, byte: u8) {
        match byte {
            b'(' => {
                self.parentheses = self.parentheses.saturating_add(1);
                self.last = LastToken::Other;
            }
            b')' => {
                self.parentheses = self.parentheses.saturating_sub(1);
                self.last = LastToken::Other;
            }
            b'{' => {
                // `struct name {` or `struct {`, possibly with attributes in between, but not
                // `struct foo *f(void) {` or `= {`.
                let is_aggregate = self.aggregate_pending
                    && self.parentheses == 0
                    && matches!(
                        self.last,
                        LastToken::Identifier | LastToken::AggregateKeyword
                    );
                if self.braces.len() < MAX_TRACKED_BRACES && self.untracked_braces == 0 {
                    self.braces.push(is_aggregate);
                } else {
                    self.untracked_braces += 1;
                }
                self.aggregate_pending = false;
                self.last = LastToken::Boundary;
            }
            b'}' => {
                if self.untracked_braces > 0 {
                    self.untracked_braces -= 1;
                } else {
                    self.braces.pop();
                }
                self.aggregate_pending = false;
                self.last = LastToken::Boundary;
            }
            b';' => {
                if self.parentheses == 0 {
                    self.aggregate_pending = false;
                }
                self.last = LastToken::Boundary;
            }
            b',' | b'=' => {
                if self.parentheses == 0 {
                    self.aggregate_pending = false;
                }
                self.last = LastToken::Other;
            }
            _ => self.last = LastToken::Other,
        }
    }

    fn directive(&mut self, keyword: &[u8]) {
        match keyword {
            b"if" | b"ifdef" | b"ifndef" => {
                if self.conditions.len() < MAX_TRACKED_BRACES && self.untracked_conditions == 0 {
                    self.conditions
                        .push((self.braces.clone(), self.untracked_braces));
                } else {
                    self.untracked_conditions += 1;
                }
            }
            b"elif" | b"elifdef" | b"elifndef" | b"else" => {
                if self.untracked_conditions == 0
                    && let Some((braces, untracked)) = self.conditions.last()
                {
                    self.braces.clone_from(braces);
                    self.untracked_braces = *untracked;
                }
            }
            b"endif" => {
                if self.untracked_conditions > 0 {
                    self.untracked_conditions -= 1;
                } else {
                    self.conditions.pop();
                }
            }
            _ => {}
        }
    }

    /// When the line starting at `start` is a lone upper-case name at the start of a member
    /// declaration, the end of the name.
    fn lone_macro_line(&self, source: &[u8], start: usize) -> Option<usize> {
        if self.braces.last() != Some(&true)
            || self.untracked_braces > 0
            || self.parentheses > 0
            || self.last != LastToken::Boundary
        {
            return None;
        }
        let mut end = start;
        while source
            .get(end)
            .is_some_and(|&byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        {
            end += 1;
        }
        let name = source.get(start..end)?;
        let has_letter = name.iter().any(|byte| byte.is_ascii_uppercase());
        if name.len() < 2 || !has_letter || name.first().is_some_and(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let mut after = end;
        while matches!(source.get(after), Some(b' ' | b'\t')) {
            after += 1;
        }
        match source.get(after) {
            None | Some(b'\n') => Some(end),
            Some(b'\r') if source.get(after + 1) == Some(&b'\n') => Some(end),
            Some(b'/') if source.get(after + 1) == Some(&b'/') => Some(end),
            _ => None,
        }
    }
}

thread_local! {
    static PARSERS: RefCell<Parsers> = RefCell::new(Parsers::default());
}

#[derive(Default)]
struct Parsers {
    c: Option<Result<Grammar, ()>>,
    cpp: Option<Result<Grammar, ()>>,
}

impl Parsers {
    fn get(&mut self, language: SourceLanguage) -> Option<&mut Grammar> {
        let slot = match language {
            SourceLanguage::C => &mut self.c,
            SourceLanguage::Cpp => &mut self.cpp,
        };
        let grammar = slot.get_or_insert_with(|| {
            let ts_language: Language = match language {
                SourceLanguage::C => tree_sitter_c::LANGUAGE.into(),
                SourceLanguage::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            };
            Grammar::new(ts_language).map_err(|error| {
                log::error!("symbol_index: cannot load the {language:?} grammar: {error:#}");
            })
        });
        grammar.as_mut().ok()
    }
}

struct Grammar {
    parser: Parser,
    table: KindTable,
}

impl Grammar {
    fn new(language: Language) -> anyhow::Result<Self> {
        let mut parser = Parser::new();
        parser.set_language(&language)?;
        Ok(Self {
            table: KindTable::new(&language),
            parser,
        })
    }

    fn parse(&mut self, source: &[u8], should_abort: &dyn Fn() -> bool) -> Option<Tree> {
        let mut progress = |_: &ParseState| {
            if should_abort() {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        };
        let options = ParseOptions::new().progress_callback(&mut progress);
        let mut read = |byte: usize, _: Point| source.get(byte..).unwrap_or_default();
        let tree = self
            .parser
            .parse_with_options(&mut read, None, Some(options));
        if tree.is_none() {
            // An aborted parse leaves state behind that the next parse would try to resume.
            self.parser.reset();
        }
        tree
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Other,
    FunctionDefinition,
    Declaration,
    FieldDeclaration,
    PreprocDef,
    PreprocFunctionDef,
    PreprocParams,
    StructSpecifier,
    UnionSpecifier,
    EnumSpecifier,
    ClassSpecifier,
    TypeDefinition,
    Enumerator,
    CallExpression,
    InitializerPair,
    InitializerList,
    InitDeclarator,
    AssignmentExpression,
    CompoundLiteralExpression,
    TypeDescriptor,
    Identifier,
    FieldIdentifier,
    TypeIdentifier,
    QualifiedIdentifier,
    TemplateFunction,
    TemplateMethod,
    TemplateType,
    DestructorName,
    OperatorName,
    FunctionDeclarator,
    PointerDeclarator,
    ReferenceDeclarator,
    ParenthesizedDeclarator,
    AttributedDeclarator,
    ArrayDeclarator,
    FieldExpression,
    PointerExpression,
    ParenthesizedExpression,
    FieldDesignator,
    StorageClassSpecifier,
    CompoundStatement,
    ExpressionStatement,
}

struct KindTable {
    kinds: Vec<Kind>,
    field_declarator: u16,
    field_body: u16,
    field_name: u16,
    field_value: u16,
    field_function: u16,
    field_field: u16,
    field_designator: u16,
    field_left: u16,
    field_right: u16,
    field_type: u16,
    field_argument: u16,
    field_operator: u16,
    field_parameters: u16,
    field_arguments: u16,
}

impl KindTable {
    fn new(language: &Language) -> Self {
        let count = language.node_kind_count();
        let mut kinds = Vec::with_capacity(count);
        for id in 0..count {
            let kind = u16::try_from(id)
                .ok()
                .filter(|&id| language.node_kind_is_named(id))
                .and_then(|id| language.node_kind_for_id(id))
                .map_or(Kind::Other, kind_from_name);
            kinds.push(kind);
        }
        let field = |name: &str| language.field_id_for_name(name).map_or(0, |id| id.get());
        Self {
            kinds,
            field_declarator: field("declarator"),
            field_body: field("body"),
            field_name: field("name"),
            field_value: field("value"),
            field_function: field("function"),
            field_field: field("field"),
            field_designator: field("designator"),
            field_left: field("left"),
            field_right: field("right"),
            field_type: field("type"),
            field_argument: field("argument"),
            field_operator: field("operator"),
            field_parameters: field("parameters"),
            field_arguments: field("arguments"),
        }
    }

    fn kind(&self, node: Node) -> Kind {
        self.kinds
            .get(usize::from(node.kind_id()))
            .copied()
            .unwrap_or(Kind::Other)
    }
}

fn kind_from_name(name: &str) -> Kind {
    match name {
        "function_definition" => Kind::FunctionDefinition,
        "declaration" => Kind::Declaration,
        "field_declaration" => Kind::FieldDeclaration,
        "preproc_def" => Kind::PreprocDef,
        "preproc_function_def" => Kind::PreprocFunctionDef,
        "preproc_params" => Kind::PreprocParams,
        "struct_specifier" => Kind::StructSpecifier,
        "union_specifier" => Kind::UnionSpecifier,
        "enum_specifier" => Kind::EnumSpecifier,
        "class_specifier" => Kind::ClassSpecifier,
        "type_definition" => Kind::TypeDefinition,
        "enumerator" => Kind::Enumerator,
        "call_expression" => Kind::CallExpression,
        "initializer_pair" => Kind::InitializerPair,
        "initializer_list" => Kind::InitializerList,
        "init_declarator" => Kind::InitDeclarator,
        "assignment_expression" => Kind::AssignmentExpression,
        "compound_literal_expression" => Kind::CompoundLiteralExpression,
        "type_descriptor" => Kind::TypeDescriptor,
        "identifier" => Kind::Identifier,
        "field_identifier" => Kind::FieldIdentifier,
        "type_identifier" => Kind::TypeIdentifier,
        "qualified_identifier" | "qualified_field_identifier" | "qualified_type_identifier" => {
            Kind::QualifiedIdentifier
        }
        "template_function" => Kind::TemplateFunction,
        "template_method" => Kind::TemplateMethod,
        "template_type" => Kind::TemplateType,
        "destructor_name" => Kind::DestructorName,
        "operator_name" => Kind::OperatorName,
        "function_declarator" => Kind::FunctionDeclarator,
        "pointer_declarator" => Kind::PointerDeclarator,
        "reference_declarator" => Kind::ReferenceDeclarator,
        "parenthesized_declarator" => Kind::ParenthesizedDeclarator,
        "attributed_declarator" => Kind::AttributedDeclarator,
        "array_declarator" => Kind::ArrayDeclarator,
        "field_expression" => Kind::FieldExpression,
        "pointer_expression" => Kind::PointerExpression,
        "parenthesized_expression" => Kind::ParenthesizedExpression,
        "field_designator" => Kind::FieldDesignator,
        "storage_class_specifier" => Kind::StorageClassSpecifier,
        "compound_statement" => Kind::CompoundStatement,
        "expression_statement" => Kind::ExpressionStatement,
        _ => Kind::Other,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeclaratorShape {
    Function,
    /// A variable, including a function pointer variable.
    Object,
}

#[derive(Clone, Copy)]
struct Frame {
    node_id: usize,
    caller: Option<u32>,
    in_function: bool,
    in_class: bool,
}

struct Walker<'a> {
    source: &'a [u8],
    language: SourceLanguage,
    table: &'a KindTable,
    frames: Vec<Frame>,
    output: ExtractedFile,
}

impl<'a> Walker<'a> {
    fn new(source: &'a [u8], language: SourceLanguage, table: &'a KindTable) -> Self {
        Self {
            source,
            language,
            table,
            frames: Vec::new(),
            output: ExtractedFile::default(),
        }
    }

    // Iterative so that deeply nested trees (long `else if` chains in generated code) cannot
    // overflow the stack of a worker thread.
    fn walk(&mut self, tree: &Tree) {
        let root = tree.root_node();
        self.output.has_syntax_errors = root.has_error();
        let mut cursor = root.walk();
        loop {
            let node = cursor.node();
            self.enter(node);
            if cursor.goto_first_child() {
                continue;
            }
            loop {
                self.leave(cursor.node());
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    return;
                }
            }
        }
    }

    fn frame(&self) -> Frame {
        self.frames.last().copied().unwrap_or(Frame {
            node_id: usize::MAX,
            caller: None,
            in_function: false,
            in_class: false,
        })
    }

    fn leave(&mut self, node: Node) {
        if self
            .frames
            .last()
            .is_some_and(|frame| frame.node_id == node.id())
        {
            self.frames.pop();
        }
    }

    fn enter(&mut self, node: Node) {
        if node.is_error() {
            self.error_node(node);
            return;
        }
        match self.table.kind(node) {
            Kind::FunctionDefinition => self.function_definition(node),
            Kind::Declaration => {
                if !self.frame().in_function {
                    self.declaration(node);
                }
            }
            Kind::FieldDeclaration => {
                let frame = self.frame();
                if self.language == SourceLanguage::Cpp && frame.in_class && !frame.in_function {
                    self.method_declaration(node);
                }
            }
            Kind::PreprocDef => self.macro_definition(node, DefinitionKind::Macro),
            Kind::PreprocFunctionDef => self.macro_definition(node, DefinitionKind::FunctionMacro),
            Kind::StructSpecifier => self.type_specifier(node, DefinitionKind::Struct),
            Kind::UnionSpecifier => self.type_specifier(node, DefinitionKind::Union),
            Kind::EnumSpecifier => self.type_specifier(node, DefinitionKind::Enum),
            Kind::ClassSpecifier => self.type_specifier(node, DefinitionKind::Class),
            Kind::TypeDefinition => self.type_definition(node),
            Kind::Enumerator => {
                if let Some(name) = self.field(node, self.table.field_name) {
                    self.push_definition(name, node, DefinitionKind::EnumConstant, false, false);
                }
            }
            Kind::CallExpression => self.call(node),
            Kind::InitializerPair => self.initializer_pair(node),
            Kind::AssignmentExpression => self.assignment(node),
            Kind::CompoundStatement => {
                if !self.frame().in_function {
                    self.macro_defined_function(node);
                }
            }
            _ => {}
        }
    }

    fn field<'t>(&self, node: Node<'t>, field_id: u16) -> Option<Node<'t>> {
        if field_id == 0 {
            None
        } else {
            node.child_by_field_id(field_id)
        }
    }

    fn field_children<'t>(&self, node: Node<'t>, field_id: u16) -> Vec<Node<'t>> {
        let Some(field_id) = std::num::NonZeroU16::new(field_id) else {
            return Vec::new();
        };
        let mut cursor = node.walk();
        node.children_by_field_id(field_id, &mut cursor).collect()
    }

    fn text(&self, node: Node) -> &'a [u8] {
        self.source.get(node.byte_range()).unwrap_or_default()
    }

    fn name_text(&self, node: Node) -> Option<String> {
        let text = std::str::from_utf8(self.text(node)).ok()?.trim();
        if text.is_empty() {
            None
        } else {
            Some(text.to_owned())
        }
    }

    fn has_storage_class(&self, node: Node, keyword: &[u8]) -> bool {
        let mut cursor = node.walk();
        node.children(&mut cursor).any(|child| {
            self.table.kind(child) == Kind::StorageClassSpecifier && self.text(child) == keyword
        })
    }

    fn push_definition(
        &mut self,
        name_node: Node,
        item_node: Node,
        kind: DefinitionKind,
        is_static: bool,
        is_declaration: bool,
    ) -> Option<u32> {
        let name = self.name_text(name_node)?;
        let index = u32::try_from(self.output.definitions.len()).ok()?;
        self.output.definitions.push(ExtractedDefinition {
            name,
            kind,
            is_static,
            is_declaration,
            name_range: node_range(name_node),
            item_range: node_range(item_node),
        });
        Some(index)
    }

    /// Follows a declarator down to the declared name and tells whether it declares a function
    /// or an object (a function pointer is an object).
    fn resolve_declarator<'t>(&self, declarator: Node<'t>) -> Option<(Node<'t>, DeclaratorShape)> {
        let mut node = declarator;
        let mut saw_function = false;
        let mut pointer_since_function = false;
        for _ in 0..64 {
            match self.table.kind(node) {
                Kind::Identifier
                | Kind::FieldIdentifier
                | Kind::TypeIdentifier
                | Kind::DestructorName
                | Kind::OperatorName => {
                    let shape = if saw_function && !pointer_since_function {
                        DeclaratorShape::Function
                    } else {
                        DeclaratorShape::Object
                    };
                    return Some((node, shape));
                }
                Kind::QualifiedIdentifier
                | Kind::TemplateFunction
                | Kind::TemplateType
                | Kind::TemplateMethod => {
                    node = self.field(node, self.table.field_name)?;
                }
                Kind::FunctionDeclarator => {
                    saw_function = true;
                    pointer_since_function = false;
                    node = self.field(node, self.table.field_declarator)?;
                }
                Kind::PointerDeclarator | Kind::ReferenceDeclarator => {
                    if saw_function {
                        pointer_since_function = true;
                    }
                    node = match self.field(node, self.table.field_declarator) {
                        Some(inner) => inner,
                        None => last_named_child(node)?,
                    };
                }
                Kind::ArrayDeclarator | Kind::InitDeclarator => {
                    if saw_function {
                        pointer_since_function = true;
                    }
                    node = self.field(node, self.table.field_declarator)?;
                }
                Kind::ParenthesizedDeclarator => node = last_named_child(node)?,
                Kind::AttributedDeclarator => node = node.named_child(0)?,
                _ => return None,
            }
        }
        None
    }

    fn is_qualified_declarator(&self, declarator: Node) -> bool {
        let mut node = declarator;
        for _ in 0..64 {
            match self.table.kind(node) {
                Kind::QualifiedIdentifier => return true,
                Kind::FunctionDeclarator
                | Kind::PointerDeclarator
                | Kind::ReferenceDeclarator
                | Kind::ArrayDeclarator => {
                    let Some(inner) = self
                        .field(node, self.table.field_declarator)
                        .or_else(|| last_named_child(node))
                    else {
                        return false;
                    };
                    node = inner;
                }
                Kind::ParenthesizedDeclarator => {
                    let Some(inner) = last_named_child(node) else {
                        return false;
                    };
                    node = inner;
                }
                _ => return false,
            }
        }
        false
    }

    fn function_definition(&mut self, node: Node) {
        let frame = self.frame();
        let mut caller = None;
        if let Some(declarator) = self.field(node, self.table.field_declarator)
            && let Some((name, _)) = self.resolve_declarator(declarator)
        {
            let kind = if self.language == SourceLanguage::Cpp
                && (frame.in_class || self.is_qualified_declarator(declarator))
            {
                DefinitionKind::Method
            } else {
                DefinitionKind::Function
            };
            let is_static = self.has_storage_class(node, b"static");
            caller = self.push_definition(name, node, kind, is_static, false);
        }
        self.frames.push(Frame {
            node_id: node.id(),
            caller: caller.or(frame.caller),
            in_function: true,
            in_class: false,
        });
    }

    fn declaration(&mut self, node: Node) {
        let is_static = self.has_storage_class(node, b"static");
        let is_extern = self.has_storage_class(node, b"extern");
        let declarators = self.field_children(node, self.table.field_declarator);
        for declarator in declarators {
            let Some((name, shape)) = self.resolve_declarator(declarator) else {
                continue;
            };
            match shape {
                DeclaratorShape::Function => {
                    let kind = if self.language == SourceLanguage::Cpp
                        && self.is_qualified_declarator(declarator)
                    {
                        DefinitionKind::Method
                    } else {
                        DefinitionKind::Function
                    };
                    self.push_definition(name, node, kind, is_static, true);
                }
                DeclaratorShape::Object => {
                    self.push_definition(
                        name,
                        node,
                        DefinitionKind::Variable,
                        is_static,
                        is_extern,
                    );
                }
            }
        }
    }

    fn method_declaration(&mut self, node: Node) {
        let declarators = self.field_children(node, self.table.field_declarator);
        let is_static = self.has_storage_class(node, b"static");
        for declarator in declarators {
            if let Some((name, DeclaratorShape::Function)) = self.resolve_declarator(declarator) {
                self.push_definition(name, node, DefinitionKind::Method, is_static, true);
            }
        }
    }

    fn type_specifier(&mut self, node: Node, kind: DefinitionKind) {
        if self.field(node, self.table.field_body).is_none() {
            return;
        }
        if let Some(name) = self.field(node, self.table.field_name) {
            let name = match self.table.kind(name) {
                Kind::QualifiedIdentifier | Kind::TemplateType => {
                    self.field(name, self.table.field_name).unwrap_or(name)
                }
                _ => name,
            };
            self.push_definition(name, node, kind, false, false);
        }
        if matches!(kind, DefinitionKind::Class | DefinitionKind::Struct)
            && self.language == SourceLanguage::Cpp
        {
            let frame = self.frame();
            self.frames.push(Frame {
                node_id: node.id(),
                caller: frame.caller,
                in_function: false,
                in_class: true,
            });
        }
    }

    fn type_definition(&mut self, node: Node) {
        let declarators = self.field_children(node, self.table.field_declarator);
        for declarator in declarators {
            if let Some((name, _)) = self.resolve_declarator(declarator) {
                self.push_definition(name, node, DefinitionKind::Typedef, false, false);
            }
        }
    }

    fn macro_definition(&mut self, node: Node, kind: DefinitionKind) {
        let Some(name) = self.field(node, self.table.field_name) else {
            return;
        };
        let Some(index) = self.push_definition(name, node, kind, false, false) else {
            return;
        };
        let Some(body) = self.field(node, self.table.field_value) else {
            return;
        };
        let mut parameters: Vec<&[u8]> = Vec::new();
        if let Some(parameter_list) = self.field(node, self.table.field_parameters) {
            let mut cursor = parameter_list.walk();
            for parameter in parameter_list.named_children(&mut cursor) {
                parameters.push(self.text(parameter));
            }
        }
        let start = node_position(body.start_byte(), body.start_position());
        scan_macro_body(
            self.text(body),
            start,
            &parameters,
            index,
            &mut self.output.calls,
        );
    }

    // tree-sitter-c gives up on a `#define` whose body contains a block comment and leaves an
    // ERROR node holding the pieces; recover the macro from the source text.
    fn error_node(&mut self, node: Node) {
        let mut cursor = node.walk();
        let defines: Vec<Node> = node
            .children(&mut cursor)
            .filter(|child| !child.is_named() && child.kind() == "#define")
            .collect();
        for define in defines {
            self.lexical_macro(define);
        }
    }

    fn lexical_macro(&mut self, define: Node) {
        let source = self.source;
        let mut cursor = SourceCursor {
            source,
            index: define.end_byte(),
            position: node_position(define.end_byte(), define.end_position()),
        };
        while matches!(cursor.peek(), Some(b' ' | b'\t')) {
            cursor.bump();
        }
        let name_start = cursor.position;
        let name_start_index = cursor.index;
        while cursor
            .peek()
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            cursor.bump();
        }
        let Some(name) = source
            .get(name_start_index..cursor.index)
            .and_then(|name| std::str::from_utf8(name).ok())
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
        else {
            return;
        };
        let name_end = cursor.position;
        let mut parameters: Vec<&[u8]> = Vec::new();
        let kind = if cursor.peek() == Some(b'(') {
            cursor.bump();
            let mut parameter_start = cursor.index;
            while let Some(byte) = cursor.peek() {
                if byte == b')' || byte == b',' {
                    if let Some(parameter) = source.get(parameter_start..cursor.index) {
                        parameters.push(parameter.trim_ascii());
                    }
                    cursor.bump();
                    if byte == b')' {
                        break;
                    }
                    parameter_start = cursor.index;
                } else if byte == b'\n' {
                    break;
                } else {
                    cursor.bump();
                }
            }
            DefinitionKind::FunctionMacro
        } else {
            DefinitionKind::Macro
        };
        let body_start = cursor.position;
        let body_start_index = cursor.index;
        while let Some(byte) = cursor.peek() {
            if byte == b'\\' && matches!(cursor.peek_at(1), Some(b'\n' | b'\r')) {
                cursor.bump();
                if cursor.peek() == Some(b'\r') {
                    cursor.bump();
                }
                cursor.bump();
                continue;
            }
            if byte == b'\n' || (byte == b'\r' && cursor.peek_at(1) == Some(b'\n')) {
                break;
            }
            cursor.bump();
        }
        let Ok(index) = u32::try_from(self.output.definitions.len()) else {
            return;
        };
        self.output.definitions.push(ExtractedDefinition {
            name,
            kind,
            is_static: false,
            is_declaration: false,
            name_range: Range {
                start: name_start,
                end: name_end,
            },
            item_range: Range {
                start: node_position(define.start_byte(), define.start_position()),
                end: cursor.position,
            },
        });
        let body = source
            .get(body_start_index..cursor.index)
            .unwrap_or_default();
        scan_macro_body(body, body_start, &parameters, index, &mut self.output.calls);
    }

    /// `SYSCALL_DEFINE3(open, ...) { body }` parses as a call statement followed by a block at
    /// file scope, which is never valid C on its own. Treat it as a definition of the function
    /// named by the macro's first argument, so that calls in the body have a caller.
    fn macro_defined_function(&mut self, body: Node) {
        let Some(statement) = body.prev_named_sibling() else {
            return;
        };
        if self.table.kind(statement) != Kind::ExpressionStatement {
            return;
        }
        let Some(call) = statement.named_child(0) else {
            return;
        };
        if self.table.kind(call) != Kind::CallExpression {
            return;
        }
        let Some(function) = self.field(call, self.table.field_function) else {
            return;
        };
        let is_macro_name = self.table.kind(function) == Kind::Identifier
            && self
                .text(function)
                .iter()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || *byte == b'_');
        if !is_macro_name {
            return;
        }
        let Some(name) = self
            .field(call, self.table.field_arguments)
            .and_then(|arguments| arguments.named_child(0))
            .filter(|argument| self.table.kind(*argument) == Kind::Identifier)
        else {
            return;
        };
        let Some(name_text) = self.name_text(name) else {
            return;
        };
        let Ok(index) = u32::try_from(self.output.definitions.len()) else {
            return;
        };
        self.output.definitions.push(ExtractedDefinition {
            name: name_text,
            kind: DefinitionKind::Function,
            is_static: false,
            is_declaration: false,
            name_range: node_range(name),
            item_range: Range {
                start: node_position(statement.start_byte(), statement.start_position()),
                end: node_position(body.end_byte(), body.end_position()),
            },
        });
        // The call to the defining macro itself is already recorded without a caller, which
        // is right: it is not a call made by the function.
        self.frames.push(Frame {
            node_id: body.id(),
            caller: Some(index),
            in_function: true,
            in_class: false,
        });
    }

    fn call(&mut self, node: Node) {
        let Some(function) = self.field(node, self.table.field_function) else {
            return;
        };
        let Some((name, kind)) = self.callee(function) else {
            return;
        };
        let Some(callee) = self.name_text(name) else {
            return;
        };
        self.output.calls.push(ExtractedCall {
            callee,
            caller: self.frame().caller,
            kind,
            in_macro_body: false,
            position: node_position(name.start_byte(), name.start_position()),
        });
    }

    fn callee<'t>(&self, function: Node<'t>) -> Option<(Node<'t>, CallKind)> {
        let mut node = function;
        let mut kind = CallKind::Direct;
        for _ in 0..16 {
            match self.table.kind(node) {
                Kind::Identifier | Kind::FieldIdentifier | Kind::DestructorName => {
                    return Some((node, kind));
                }
                Kind::OperatorName => return None,
                Kind::FieldExpression => {
                    kind = CallKind::Member;
                    node = self.field(node, self.table.field_field)?;
                }
                Kind::QualifiedIdentifier
                | Kind::TemplateFunction
                | Kind::TemplateMethod
                | Kind::TemplateType => {
                    node = self.field(node, self.table.field_name)?;
                }
                Kind::ParenthesizedExpression => {
                    if node.named_child_count() != 1 {
                        return None;
                    }
                    node = node.named_child(0)?;
                }
                _ => return None,
            }
        }
        None
    }

    fn assigned_function<'t>(&self, value: Node<'t>) -> Option<Node<'t>> {
        let mut node = value;
        for _ in 0..8 {
            match self.table.kind(node) {
                Kind::Identifier => return Some(node),
                Kind::QualifiedIdentifier => node = self.field(node, self.table.field_name)?,
                Kind::PointerExpression => {
                    let operator = self.field(node, self.table.field_operator)?;
                    if self.text(operator) != b"&" {
                        return None;
                    }
                    node = self.field(node, self.table.field_argument)?;
                }
                Kind::ParenthesizedExpression => {
                    if node.named_child_count() != 1 {
                        return None;
                    }
                    node = node.named_child(0)?;
                }
                _ => return None,
            }
        }
        None
    }

    fn initializer_pair(&mut self, node: Node) {
        let designator = self
            .field_children(node, self.table.field_designator)
            .last()
            .copied();
        let field_node = match designator {
            Some(designator) => match self.table.kind(designator) {
                Kind::FieldDesignator => designator.named_child(0),
                Kind::FieldIdentifier => Some(designator),
                _ => None,
            },
            None => None,
        };
        let Some(field_node) = field_node else {
            return;
        };
        let Some(value) = self
            .field(node, self.table.field_value)
            .and_then(|value| self.assigned_function(value))
        else {
            return;
        };
        let (Some(field), Some(value_name)) = (self.name_text(field_node), self.name_text(value))
        else {
            return;
        };
        let container = self.initializer_container(node);
        self.output.assignments.push(ExtractedAssignment {
            field,
            value: value_name,
            container,
            kind: AssignmentKind::DesignatedInitializer,
            position: node_position(value.start_byte(), value.start_position()),
        });
    }

    fn initializer_container(&self, pair: Node) -> Option<String> {
        let list = pair.parent()?;
        if self.table.kind(list) != Kind::InitializerList {
            return None;
        }
        let owner = list.parent()?;
        let type_node = match self.table.kind(owner) {
            Kind::InitDeclarator => {
                let declaration = owner.parent()?;
                self.field(declaration, self.table.field_type)?
            }
            Kind::CompoundLiteralExpression => {
                let descriptor = self.field(owner, self.table.field_type)?;
                if self.table.kind(descriptor) == Kind::TypeDescriptor {
                    self.field(descriptor, self.table.field_type)?
                } else {
                    descriptor
                }
            }
            _ => return None,
        };
        let name_node = match self.table.kind(type_node) {
            Kind::StructSpecifier | Kind::UnionSpecifier | Kind::ClassSpecifier => {
                self.field(type_node, self.table.field_name)?
            }
            Kind::TypeIdentifier => type_node,
            _ => return None,
        };
        self.name_text(name_node)
    }

    fn assignment(&mut self, node: Node) {
        let Some(operator) = self.field(node, self.table.field_operator) else {
            return;
        };
        if self.text(operator) != b"=" {
            return;
        }
        let Some(left) = self.field(node, self.table.field_left) else {
            return;
        };
        if self.table.kind(left) != Kind::FieldExpression {
            return;
        }
        let Some(field_node) = self.field(left, self.table.field_field) else {
            return;
        };
        let Some(value) = self
            .field(node, self.table.field_right)
            .and_then(|value| self.assigned_function(value))
        else {
            return;
        };
        let (Some(field), Some(value_name)) = (self.name_text(field_node), self.name_text(value))
        else {
            return;
        };
        self.output.assignments.push(ExtractedAssignment {
            field,
            value: value_name,
            container: None,
            kind: AssignmentKind::Assignment,
            position: node_position(value.start_byte(), value.start_position()),
        });
    }
}

struct SourceCursor<'a> {
    source: &'a [u8],
    index: usize,
    position: Position,
}

impl SourceCursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.source.get(self.index).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.source.get(self.index.checked_add(offset)?).copied()
    }

    fn bump(&mut self) {
        let Some(byte) = self.peek() else {
            return;
        };
        self.index += 1;
        self.position.byte = self.position.byte.saturating_add(1);
        if byte == b'\n' {
            self.position.row = self.position.row.saturating_add(1);
            self.position.column = 0;
        } else {
            self.position.column = self.position.column.saturating_add(1);
        }
    }
}

fn last_named_child(node: Node) -> Option<Node> {
    let count = u32::try_from(node.named_child_count()).ok()?;
    node.named_child(count.checked_sub(1)?)
}

fn to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn node_position(byte: usize, point: Point) -> Position {
    Position {
        byte: to_u32(byte),
        row: to_u32(point.row),
        column: to_u32(point.column),
    }
}

fn node_range(node: Node) -> Range {
    Range {
        start: node_position(node.start_byte(), node.start_position()),
        end: node_position(node.end_byte(), node.end_position()),
    }
}

const MACRO_BODY_KEYWORDS: &[&[u8]] = &[
    b"if",
    b"while",
    b"for",
    b"switch",
    b"return",
    b"sizeof",
    b"typeof",
    b"__typeof",
    b"__typeof__",
    b"alignof",
    b"_Alignof",
    b"__alignof__",
    b"_Generic",
    b"_Static_assert",
    b"static_assert",
    b"defined",
    b"do",
    b"else",
    b"case",
    b"asm",
    b"__asm",
    b"__asm__",
    b"volatile",
    b"__volatile__",
    b"__attribute__",
    b"__attribute",
    b"__declspec",
    b"__builtin_offsetof",
    b"offsetof",
];

/// The body of a `#define` is opaque to the grammar, so calls in it are found lexically:
/// an identifier followed by `(`, skipping comments, literals, keywords, the macro's own
/// parameters and token pasting.
fn scan_macro_body(
    text: &[u8],
    start: Position,
    parameters: &[&[u8]],
    caller: u32,
    calls: &mut Vec<ExtractedCall>,
) {
    let mut row = start.row;
    let mut column = start.column;
    let mut index = 0;
    // What precedes the current token: `->`/`.` makes a member call, `##`/`#` a pasted or
    // stringified token that is not a real name.
    let mut previous_significant: &[u8] = b"";
    let advance = |index: &mut usize, row: &mut u32, column: &mut u32, byte: u8| {
        *index += 1;
        if byte == b'\n' {
            *row += 1;
            *column = 0;
        } else {
            *column += 1;
        }
    };
    while let Some(&byte) = text.get(index) {
        if byte == b'/' && text.get(index + 1) == Some(&b'*') {
            advance(&mut index, &mut row, &mut column, byte);
            advance(&mut index, &mut row, &mut column, b'*');
            while let Some(&inner) = text.get(index) {
                if inner == b'*' && text.get(index + 1) == Some(&b'/') {
                    advance(&mut index, &mut row, &mut column, inner);
                    advance(&mut index, &mut row, &mut column, b'/');
                    break;
                }
                advance(&mut index, &mut row, &mut column, inner);
            }
            continue;
        }
        if byte == b'/' && text.get(index + 1) == Some(&b'/') {
            while let Some(&inner) = text.get(index) {
                if inner == b'\n' {
                    break;
                }
                advance(&mut index, &mut row, &mut column, inner);
            }
            continue;
        }
        if byte == b'"' || byte == b'\'' {
            advance(&mut index, &mut row, &mut column, byte);
            while let Some(&inner) = text.get(index) {
                advance(&mut index, &mut row, &mut column, inner);
                if inner == b'\\' {
                    if let Some(&escaped) = text.get(index) {
                        advance(&mut index, &mut row, &mut column, escaped);
                    }
                } else if inner == byte || inner == b'\n' {
                    break;
                }
            }
            previous_significant = b"literal";
            continue;
        }
        if byte.is_ascii_alphabetic() || byte == b'_' {
            let token_start = index;
            let token_position = Position {
                byte: start.byte.saturating_add(to_u32(index)),
                row,
                column,
            };
            while let Some(&inner) = text.get(index) {
                if inner.is_ascii_alphanumeric() || inner == b'_' {
                    advance(&mut index, &mut row, &mut column, inner);
                } else {
                    break;
                }
            }
            let token = text.get(token_start..index).unwrap_or_default();
            let mut lookahead = index;
            while let Some(&inner) = text.get(lookahead) {
                if inner == b' ' || inner == b'\t' || inner == b'\r' || inner == b'\n' {
                    lookahead += 1;
                } else if inner == b'\\'
                    && matches!(text.get(lookahead + 1), Some(b'\n') | Some(b'\r'))
                {
                    lookahead += 1;
                } else {
                    break;
                }
            }
            let followed_by_paren = text.get(lookahead) == Some(&b'(');
            let followed_by_paste = text.get(lookahead) == Some(&b'#');
            let pasted_or_stringified =
                previous_significant == b"#" || previous_significant == b"##";
            if followed_by_paren
                && !followed_by_paste
                && !pasted_or_stringified
                && !MACRO_BODY_KEYWORDS.contains(&token)
                && !parameters.contains(&token)
                && let Ok(name) = std::str::from_utf8(token)
            {
                let kind = if previous_significant == b"->" || previous_significant == b"." {
                    CallKind::Member
                } else {
                    CallKind::Direct
                };
                calls.push(ExtractedCall {
                    callee: name.to_owned(),
                    caller: Some(caller),
                    kind,
                    in_macro_body: true,
                    position: token_position,
                });
            }
            previous_significant = b"identifier";
            continue;
        }
        if byte.is_ascii_digit() {
            while let Some(&inner) = text.get(index) {
                if inner.is_ascii_alphanumeric() || inner == b'_' || inner == b'.' {
                    advance(&mut index, &mut row, &mut column, inner);
                } else {
                    break;
                }
            }
            previous_significant = b"number";
            continue;
        }
        if !byte.is_ascii_whitespace() && byte != b'\\' {
            previous_significant = if byte == b'-' && text.get(index + 1) == Some(&b'>') {
                advance(&mut index, &mut row, &mut column, byte);
                b"->"
            } else if byte == b'#' && text.get(index + 1) == Some(&b'#') {
                advance(&mut index, &mut row, &mut column, byte);
                b"##"
            } else if byte == b'#' {
                b"#"
            } else if byte == b'.' {
                b"."
            } else {
                b"punctuation"
            };
        }
        advance(&mut index, &mut row, &mut column, byte);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract_c(source: &str) -> ExtractedFile {
        extract(source.as_bytes(), SourceLanguage::C, &|| false)
    }

    fn definitions_named<'a>(file: &'a ExtractedFile, name: &str) -> Vec<&'a ExtractedDefinition> {
        file.definitions
            .iter()
            .filter(|definition| definition.name == name)
            .collect()
    }

    fn only_definition<'a>(file: &'a ExtractedFile, name: &str) -> &'a ExtractedDefinition {
        let found = definitions_named(file, name);
        assert_eq!(
            found.len(),
            1,
            "expected one definition of {name}: {found:#?}"
        );
        found[0]
    }

    fn calls_to<'a>(file: &'a ExtractedFile, name: &str) -> Vec<&'a ExtractedCall> {
        file.calls
            .iter()
            .filter(|call| call.callee == name)
            .collect()
    }

    fn caller_name(file: &ExtractedFile, call: &ExtractedCall) -> Option<String> {
        call.caller
            .and_then(|index| file.definitions.get(index as usize))
            .map(|definition| definition.name.clone())
    }

    /// The position of the `occurrence`-th match of `needle`, computed independently of
    /// tree-sitter.
    fn position_of(source: &str, needle: &str, occurrence: usize) -> Position {
        let byte = source
            .match_indices(needle)
            .nth(occurrence)
            .map(|(index, _)| index)
            .expect("needle present");
        let before = &source[..byte];
        let row = before.matches('\n').count();
        let line_start = before.rfind('\n').map_or(0, |index| index + 1);
        Position {
            byte: byte as u32,
            row: row as u32,
            column: (byte - line_start) as u32,
        }
    }

    #[test]
    fn static_functions_prototypes_and_definitions() {
        let file = extract_c(
            "int shared(int value);\n\
             static int helper(void) { return 1; }\n\
             int shared(int value) { return helper() + value; }\n\
             extern int counter;\n\
             int total = 0;\n\
             static int cache[4];\n\
             int (*hook)(int);\n\
             void (*install(int signal, void (*handler)(int)))(int);\n",
        );
        let shared = definitions_named(&file, "shared");
        assert_eq!(shared.len(), 2);
        assert!(
            shared
                .iter()
                .all(|definition| definition.kind == DefinitionKind::Function)
        );
        assert_eq!(
            shared
                .iter()
                .map(|definition| definition.is_declaration)
                .collect::<Vec<_>>(),
            vec![true, false]
        );
        let helper = only_definition(&file, "helper");
        assert!(helper.is_static && !helper.is_declaration);
        assert!(!shared.iter().any(|definition| definition.is_static));

        let counter = only_definition(&file, "counter");
        assert_eq!(counter.kind, DefinitionKind::Variable);
        assert!(counter.is_declaration);
        let total = only_definition(&file, "total");
        assert_eq!(total.kind, DefinitionKind::Variable);
        assert!(!total.is_declaration);
        assert!(only_definition(&file, "cache").is_static);
        assert_eq!(
            only_definition(&file, "hook").kind,
            DefinitionKind::Variable
        );
        let install = only_definition(&file, "install");
        assert_eq!(install.kind, DefinitionKind::Function);
        assert!(install.is_declaration);
        assert!(definitions_named(&file, "handler").is_empty());

        let helper_calls = calls_to(&file, "helper");
        assert_eq!(helper_calls.len(), 1);
        assert_eq!(
            caller_name(&file, helper_calls[0]).as_deref(),
            Some("shared")
        );
        assert_eq!(helper_calls[0].kind, CallKind::Direct);
    }

    #[test]
    fn types_enums_and_macros() {
        let file = extract_c(
            "#define LIMIT 16\n\
             #define MAX(a, b) ((a) > (b) ? (a) : (b))\n\
             #define OPEN_TWICE(x) do { do_open(x); helper(/* c */ 1); } while (0)\n\
             #define APPLY(f, v) f(v)\n\
             #define PASTE(name) name##_init(); prefix_##name()\n\
             struct point { int x; int y; };\n\
             union value { int i; float f; };\n\
             enum color { RED, GREEN = 2 };\n\
             typedef struct point point_t;\n\
             typedef int (*callback_t)(int);\n\
             int use(void) { return MAX(1, 2) + LIMIT; }\n",
        );
        assert_eq!(only_definition(&file, "LIMIT").kind, DefinitionKind::Macro);
        assert_eq!(
            only_definition(&file, "MAX").kind,
            DefinitionKind::FunctionMacro
        );
        assert_eq!(only_definition(&file, "point").kind, DefinitionKind::Struct);
        assert_eq!(only_definition(&file, "value").kind, DefinitionKind::Union);
        assert_eq!(only_definition(&file, "color").kind, DefinitionKind::Enum);
        assert_eq!(
            only_definition(&file, "RED").kind,
            DefinitionKind::EnumConstant
        );
        assert_eq!(
            only_definition(&file, "GREEN").kind,
            DefinitionKind::EnumConstant
        );
        assert_eq!(
            only_definition(&file, "point_t").kind,
            DefinitionKind::Typedef
        );
        assert_eq!(
            only_definition(&file, "callback_t").kind,
            DefinitionKind::Typedef
        );

        // A call to a function-like macro is recorded like any call.
        let max_calls = calls_to(&file, "MAX");
        assert_eq!(max_calls.len(), 1);
        assert_eq!(caller_name(&file, max_calls[0]).as_deref(), Some("use"));

        // Calls inside a macro body belong to the macro.
        for callee in ["do_open", "helper"] {
            let calls = calls_to(&file, callee);
            assert_eq!(calls.len(), 1, "{callee}: {:#?}", file.calls);
            assert!(calls[0].in_macro_body);
            assert_eq!(caller_name(&file, calls[0]).as_deref(), Some("OPEN_TWICE"));
        }
        // Parameters, keywords and pasted tokens are not callees.
        for not_a_callee in ["f", "while", "name", "_init", "prefix_"] {
            assert!(calls_to(&file, not_a_callee).is_empty(), "{not_a_callee}");
        }
    }

    #[test]
    fn member_calls_and_function_pointer_wiring() {
        let file = extract_c(
            "static int foo_open(struct inode *inode) { return 0; }\n\
             static int foo_release(struct inode *inode) { return 0; }\n\
             static const struct file_operations foo_fops = {\n\
             \t.owner = THIS_MODULE,\n\
             \t.open = foo_open,\n\
             \t.release = &foo_release,\n\
             };\n\
             void wire(struct file_operations *ops, struct device *device) {\n\
             \tops->read = foo_read;\n\
             \tops->count = 3;\n\
             \tops->open(device->inode);\n\
             \tdevice->ops.release(0);\n\
             }\n",
        );
        let open_calls = calls_to(&file, "open");
        assert_eq!(open_calls.len(), 1);
        assert_eq!(open_calls[0].kind, CallKind::Member);
        assert_eq!(caller_name(&file, open_calls[0]).as_deref(), Some("wire"));
        assert_eq!(calls_to(&file, "release")[0].kind, CallKind::Member);

        let assignment = |field: &str| {
            file.assignments
                .iter()
                .find(|assignment| assignment.field == field)
                .unwrap_or_else(|| panic!("no assignment to {field}: {:#?}", file.assignments))
        };
        let open = assignment("open");
        assert_eq!(open.value, "foo_open");
        assert_eq!(open.kind, AssignmentKind::DesignatedInitializer);
        assert_eq!(open.container.as_deref(), Some("file_operations"));
        let release = assignment("release");
        assert_eq!(release.value, "foo_release");
        let read = assignment("read");
        assert_eq!(read.value, "foo_read");
        assert_eq!(read.kind, AssignmentKind::Assignment);
        assert_eq!(read.container, None);
        // `ops->count = 3` stores no name, so it is not a wiring candidate.
        assert!(
            file.assignments
                .iter()
                .all(|assignment| assignment.field != "count")
        );
    }

    #[test]
    fn both_branches_of_conditional_compilation() {
        let file = extract_c(
            "#ifdef CONFIG_FAST\n\
             int feature(void) { return fast_path(); }\n\
             #else\n\
             int feature(void) { return slow_path(); }\n\
             #endif\n\
             #if defined(A) && B > 1\n\
             static void only_a(void) {}\n\
             #elif C\n\
             static void only_c(void) {}\n\
             #endif\n",
        );
        assert_eq!(definitions_named(&file, "feature").len(), 2);
        for callee in ["fast_path", "slow_path"] {
            let calls = calls_to(&file, callee);
            assert_eq!(calls.len(), 1, "{callee}");
            assert_eq!(caller_name(&file, calls[0]).as_deref(), Some("feature"));
        }
        only_definition(&file, "only_a");
        only_definition(&file, "only_c");
    }

    #[test]
    fn broken_source_still_yields_what_parses() {
        let file = extract_c(
            "int before(void) { return first_call(); }\n\
             int broken( { ;;; ]] \n\
             FOO_MACRO(bar) {{ \n\
             int after(void) { return second_call(); }\n\
             #define STILL_HERE 1\n",
        );
        assert!(file.has_syntax_errors);
        assert!(!file.parse_failed);
        only_definition(&file, "before");
        only_definition(&file, "STILL_HERE");
        assert_eq!(calls_to(&file, "first_call").len(), 1);
        assert_eq!(calls_to(&file, "second_call").len(), 1);
    }

    #[test]
    fn positions_with_crlf_and_multibyte_comments() {
        let source = "/* 説明: 日本語のコメント */\r\n\
                      static int alpha(void)\r\n\
                      {\r\n\
                      \t/* 注意 */ return beta(1) + gamma(2); // 末尾\r\n\
                      }\r\n\
                      #define DELTA(x) \\\r\n\
                      \t/* ユーザー */ epsilon(x)\r\n";
        let file = extract_c(source);
        let alpha = only_definition(&file, "alpha");
        assert_eq!(alpha.name_range.start, position_of(source, "alpha", 0));
        assert_eq!(alpha.name_range.end.byte, alpha.name_range.start.byte + 5);
        assert_eq!(alpha.item_range.start, position_of(source, "static", 0));
        assert_eq!(alpha.item_range.end.row, 4);
        for callee in ["beta", "gamma", "epsilon"] {
            let calls = calls_to(&file, callee);
            assert_eq!(calls.len(), 1, "{callee}: {:#?}", file.calls);
            assert_eq!(
                calls[0].position,
                position_of(source, callee, 0),
                "{callee}"
            );
        }
        assert_eq!(
            only_definition(&file, "DELTA").name_range.start,
            position_of(source, "DELTA", 0)
        );
    }

    #[test]
    fn cpp_classes_methods_and_calls() {
        let source = "namespace app {\n\
                      class Widget {\n\
                      public:\n\
                      \tvoid draw();\n\
                      \tint size() const { return compute(); }\n\
                      };\n\
                      void Widget::draw() { helper(); this->size(); renderer.paint(1); ns::free_fn(); }\n\
                      }\n";
        let file = extract(source.as_bytes(), SourceLanguage::Cpp, &|| false);
        assert_eq!(only_definition(&file, "Widget").kind, DefinitionKind::Class);
        let draw = definitions_named(&file, "draw");
        assert_eq!(draw.len(), 2, "{:#?}", file.definitions);
        assert!(
            draw.iter()
                .all(|definition| definition.kind == DefinitionKind::Method)
        );
        assert_eq!(
            draw.iter()
                .map(|definition| definition.is_declaration)
                .collect::<Vec<_>>(),
            vec![true, false]
        );
        assert_eq!(only_definition(&file, "size").kind, DefinitionKind::Method);
        assert_eq!(
            caller_name(&file, calls_to(&file, "compute")[0]).as_deref(),
            Some("size")
        );
        for (callee, kind) in [
            ("helper", CallKind::Direct),
            ("size", CallKind::Member),
            ("paint", CallKind::Member),
            ("free_fn", CallKind::Direct),
        ] {
            let calls = calls_to(&file, callee);
            assert_eq!(calls.len(), 1, "{callee}: {:#?}", file.calls);
            assert_eq!(calls[0].kind, kind, "{callee}");
            assert_eq!(caller_name(&file, calls[0]).as_deref(), Some("draw"));
        }
    }

    #[test]
    fn functions_behind_annotation_macros() {
        // Export and inlining macros in front of the return type leave ERROR nodes, but the
        // definitions and their calls must still be found.
        let file = extract_c(
            "ZSTDLIB_API size_t ZSTD_compress(void *dst, size_t capacity);\n\
             MEM_STATIC U16 MEM_read16(const void *pointer) { return helper16(pointer); }\n\
             static INLINE int ss_ilg(int n) { return lg(n); }\n\
             static int __init foo_init(void) { return register_foo(); }\n\
             asmlinkage long sys_foo(int x) { return do_foo(x); }\n\
             int __attribute__((weak)) weak_fn(void) { return weak_callee(); }\n\
             SYSCALL_DEFINE3(open, const char __user *, filename, int, flags, umode_t, mode)\n\
             {\n\
             \treturn do_sys_open(AT_FDCWD, filename, flags, mode);\n\
             }\n\
             void after(void) { tail_call(); }\n",
        );
        let compress = only_definition(&file, "ZSTD_compress");
        assert!(compress.is_declaration);
        for (function, callee) in [
            ("MEM_read16", "helper16"),
            ("ss_ilg", "lg"),
            ("foo_init", "register_foo"),
            ("sys_foo", "do_foo"),
            ("weak_fn", "weak_callee"),
            ("open", "do_sys_open"),
            ("after", "tail_call"),
        ] {
            let definition = only_definition(&file, function);
            assert_eq!(definition.kind, DefinitionKind::Function, "{function}");
            assert!(!definition.is_declaration, "{function}");
            let calls = calls_to(&file, callee);
            assert_eq!(calls.len(), 1, "{callee}");
            assert_eq!(
                caller_name(&file, calls[0]).as_deref(),
                Some(function),
                "{callee}"
            );
        }
        assert!(only_definition(&file, "foo_init").is_static);
        // The defining macro is not a function of its own.
        assert!(definitions_named(&file, "SYSCALL_DEFINE3").is_empty());
        let open = only_definition(&file, "open");
        assert_eq!(open.item_range.start.row, 6);
        assert_eq!(open.item_range.end.row, 9);
    }

    #[test]
    fn header_language_detection() {
        let guess = language_for_path("include/linux/fs.h");
        assert_eq!(guess, Some(LanguageGuess::Header));
        let guess = guess.expect("header");
        assert_eq!(
            guess.resolve(b"struct class { int new; };"),
            SourceLanguage::C
        );
        assert_eq!(guess.resolve(b"namespace x {}"), SourceLanguage::Cpp);
        assert_eq!(language_for_path("a/b.rs"), None);
        assert_eq!(language_for_path("dir.c/readme"), None);
    }

    #[test]
    fn deeply_nested_code_does_not_overflow_the_stack() {
        let mut source = String::from("int deep(int x) {\n");
        for _ in 0..20_000 {
            source.push_str("if (x) { ");
        }
        source.push_str("leaf();");
        for _ in 0..20_000 {
            source.push('}');
        }
        source.push_str("\n}\n");
        let file = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || extract_c(&source))
            .expect("spawn")
            .join()
            .expect("no stack overflow");
        assert_eq!(calls_to(&file, "leaf").len(), 1);
        assert_eq!(
            caller_name(&file, calls_to(&file, "leaf")[0]).as_deref(),
            Some("deep")
        );
    }

    #[test]
    fn abort_stops_the_parse() {
        let source = "int f(void) { return g(); }\n".repeat(20_000);
        let file = extract(source.as_bytes(), SourceLanguage::C, &|| true);
        assert!(file.parse_failed);
        // The parser must be usable again after an aborted parse.
        let file = extract_c("int f(void) { return g(); }");
        assert!(!file.parse_failed);
        assert_eq!(calls_to(&file, "g").len(), 1);
    }

    #[test]
    fn member_calls_in_macro_bodies() {
        let file =
            extract_c("#define CALL_OPEN(ops, x) ((ops)->open(x) + (ops).close(x) + plain(x))\n");
        for (callee, kind) in [
            ("open", CallKind::Member),
            ("close", CallKind::Member),
            ("plain", CallKind::Direct),
        ] {
            let calls = calls_to(&file, callee);
            assert_eq!(calls.len(), 1, "{callee}: {:#?}", file.calls);
            assert_eq!(calls[0].kind, kind, "{callee}");
            assert!(calls[0].in_macro_body);
            assert_eq!(caller_name(&file, calls[0]).as_deref(), Some("CALL_OPEN"));
        }
    }

    #[test]
    fn multi_line_macros_with_crlf_and_comments() {
        // Without care, tree-sitter-c ends each of these macros at its first line and parses
        // the continuation lines as code, which turns `second_call(x)` into a definition that
        // swallows `after`.
        let line_ends = [("LF", "\n", "\\\n"), ("CRLF", "\r\n", "\\\r\n")];
        for (name, newline, continuation) in line_ends {
            let comments = [
                String::new(),
                " /* note */".to_owned(),
                " /* C:\\path\\ \"quoted */".to_owned(),
                // A comment does not end a directive even when it spans lines.
                format!(" /* two{newline} lines */"),
            ];
            for comment in comments {
                let source = format!(
                    "#define WRAP(x){comment} {continuation}\
                     \tfirst_call(x);{comment} {continuation}\
                     \tsecond_call(\"s\", x){newline}\
                     int after(void) {{ return tail(); }}{newline}"
                );
                let case = format!("{name} {comment:?}");
                let file = extract_c(&source);
                assert!(!file.has_syntax_errors, "{case}");
                let wrap = only_definition(&file, "WRAP");
                assert_eq!(wrap.kind, DefinitionKind::FunctionMacro, "{case}");
                assert_eq!(wrap.name_range.start, position_of(&source, "WRAP", 0));
                assert_eq!(wrap.item_range.start, position_of(&source, "#define", 0));
                assert_eq!(wrap.item_range.end, position_of(&source, "int after", 0));
                for callee in ["first_call", "second_call"] {
                    let calls = calls_to(&file, callee);
                    assert_eq!(calls.len(), 1, "{case} {callee}: {:#?}", file.calls);
                    assert_eq!(calls[0].position, position_of(&source, callee, 0), "{case}");
                    assert!(calls[0].in_macro_body, "{case}");
                    assert_eq!(caller_name(&file, calls[0]).as_deref(), Some("WRAP"));
                }
                assert!(definitions_named(&file, "second_call").is_empty(), "{case}");
                let after = only_definition(&file, "after");
                assert_eq!(after.name_range.start, position_of(&source, "after", 0));
                assert_eq!(
                    caller_name(&file, calls_to(&file, "tail")[0]).as_deref(),
                    Some("after"),
                    "{case}"
                );
            }
            // Outside directives, multi-line comments must still parse cleanly.
            let source = format!(
                "/**{newline} * Documentation.{newline} */{newline}\
                 int documented(void) {{ return callee(); }} /* a{newline} b */{newline}"
            );
            let file = extract_c(&source);
            assert!(!file.has_syntax_errors, "{name}");
            assert_eq!(
                only_definition(&file, "documented").name_range.start,
                position_of(&source, "documented", 0)
            );
            assert_eq!(calls_to(&file, "callee").len(), 1);
        }
    }

    #[test]
    fn normalization_keeps_every_offset() {
        let source: &[u8] = b"a /* x\\\r\ny */ \"/*\" '\\'' // c /* d\r\n\
            #define M \\\r\n b /* \"c\r\n d */ \\\r\n e /* last */\r\n\
            #include \"x.h\" /* kept */\r\n\
            # define N(a) /* e */ f(a) \"/*\" g(a)\r\n\
            #if A \\\r\n && B\r\n/* open";
        let normalized = normalize_for_parsing(source);
        assert_eq!(normalized.len(), source.len());
        let newlines = |bytes: &[u8]| {
            bytes
                .iter()
                .enumerate()
                .filter(|&(_, &byte)| byte == b'\n')
                .map(|(index, _)| index)
                .collect::<Vec<_>>()
        };
        assert_eq!(newlines(&normalized), newlines(source));
        // Only `#define` directives change: continuations before CRLF swap their bytes, and
        // comments followed by more of the macro become spaces with continuations.
        assert_eq!(
            normalized.as_ref(),
            b"a /* x\\\r\ny */ \"/*\" '\\'' // c /* d\r\n\
            #define M \r\\\n b     \r\\\n      \r\\\n e /* last */\r\n\
            #include \"x.h\" /* kept */\r\n\
            # define N(a)         f(a) \"/*\" g(a)\r\n\
            #if A \\\r\n && B\r\n/* open"
                .as_slice()
        );
        assert_eq!(
            normalize_for_parsing(b"#define M /* a\n b */ x\nint /* c\n d */ y;\n").as_ref(),
            b"#define M    \\\n      x\nint /* c\n d */ y;\n".as_slice()
        );
        // A comment that ends the macro stays: the grammar handles it, while spaces before
        // the line break would not be accepted after every directive.
        assert!(matches!(
            normalize_for_parsing(b"#define EMPTY /* nothing */\r\n#define ONE 1 /* one */ \n"),
            Cow::Borrowed(_)
        ));
        assert!(matches!(
            normalize_for_parsing(b"int f(void) { return 0; }\n"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn source_cursor_positions_match_the_text() {
        // Used to recover macros that the grammar gave up on.
        let source = "ab\r\n\tc\u{3042}\nd";
        let mut cursor = SourceCursor {
            source: source.as_bytes(),
            index: 0,
            position: Position::default(),
        };
        while cursor.peek().is_some() {
            cursor.bump();
            let before = &source.as_bytes()[..cursor.index];
            let line_start = before
                .iter()
                .rposition(|&byte| byte == b'\n')
                .map_or(0, |index| index + 1);
            let expected = Position {
                byte: cursor.index as u32,
                row: before.iter().filter(|&&byte| byte == b'\n').count() as u32,
                column: (cursor.index - line_start) as u32,
            };
            assert_eq!(cursor.position, expected, "after byte {}", cursor.index);
        }
        assert_eq!(cursor.position.row, 2);
        assert_eq!(cursor.position.column, 1);
    }

    #[test]
    fn hostile_sources_do_not_panic() {
        let mut random: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move || {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            random
        };
        let mut sources: Vec<Vec<u8>> = Vec::new();
        for length in [1usize, 7, 100, 4096, 65_536] {
            sources.push((0..length).map(|_| next() as u8).collect());
        }
        // Random bytes drawn from C's punctuation and keywords are more likely to reach
        // unusual parser states than uniform noise.
        let pieces: [&[u8]; 24] = [
            b"#define ",
            b"(",
            b")",
            b"{",
            b"}",
            b"\\\n",
            b"\\\r\n",
            b"/*",
            b"*/",
            b"\"",
            b"'",
            b"->",
            b".",
            b"=",
            b",",
            b";",
            b"##",
            b"#",
            b"struct ",
            b"static ",
            b"f",
            b"\0",
            b"\r",
            b"\n",
        ];
        for _ in 0..200 {
            let mut source = Vec::new();
            for _ in 0..(next() % 200) {
                source.extend_from_slice(pieces[(next() % pieces.len() as u64) as usize]);
            }
            sources.push(source);
        }
        sources.push(b"int f(void) { return g(); }\0\0\0 int h(void) { return i(); }".to_vec());
        let mut one_line = b"int wide(void) { return ".to_vec();
        for index in 0..200_000 {
            one_line.extend_from_slice(format!("c{index}() + ").as_bytes());
        }
        one_line.extend_from_slice(b"0; }");
        sources.push(one_line);
        sources.push([b"#define M(".as_slice(), &vec![b'a'; 1 << 20]].concat());
        sources.push([b"#define M(x) \"".as_slice(), &vec![b'\\'; 1 << 20]].concat());
        for source in &sources {
            for language in [SourceLanguage::C, SourceLanguage::Cpp] {
                let file = extract(source, language, &|| false);
                for definition in &file.definitions {
                    assert!(definition.name_range.start.byte as usize <= source.len());
                    assert!(definition.item_range.end.byte as usize <= source.len());
                }
                for call in &file.calls {
                    assert!(call.position.byte as usize <= source.len());
                    if let Some(caller) = call.caller {
                        assert!((caller as usize) < file.definitions.len());
                    }
                }
            }
        }
        let wide = extract(&sources[sources.len() - 3], SourceLanguage::C, &|| false);
        assert_eq!(wide.calls.len(), 200_000);
    }

    #[test]
    fn member_macros_without_semicolons_keep_the_struct_in_bounds() {
        // From tree-sitter's own `subtree.h`: `SUBTREE_BITS` and `SUBTREE_SIZE` expand to
        // members with their semicolons, and the struct has one body per `#if` branch.
        for newline in ["\n", "\r\n"] {
            let source = [
                "#define SUBTREE_BITS \\",
                "  bool visible : 1;",
                "#if TS_BIG_ENDIAN",
                "struct SubtreeInlineData {",
                "  uint16_t parse_state;",
                "  SUBTREE_BITS",
                "  bool is_inline : 1;",
                "  SUBTREE_SIZE",
                "};",
                "#else",
                "struct SubtreeInlineData {",
                "  bool is_inline : 1;",
                "  SUBTREE_BITS  // the flags",
                "  SUBTREE_SIZE",
                "};",
                "#endif",
                "typedef struct {",
                "  uint32_t ref_count;",
                "  union {",
                "    struct { uint32_t count; };",
                "    int32_t lookahead_char;",
                "  };",
                "} SubtreeHeapData;",
                "typedef union {",
                "  struct SubtreeInlineData data;",
                "  const SubtreeHeapData *ptr;",
                "} Subtree;",
                "enum mode {",
                "  MODE_A,",
                "  MODE_LAST",
                "};",
                "BOOL",
                "WINAPI",
                "CreateThing(int flags)",
                "{",
                "  THING_LOCK",
                "  return flags;",
                "}",
                "",
            ]
            .join(newline);
            let file = extract_c(&source);
            let inline = definitions_named(&file, "SubtreeInlineData");
            assert_eq!(inline.len(), 2, "{newline:?}: {:#?}", file.definitions);
            for (definition, rows) in inline.iter().zip([(3, 8), (10, 14)]) {
                assert_eq!(definition.kind, DefinitionKind::Struct);
                assert_eq!(
                    (
                        definition.item_range.start.row,
                        definition.item_range.end.row
                    ),
                    rows
                );
            }
            let heap = only_definition(&file, "SubtreeHeapData");
            assert_eq!(heap.kind, DefinitionKind::Typedef);
            assert_eq!(
                heap.item_range.start,
                position_of(&source, "typedef struct", 0)
            );
            let subtree = only_definition(&file, "Subtree");
            assert_eq!(subtree.kind, DefinitionKind::Typedef);
            assert_eq!(
                subtree.item_range.start,
                position_of(&source, "typedef union", 0)
            );
            // A lone name is only dropped inside struct bodies: enumerators, return types
            // and statements are left alone.
            assert_eq!(
                only_definition(&file, "MODE_LAST").kind,
                DefinitionKind::EnumConstant
            );
            let create = only_definition(&file, "CreateThing");
            assert_eq!(create.kind, DefinitionKind::Function);
            assert_eq!(create.item_range.start, position_of(&source, "BOOL", 0));
        }
        // Each branch opens the body, but only one of them counts: the lines after the
        // struct are at file scope again.
        let split =
            b"#if A\nstruct s {\n#else\nstruct s {\n#endif\n  int a;\n};\nBOOL\nWINAPI\nf(void);\n";
        assert!(matches!(normalize_for_parsing(split), Cow::Borrowed(_)));
        // A name alone on a line in the middle of a declaration is part of it.
        let continued = b"struct s {\n  unsigned\n  WIDTH\n  x;\n};\n";
        assert!(matches!(normalize_for_parsing(continued), Cow::Borrowed(_)));
        let source = b"struct s {\n  MEMBERS\n};\nenum e {\n  LAST\n};\n";
        let normalized = normalize_for_parsing(source);
        assert_eq!(
            normalized.as_ref(),
            b"struct s {\n         \n};\nenum e {\n  LAST\n};\n".as_slice()
        );
    }
}
