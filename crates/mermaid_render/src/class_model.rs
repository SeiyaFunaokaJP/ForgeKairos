//! A round-trippable editing model for Mermaid class diagrams.
//!
//! The shape of this module mirrors [`crate::state_model`]: [`parse_class`]
//! reads the body of a ```` ```mermaid ```` fence into [`ClassDocument`], and
//! [`emit_class`] writes it back. See [`crate::mermaid_model`] for the
//! invariants every diagram kind keeps.
//!
//! Like the state model, and for the same reason, this one does not read
//! `merman`'s class model. That model is a set of classes keyed by name with a
//! flat list of relations beside it, and `ClassRelation::id` is only the index
//! of the relation in that list — so the order a `class` declaration and a
//! relation line were written in, which is the order the editor has to show and
//! write back, is gone. The source is scanned here, and `merman` is asked only
//! whether the fence is a class diagram it can draw at all.
//!
//! A statement, not a class, is the unit of the model: `class A` followed by
//! `A : +foo()` is two [`ClassItem`] entries, because that is two lines of the
//! user's text. Members written inside a `class A { … }` block stay in the
//! block, and members written as `A : +foo()` statements stay statements; a
//! fence that mixes the two spellings comes back mixed, so confirming the
//! editor does not rewrite lines the user did not touch.
//!
//! What the parser rewrites, all of it stable from the first emit: indentation,
//! the position of `direction` (which moves up to just under the diagram
//! keyword), the whitespace inside a relation statement and before a comment
//! at the end of one, and a `namespace N { class A }` written on one line,
//! which comes back spread over three.

use std::fmt::Write as _;
use std::ops::Range;

use anyhow::{Result, anyhow, bail};
use merman::diagram::RenderSemanticModel;

use crate::flowchart_model::{FlowDirection, PreservedLine, PreservedPosition};
use crate::mermaid_model::{INDENT, first_statement_line, join_lines, scan_preamble};

/// The word a class diagram opens with.
///
/// `merman`'s lexer also takes `classDiagram-v2`, but Zed's own render gate
/// (`markdown::is_supported_diagram_type`) matches the first token exactly and
/// does not list that spelling, so a fence written that way is not drawn in the
/// preview and is not opened here either.
pub const CLASS_KEYWORD: &str = "classDiagram";

/// How deep a `namespace` may nest before the parser gives up. Mermaid's own
/// parser has a comparable limit; this one keeps a pathological fence from
/// recursing without bound.
const MAX_NESTING_DEPTH: usize = 16;

/// The words that open a statement the model does not carry, and that is
/// therefore held verbatim as a [`ClassItem::Preserved`].
///
/// `classDef` is here rather than in [`UNSUPPORTED_STATEMENT_KEYWORDS`] because
/// it defines a style class and names no class of the diagram, so nothing has
/// to follow a rename into it.
const PRESERVED_STATEMENT_KEYWORDS: [&str; 3] = ["classDef", "accTitle", "accDescr"];

/// The words that open a statement which addresses classes by name, and that
/// is held verbatim as a [`ClassItem::Preserved`] all the same.
///
/// None of them declares the class it names — Mermaid applies them to a class
/// that already exists and ignores them otherwise — so the words that name a
/// class are all an editor has to follow: [`rename_statement_targets`] carries
/// a rename into them and [`retain_statement_targets`] takes a deleted class
/// out of them.
const TARGETED_STATEMENT_KEYWORDS: [&str; 5] = ["style", "cssClass", "click", "link", "callback"];

/// The words that open no statement of a class diagram at all. `href` is only
/// ever the second word of a `click` statement, so a line that starts with it
/// is one Mermaid cannot read.
const UNSUPPORTED_STATEMENT_KEYWORDS: [&str; 1] = ["href"];

/// The relation operators the editor's menu offers, with the words it shows for
/// them. An operator the list does not hold still survives, because
/// [`ClassRelation::arrow`] keeps whatever the fence spelled.
pub const RELATION_PRESETS: [(&str, &str); 14] = [
    ("<|--", i18n::msg!("Inheritance (left is parent)")),
    ("--|>", i18n::msg!("Inheritance (right is parent)")),
    ("*--", i18n::msg!("Composition (left is whole)")),
    ("--*", i18n::msg!("Composition (right is whole)")),
    ("o--", i18n::msg!("Aggregation (left is whole)")),
    ("--o", i18n::msg!("Aggregation (right is whole)")),
    ("-->", i18n::msg!("Association (pointing right)")),
    ("<--", i18n::msg!("Association (pointing left)")),
    ("--", i18n::msg!("Association (line only)")),
    ("..>", i18n::msg!("Dependency (pointing right)")),
    ("<..", i18n::msg!("Dependency (pointing left)")),
    ("..|>", i18n::msg!("Realization (right is abstract)")),
    ("<|..", i18n::msg!("Realization (left is abstract)")),
    ("..", i18n::msg!("Dashed (line only)")),
];

/// A class as a statement names it: its name, and the `~T~` some statements
/// write after it.
///
/// Mermaid reads `List~int~` as the class `List` drawn with the type parameter
/// `int`, so the name is what everything else in the diagram refers to and the
/// type parameter travels beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassRef {
    pub name: String,
    pub generic: Option<String>,
}

impl ClassRef {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            generic: None,
        }
    }

    /// The reference as a statement spells it.
    pub fn text(&self) -> String {
        match &self.generic {
            Some(generic) => format!("{}~{generic}~", self.name),
            None => self.name.clone(),
        }
    }
}

/// One `class A["表示名"]:::style { … }` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassDecl {
    pub reference: ClassRef,
    /// The text of `class A["表示名"]`, drawn instead of the name.
    pub label: Option<String>,
    /// The style class a `class A:::warn` statement applies, which a `classDef`
    /// line defines. It is carried rather than edited: `class` is the only
    /// statement Mermaid takes it in, and a rename of the class it is written on
    /// does not touch it.
    pub css_class: Option<String>,
    /// The lines of the `{ … }` block, verbatim and trimmed, in the order they
    /// were written. `None` when the statement opened no block at all, which is
    /// a different fence from an empty one.
    ///
    /// The lines stay verbatim because Mermaid reads a member line as a whole —
    /// visibility, name, parameters, return type and `~T~` generics are pulled
    /// apart only when the picture is drawn — and a model that took them apart
    /// would have to put them back in a spelling of its own choosing. A blank
    /// line is an empty entry: Mermaid skips it, but the user put it there to
    /// group the members, and dropping it would rewrite a block nobody edited.
    pub members: Option<Vec<String>>,
    /// The `%% …` written at the end of the statement's line, after the `{`
    /// when it opens a block. See [`ClassRelation::comment`].
    pub comment: Option<String>,
}

/// One relation statement: `A "1" <|-- "many" B : ラベル`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassRelation {
    pub left: ClassRef,
    pub left_cardinality: Option<String>,
    /// The operator, verbatim, so that an arrow the presets do not list still
    /// comes back the way it was written.
    pub arrow: String,
    pub right_cardinality: Option<String>,
    pub right: ClassRef,
    /// Everything after the `:`, up to the end of the line. A `%%` in it is
    /// part of the label: Mermaid reads a label to the end of its line without
    /// looking for a comment.
    pub label: Option<String>,
    /// The `%% …` written at the end of the line, verbatim from the `%%` on.
    ///
    /// Mermaid drops it, so it is carried rather than edited, and written back
    /// after the statement. A statement that has a label cannot hold one there
    /// — it would become part of the label — so the emitter moves it to a line
    /// of its own above the statement instead.
    pub comment: Option<String>,
}

/// One `note "…"` or `note for A "…"` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassNote {
    /// `None` for a floating note that is attached to no class.
    pub target: Option<String>,
    pub text: String,
    /// See [`ClassRelation::comment`].
    pub comment: Option<String>,
}

/// One `namespace N { … }` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassNamespace {
    pub name: String,
    pub label: Option<String>,
    /// Mermaid only takes class declarations, notes and further namespaces
    /// inside a namespace, so a relation written in one makes the parse fail.
    pub items: Vec<ClassItem>,
    /// The `%% …` written after the `{`. See [`ClassRelation::comment`].
    pub comment: Option<String>,
}

/// One statement.
///
/// Order matters: a comment explains the statement it was written above, and a
/// class that is declared twice keeps both of its lines, so a flat set of
/// classes and a flat list of relations could not say what the fence says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassItem {
    Class(ClassDecl),
    /// `A : +foo()`, the spelling that adds one member without a block.
    Member {
        reference: ClassRef,
        text: String,
    },
    /// `<<interface>> A`, the spelling that adds an annotation without a block.
    Annotation {
        reference: ClassRef,
        annotation: String,
        /// See [`ClassRelation::comment`].
        comment: Option<String>,
    },
    Relation(ClassRelation),
    Note(ClassNote),
    Namespace(ClassNamespace),
    /// A line the model does not carry — a blank line, a `%%` comment, a
    /// `classDef`, an accessibility statement, or a `style` / `cssClass` /
    /// `click` / `link` / `callback` statement. Written back verbatim, in
    /// place, because a comment that moved out of the namespace it explains
    /// would be as bad as a comment that vanished. The last five name classes;
    /// [`statement_targets`] says where.
    Preserved(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassDocument {
    pub direction: Option<FlowDirection>,
    pub items: Vec<ClassItem>,
    /// The frontmatter, `%%{init}%%` directives and comments that come before
    /// the diagram keyword. Everything inside the diagram is a
    /// [`ClassItem::Preserved`] of the scope it was written in.
    pub preserved_lines: Vec<PreservedLine>,
}

impl ClassDocument {
    /// A blank class diagram, used when the editor is opened on an empty fence.
    pub fn empty() -> Self {
        Self {
            direction: None,
            items: Vec::new(),
            preserved_lines: Vec::new(),
        }
    }

    /// Every class name the diagram mentions, in the order it first appears.
    ///
    /// A relation names a class into existence in Mermaid, so this is not the
    /// list of `class` statements: it is every name the picture will hold.
    pub fn class_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        collect_class_names(&self.items, &mut names);
        names
    }

    /// A name no class is using, based on `prefix`.
    pub fn unused_name(&self, prefix: &str) -> String {
        let taken = self.class_names();
        let taken = |candidate: &str| taken.iter().any(|name| name == candidate);
        if !taken(prefix) {
            return prefix.to_string();
        }
        for suffix in 1..usize::MAX {
            let candidate = format!("{prefix}{suffix}");
            if !taken(&candidate) {
                return candidate;
            }
        }
        prefix.to_string()
    }
}

fn collect_class_names(items: &[ClassItem], names: &mut Vec<String>) {
    fn push(name: &str, names: &mut Vec<String>) {
        if !name.is_empty() && !names.iter().any(|known| known == name) {
            names.push(name.to_string());
        }
    }

    for item in items {
        match item {
            ClassItem::Class(declaration) => push(&declaration.reference.name, names),
            ClassItem::Member { reference, .. } | ClassItem::Annotation { reference, .. } => {
                push(&reference.name, names)
            }
            ClassItem::Relation(relation) => {
                push(&relation.left.name, names);
                push(&relation.right.name, names);
            }
            ClassItem::Note(note) => {
                if let Some(target) = &note.target {
                    push(target, names);
                }
            }
            ClassItem::Namespace(namespace) => collect_class_names(&namespace.items, names),
            ClassItem::Preserved(_) => {}
        }
    }
}

/// Whether `source` is a class diagram. Cheap enough to call before parsing.
pub fn is_class_source(source: &str) -> bool {
    first_statement_line(source).is_some_and(|line| word_tail(line, CLASS_KEYWORD).is_some())
}

/// Reads `source` — the body of a ```` ```mermaid ```` fence — into an editable
/// document.
pub fn parse_class(source: &str) -> Result<ClassDocument> {
    if let Some(problem) = crate::mermaid_model::preprocessing_problem(source) {
        bail!("{problem}");
    }
    let lines = source.lines().collect::<Vec<_>>();
    let preamble = scan_preamble(&lines);

    // `merman` 0.8 refuses `classDiagram` with no line after it, which is
    // what a blank diagram is written as, so a fence holding only the keyword
    // opens as the blank diagram it is without asking it, the way an empty
    // fence does.
    let has_body = preamble.header.is_some_and(|header| {
        lines
            .iter()
            .skip(header + 1)
            .any(|line| !line.trim().is_empty())
    });
    if has_body {
        let parsed = crate::parse_for_render_model(source)
            .map_err(|error| {
                anyhow!(i18n::t!(
                    "Mermaid could not parse this diagram ({error}). Edit it as text."
                ))
            })?
            .ok_or_else(|| anyhow!(i18n::t!("Mermaid could not read this as a diagram.")))?;
        if !matches!(parsed.model(), RenderSemanticModel::Class(_)) {
            bail!(i18n::t!("This diagram is not a class diagram."));
        }
    }

    // An empty fence: the editor opens on a blank diagram, and whatever
    // comments the user left keep their place.
    let Some(header_index) = preamble.header else {
        return Ok(ClassDocument {
            direction: None,
            items: Vec::new(),
            preserved_lines: preamble.preserved,
        });
    };

    let header = lines.get(header_index).copied().unwrap_or_default().trim();
    let Some(rest) = word_tail(header, CLASS_KEYWORD) else {
        bail!(i18n::t!(
            "This is not a class diagram (only diagrams that start with `classDiagram` can be edited in the structured editor)."
        ));
    };
    if !rest.is_empty() {
        bail!(i18n::t!(
            "`{rest}` follows `{CLASS_KEYWORD}` on the same line. Move it to the next line, then open it again."
        ));
    }

    let mut parser = ClassParser {
        lines: &lines,
        index: header_index + 1,
        direction: None,
    };
    let mut items = parser.parse_scope(0, false)?;
    // A blank line at the very end of the fence would be trimmed off by the
    // emitter, and the diagram would lose one item every time it was confirmed.
    while items
        .last()
        .is_some_and(|item| matches!(item, ClassItem::Preserved(text) if text.trim().is_empty()))
    {
        items.pop();
    }

    Ok(ClassDocument {
        direction: parser.direction,
        items,
        preserved_lines: preamble.preserved,
    })
}

struct ClassParser<'a> {
    lines: &'a [&'a str],
    index: usize,
    direction: Option<FlowDirection>,
}

impl ClassParser<'_> {
    /// Reads statements until the `}` that closes the scope, or until the end
    /// of the fence at the top level.
    fn parse_scope(&mut self, depth: usize, in_namespace: bool) -> Result<Vec<ClassItem>> {
        if depth > MAX_NESTING_DEPTH {
            bail!(i18n::t!(
                "The `namespace`s are nested too deeply. Edit it as text."
            ));
        }
        let mut items = Vec::new();

        while let Some(line) = self.lines.get(self.index).copied() {
            let trimmed = line.trim();
            let start = self.index;
            self.index += 1;

            if trimmed.is_empty() {
                items.push(ClassItem::Preserved(String::new()));
                continue;
            }
            if trimmed == "}" {
                if !in_namespace {
                    bail!(i18n::t!("There is a `}}` without a matching `{{`."));
                }
                return Ok(items);
            }
            refuse_trailing_brace(trimmed)?;
            if trimmed.starts_with("%%") {
                if trimmed.starts_with("%%{") && !trimmed.contains("}%%") {
                    while self
                        .lines
                        .get(self.index)
                        .is_some_and(|line| !line.contains("}%%"))
                    {
                        self.index += 1;
                    }
                    if self.index < self.lines.len() {
                        self.index += 1;
                    }
                }
                items.push(ClassItem::Preserved(self.preserved(start)));
                continue;
            }

            if let Some(direction) = self.take_direction(trimmed, in_namespace)? {
                self.direction = Some(direction);
                continue;
            }
            // These are held verbatim, a comment at the end included: Mermaid
            // reads the rest of a `classDef` or `style` line as its value, and
            // a line that comes back byte for byte cannot lose anything.
            if let Some(item) = self.take_preserved_statement(trimmed, start, in_namespace)? {
                items.push(item);
                continue;
            }
            if let Some(item) = self.take_targeted_statement(trimmed, start, in_namespace)? {
                items.push(item);
                continue;
            }
            for keyword in UNSUPPORTED_STATEMENT_KEYWORDS {
                if word_tail(trimmed, keyword).is_some() {
                    bail!(i18n::t!(
                        "`{keyword}` statements are not supported by the structured editor ({trimmed}). Edit it as text."
                    ));
                }
            }

            let (statement, comment) = split_trailing_comment(trimmed);
            items.push(self.parse_statement(statement, comment, trimmed, depth, in_namespace)?);
        }

        if in_namespace {
            bail!(i18n::t!("The `{{` of a `namespace` has no matching `}}`."));
        }
        Ok(items)
    }

    /// One statement the model carries, with the comment that was written at
    /// the end of its line split off already. `line` is the whole line, for
    /// the messages.
    fn parse_statement(
        &mut self,
        statement: &str,
        comment: Option<String>,
        line: &str,
        depth: usize,
        in_namespace: bool,
    ) -> Result<ClassItem> {
        if let Some(rest) = word_tail(statement, "namespace") {
            return self.parse_namespace(rest, line, comment, depth);
        }
        if let Some(rest) = word_tail(statement, "class") {
            let mut declaration = self.parse_class_statement(rest, line)?;
            declaration.comment = comment;
            return Ok(ClassItem::Class(declaration));
        }
        if let Some(rest) = word_tail(statement, "note") {
            let mut note = parse_note(rest, line)?;
            note.comment = comment;
            return Ok(ClassItem::Note(note));
        }

        if in_namespace {
            bail!(i18n::t!(
                "Only class declarations, notes and nested `namespace`s can be written inside a `namespace` ({line})."
            ));
        }
        let mut item = parse_plain_statement(statement)?;
        match &mut item {
            ClassItem::Relation(relation) => relation.comment = comment,
            ClassItem::Annotation {
                comment: carried, ..
            } => *carried = comment,
            // A member statement ends in a label, which runs to the end of the
            // line, so no comment was split off one.
            _ => {}
        }
        Ok(item)
    }

    /// The lines from `start` up to the cursor, verbatim.
    fn preserved(&self, start: usize) -> String {
        join_lines(self.lines.get(start..self.index).unwrap_or_default())
    }

    /// The direction `statement` sets, if it is a direction statement at all.
    fn take_direction(&self, statement: &str, in_namespace: bool) -> Result<Option<FlowDirection>> {
        let Some(rest) = word_tail(statement, "direction") else {
            return Ok(None);
        };
        let Some(direction) = FlowDirection::ALL
            .into_iter()
            .find(|direction| rest.starts_with(direction.keyword()))
        else {
            // Mermaid's lexer swallows the word and the whitespace after it
            // before it looks for the direction, so what is left of the line is
            // read as if `direction` had never been there.
            bail!(i18n::t!(
                "Write one of TB / BT / LR / RL after `direction` ({statement})."
            ));
        };
        let after = rest
            .get(direction.keyword().len()..)
            .unwrap_or_default()
            .trim();
        if !after.is_empty() {
            // Mermaid reads the rest of the line and throws it away.
            bail!(i18n::t!(
                "Mermaid skips `{after}` after `direction {}` ({statement}). Remove it, then open it again.",
                direction.keyword()
            ));
        }
        if in_namespace {
            bail!(i18n::t!(
                "`direction` cannot be written inside a `namespace` ({statement})."
            ));
        }
        if self.direction.is_some() {
            bail!(i18n::t!(
                "`direction` is written twice. Merge them into one, then open it again ({statement})."
            ));
        }
        Ok(Some(direction))
    }

    /// The statement held verbatim that `statement` opens, if it is one.
    /// Advances the cursor past an `accDescr { … }` block.
    fn take_preserved_statement(
        &mut self,
        statement: &str,
        start: usize,
        in_namespace: bool,
    ) -> Result<Option<ClassItem>> {
        let Some(keyword) = PRESERVED_STATEMENT_KEYWORDS
            .into_iter()
            .find(|keyword| word_tail(statement, keyword).is_some())
        else {
            return Ok(None);
        };
        if in_namespace {
            bail!(i18n::t!(
                "`{keyword}` cannot be written inside a `namespace` ({statement})."
            ));
        }
        if keyword == "accDescr"
            && word_tail(statement, "accDescr").is_some_and(|rest| rest.starts_with('{'))
            && !statement.contains('}')
        {
            while self
                .lines
                .get(self.index)
                .is_some_and(|line| !line.contains('}'))
            {
                self.index += 1;
            }
            if self.index < self.lines.len() {
                self.index += 1;
            }
        }
        Ok(Some(ClassItem::Preserved(self.preserved(start))))
    }

    /// A `style`, `cssClass`, `click`, `link` or `callback` statement, held
    /// verbatim, if `statement` is one.
    fn take_targeted_statement(
        &self,
        statement: &str,
        start: usize,
        in_namespace: bool,
    ) -> Result<Option<ClassItem>> {
        let Some(keyword) = TARGETED_STATEMENT_KEYWORDS
            .into_iter()
            .find(|keyword| word_tail(statement, keyword).is_some())
        else {
            return Ok(None);
        };
        if in_namespace {
            bail!(i18n::t!(
                "`{keyword}` cannot be written inside a `namespace` ({statement})."
            ));
        }
        // A class the editor cannot find in the line is one it cannot carry a
        // rename into, and the statement would quietly stop applying to it.
        if statement_targets(statement).is_none() {
            bail!(i18n::t!(
                "Could not read the class name `{keyword}` applies to ({statement}). Names enclosed in `` ` `` are not supported by the structured editor. Edit it as text."
            ));
        }
        Ok(Some(ClassItem::Preserved(self.preserved(start))))
    }

    fn parse_namespace(
        &mut self,
        rest: &str,
        statement: &str,
        comment: Option<String>,
        depth: usize,
    ) -> Result<ClassItem> {
        let (name, rest) = take_name(rest, statement)?;
        if !is_valid_class_name(&name) {
            bail!(i18n::t!(
                "`{name}` cannot be used as a `namespace` name ({statement})."
            ));
        }
        let (label, rest) = take_label(rest, statement)?;
        let rest = rest.trim();
        let Some(after_brace) = rest.strip_prefix('{') else {
            bail!(i18n::t!("Write it as `namespace {name} {{` ({statement})."));
        };
        let after_brace = after_brace.trim();
        let items = match after_brace.is_empty() {
            true => self.parse_scope(depth + 1, true)?,
            false => parse_one_line_namespace(after_brace, statement, depth)?,
        };
        Ok(ClassItem::Namespace(ClassNamespace {
            name,
            label,
            items,
            comment,
        }))
    }

    fn parse_class_statement(&mut self, rest: &str, statement: &str) -> Result<ClassDecl> {
        let (reference, rest) = take_reference(rest, statement)?;
        let (label, rest) = take_label(rest, statement)?;
        // Mermaid reads the three parts in this order and in no other, so a
        // `:::` before the label or after the `{` is not a style class at all.
        let (css_class, rest) = take_css_class(rest, statement)?;
        let rest = rest.trim();
        let members = match rest.strip_prefix('{') {
            None => {
                if !rest.is_empty() {
                    bail!(i18n::t!(
                        "Could not read `{rest}` after the `class` statement ({statement})."
                    ));
                }
                None
            }
            Some(after_brace) => {
                if !after_brace.trim().is_empty() {
                    bail!(i18n::t!(
                        "Start a new line after `{{` ({statement}). Mermaid cannot read a member block written on one line either."
                    ));
                }
                Some(self.parse_members(statement)?)
            }
        };
        Ok(ClassDecl {
            reference,
            label,
            css_class,
            members,
            comment: None,
        })
    }

    /// Reads the lines of a `class A { … }` block.
    ///
    /// Mermaid reads every line of the block as one member and ends the block
    /// at a line that is nothing but `}`, so there is no brace counting to get
    /// wrong: a `{` inside a member, a label or a generic is part of that line
    /// and nothing more.
    fn parse_members(&mut self, statement: &str) -> Result<Vec<String>> {
        let mut members = Vec::new();
        loop {
            let Some(line) = self.lines.get(self.index).copied() else {
                bail!(i18n::t!(
                    "The member block of `class` has no matching `}}` ({statement})."
                ));
            };
            self.index += 1;
            let trimmed = line.trim();
            if trimmed == "}" {
                return Ok(members);
            }
            refuse_trailing_brace(trimmed)?;
            members.push(trimmed.to_string());
        }
    }
}

/// The statements of a `namespace N { … }` written on one line: what `rest`
/// holds after the `{`.
///
/// Mermaid's grammar takes a namespace body with no line break in it, but only
/// one statement fits there, since the statements of a body are separated by
/// line breaks. That is exactly what reading the body as its own two lines —
/// the statement, then the `}` — accepts, so the scope parser does the work and
/// every rule it has for a namespace applies here too.
fn parse_one_line_namespace(rest: &str, statement: &str, depth: usize) -> Result<Vec<ClassItem>> {
    let Some(inner) = rest.strip_suffix('}') else {
        bail!(i18n::t!(
            "Close a one-line `namespace` with `}}` on the same line ({statement}). Split it into several lines, or edit it as text."
        ));
    };
    let inner = inner.trim();
    let lines = match inner.is_empty() {
        true => vec!["}"],
        false => vec![inner, "}"],
    };
    let mut parser = ClassParser {
        lines: &lines,
        index: 0,
        direction: None,
    };
    let items = parser.parse_scope(depth + 1, true)?;
    // A `class A {` in there would have opened a block that took the closing
    // brace as its own, and the scope would have failed for want of another.
    if parser.index != lines.len() {
        bail!(i18n::t!(
            "Could not read the one-line `namespace` ({statement})."
        ));
    }
    Ok(items)
}

/// Splits the comment at the end of `statement` off, if it has one.
///
/// Mermaid's lexer looks for a comment wherever a token could start, so a `%%`
/// after whitespace, a closing quote or a bracket opens one that runs to the
/// end of the line. It does not look inside a token: a `%%` right after a name
/// is part of that name, and a `:` opens a label or a member that runs to the
/// end of the line, `%%` and all. Quoted strings are skipped for the same
/// reason.
fn split_trailing_comment(statement: &str) -> (&str, Option<String>) {
    let mut quoted = false;
    let mut previous: Option<char> = None;
    let mut characters = statement.char_indices().peekable();
    while let Some((offset, character)) = characters.next() {
        if character == '"' {
            quoted = !quoted;
        } else if !quoted {
            let rest = statement.get(offset..).unwrap_or_default();
            if character == ':' {
                if !rest.starts_with(":::") {
                    break;
                }
                characters.next();
                characters.next();
                previous = Some(':');
                continue;
            }
            if rest.starts_with("%%")
                && previous.is_none_or(|previous| {
                    previous.is_whitespace() || matches!(previous, '"' | '[' | ']' | '{' | '}')
                })
            {
                let head = statement.get(..offset).unwrap_or_default().trim_end();
                return (head, Some(rest.trim_end().to_string()));
            }
        }
        previous = Some(character);
    }
    (statement, None)
}

/// A relation, a member statement or an annotation statement.
fn parse_plain_statement(statement: &str) -> Result<ClassItem> {
    if let Some(rest) = statement.strip_prefix("<<") {
        let Some((annotation, after)) = rest.split_once(">>") else {
            bail!(i18n::t!("`<<annotation>>` is not closed ({statement})."));
        };
        let annotation = annotation.trim();
        if !is_valid_annotation(annotation) {
            bail!(i18n::t!(
                "`{annotation}` cannot be used as an annotation ({statement})."
            ));
        }
        let (reference, after) = take_reference(after.trim_start(), statement)?;
        if !after.trim().is_empty() {
            bail!(i18n::t!(
                "Could not read `{}` after `<<annotation>> ClassName` ({statement}).",
                after.trim()
            ));
        }
        return Ok(ClassItem::Annotation {
            reference,
            annotation: annotation.to_string(),
            comment: None,
        });
    }

    let (reference, rest) = take_reference(statement, statement)?;
    let rest = rest.trim_start();

    if rest.starts_with(":::") {
        bail!(i18n::t!(
            "Style classes applied with `:::` are not supported by the structured editor ({statement}). Edit it as text."
        ));
    }
    // A name ends at `:`, so a statement whose next character is one adds a
    // member to the class rather than relating it to another.
    if let Some(text) = rest.strip_prefix(':') {
        return Ok(ClassItem::Member {
            reference,
            text: text.trim().to_string(),
        });
    }

    let (left_cardinality, rest) = take_cardinality(rest, statement)?;
    let Some((arrow, rest)) = split_arrow(rest.trim_start()) else {
        bail!(i18n::t!(
            "Could not read `{statement}`. A class diagram statement is one of `class A`, `A : member`, `A --> B`, `<<annotation>> A` and `note for A \"…\"`."
        ));
    };
    let (right_cardinality, rest) = take_cardinality(rest.trim_start(), statement)?;
    let (right, rest) = take_reference(rest.trim_start(), statement)?;
    let rest = rest.trim();
    let label = match rest.strip_prefix(':') {
        Some(_) if rest.starts_with(":::") => bail!(i18n::t!(
            "Style classes applied with `:::` are not supported by the structured editor ({statement}). Edit it as text."
        )),
        Some(label) => Some(label.trim().to_string()),
        None if rest.is_empty() => None,
        None => bail!(i18n::t!(
            "Could not read `{rest}` after the relation ({statement})."
        )),
    };

    Ok(ClassItem::Relation(ClassRelation {
        left: reference,
        left_cardinality,
        arrow: arrow.to_string(),
        right_cardinality,
        right,
        label,
        comment: None,
    }))
}

fn parse_note(rest: &str, statement: &str) -> Result<ClassNote> {
    let (target, rest) = match word_tail(rest, "for") {
        Some(after_for) => {
            let (name, rest) = take_name(after_for, statement)?;
            if !is_valid_class_name(&name) {
                bail!(i18n::t!(
                    "`{name}` cannot be used as a class name ({statement})."
                ));
            }
            (Some(name), rest)
        }
        None => (None, rest),
    };
    let rest = rest.trim_start();
    let Some(quoted) = rest.strip_prefix('"') else {
        bail!(i18n::t!("Write it as `note \"text\"` ({statement})."));
    };
    let Some((text, after)) = quoted.split_once('"') else {
        bail!(i18n::t!(
            "The quotes around the note text are not closed ({statement})."
        ));
    };
    if !after.trim().is_empty() {
        bail!(i18n::t!(
            "Could not read `{}` after the note ({statement}).",
            after.trim()
        ));
    }
    Ok(ClassNote {
        target,
        text: text.to_string(),
        comment: None,
    })
}

/// What follows `keyword` at the start of `line`, with its leading whitespace
/// removed, or `None` when the line does not start with that word.
///
/// The word has to end where `merman`'s class lexer ends it, and the case has
/// to match: that lexer compares the keywords byte for byte, so `Class A`
/// declares nothing and `class` is not a name.
fn word_tail<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(keyword)?;
    if rest.is_empty()
        || rest.starts_with(|character: char| {
            character.is_whitespace()
                || matches!(character, '{' | '}' | '[' | ']' | '"' | '`' | ':')
        })
    {
        return Some(rest.trim_start());
    }
    None
}

/// How much of `text` Mermaid's lexer reads as one name.
fn name_length(text: &str) -> usize {
    let mut offset = 0;
    while let Some(rest) = text.get(offset..) {
        let Some(character) = rest.chars().next() else {
            break;
        };
        if character.is_whitespace()
            || matches!(
                character,
                '{' | '}' | '[' | ']' | '"' | ',' | ':' | '<' | '>'
            )
            || rest.starts_with("--")
            || rest.starts_with("..")
        {
            break;
        }
        offset += character.len_utf8();
    }
    offset
}

/// Splits the name at the start of `text` off, and what follows it.
fn take_name<'a>(text: &'a str, statement: &str) -> Result<(String, &'a str)> {
    let text = text.trim_start();
    if text.starts_with('`') {
        bail!(i18n::t!(
            "Names enclosed in `` ` `` are not supported by the structured editor ({statement}). Edit it as text."
        ));
    }
    let length = name_length(text);
    let Some(name) = text.get(..length).filter(|name| !name.is_empty()) else {
        bail!(i18n::t!("Could not read the class name ({statement})."));
    };
    Ok((name.to_string(), text.get(length..).unwrap_or_default()))
}

/// Splits the class reference at the start of `text` off, `~T~` and all.
fn take_reference<'a>(text: &'a str, statement: &str) -> Result<(ClassRef, &'a str)> {
    let (token, rest) = take_name(text, statement)?;
    let reference = match token.split_once('~') {
        None => ClassRef {
            name: token,
            generic: None,
        },
        Some((name, tail)) => {
            let Some(generic) = tail.strip_suffix('~') else {
                bail!(i18n::t!(
                    "Write a generic type as `{name}~T~` ({statement})."
                ));
            };
            if !is_valid_generic(generic) {
                bail!(i18n::t!(
                    "`{generic}` cannot be used as a generic type ({statement})."
                ));
            }
            ClassRef {
                name: name.to_string(),
                generic: Some(generic.to_string()),
            }
        }
    };
    if !is_valid_class_name(&reference.name) {
        bail!(i18n::t!(
            "`{}` cannot be used as a class name ({statement}). Spaces, symbols and Mermaid's reserved words are not allowed.",
            reference.name
        ));
    }
    Ok((reference, rest))
}

/// Splits a `["表示名"]` off the start of `text`, if there is one.
fn take_label<'a>(text: &'a str, statement: &str) -> Result<(Option<String>, &'a str)> {
    let trimmed = text.trim_start();
    let Some(after_bracket) = trimmed.strip_prefix('[') else {
        return Ok((None, text));
    };
    let Some(quoted) = after_bracket.trim_start().strip_prefix('"') else {
        bail!(i18n::t!("Write it as `[\"display name\"]` ({statement})."));
    };
    let Some((label, after)) = quoted.split_once('"') else {
        bail!(i18n::t!(
            "The quotes around the display name are not closed ({statement})."
        ));
    };
    let Some(rest) = after.trim_start().strip_prefix(']') else {
        bail!(i18n::t!(
            "The `]` of `[\"display name\"]` is missing ({statement})."
        ));
    };
    Ok((Some(label.to_string()), rest))
}

/// Splits a `:::style` off the start of `text`, if there is one.
///
/// Mermaid takes it only between the label and the `{`, and only on a `class`
/// statement, so nothing else in this module looks for one.
fn take_css_class<'a>(text: &'a str, statement: &str) -> Result<(Option<String>, &'a str)> {
    let trimmed = text.trim_start();
    let Some(after_marker) = trimmed.strip_prefix(":::") else {
        return Ok((None, text));
    };
    let after_marker = after_marker.trim_start();
    if after_marker.starts_with('`') {
        bail!(i18n::t!(
            "Style class names enclosed in `` ` `` are not supported by the structured editor ({statement}). Edit it as text."
        ));
    }
    let length = name_length(after_marker);
    let Some(name) = after_marker.get(..length).filter(|name| !name.is_empty()) else {
        bail!(i18n::t!(
            "There is no style class name after `:::` ({statement})."
        ));
    };
    if !is_valid_css_class(name) {
        bail!(i18n::t!(
            "`{name}` cannot be used as a `:::` style class name ({statement})."
        ));
    }
    Ok((
        Some(name.to_string()),
        after_marker.get(length..).unwrap_or_default(),
    ))
}

/// Refuses a line that closes a block and then goes on saying something else.
///
/// Mermaid's grammar closes the block at the `}` and reads the rest of the line
/// on its own, so a `} %% メモ` would close the block there and the emitter,
/// which writes a bare `}`, would drop everything that followed the brace.
fn refuse_trailing_brace(trimmed: &str) -> Result<()> {
    if let Some(rest) = trimmed.strip_prefix('}')
        && !rest.trim().is_empty()
    {
        bail!(i18n::t!(
            "`{}` follows `}}` on the same line ({trimmed}). Leave `}}` alone on its line, then open it again.",
            rest.trim()
        ));
    }
    Ok(())
}

/// Splits a `"1..*"` cardinality off the start of `text`, if there is one.
fn take_cardinality<'a>(text: &'a str, statement: &str) -> Result<(Option<String>, &'a str)> {
    let Some(quoted) = text.strip_prefix('"') else {
        return Ok((None, text));
    };
    let Some((cardinality, rest)) = quoted.split_once('"') else {
        bail!(i18n::t!(
            "The quotes around the multiplicity are not closed ({statement})."
        ));
    };
    // Mermaid trims the text and drops it when nothing is left, so an empty
    // pair of quotes is the same fence as no quotes at all.
    let cardinality = cardinality.trim();
    Ok((
        (!cardinality.is_empty()).then(|| cardinality.to_string()),
        rest,
    ))
}

/// Splits the relation operator at the start of `text` off, and what follows
/// it: an optional marker, the line, and an optional marker, with no whitespace
/// between them.
fn split_arrow(text: &str) -> Option<(&str, &str)> {
    let after_head = split_relation_marker(text);
    let after_line = after_head
        .strip_prefix("--")
        .or_else(|| after_head.strip_prefix(".."))?;
    let rest = split_relation_marker(after_line);
    let length = text.len().checked_sub(rest.len())?;
    Some((text.get(..length)?, rest))
}

/// `text` with the relation marker it opens with, if any, taken off.
fn split_relation_marker(text: &str) -> &str {
    // The two character markers come first: `<|` would otherwise be read as the
    // `<` of a dependency followed by a `|` no rule accepts.
    for marker in ["<|", "|>", "()", "*", "o", "<", ">"] {
        if let Some(rest) = text.strip_prefix(marker) {
            return rest;
        }
    }
    text
}

/// Writes `document` back as Mermaid source, without a trailing newline.
pub fn emit_class(document: &ClassDocument) -> String {
    let mut out = String::new();

    for line in preserved_at(document, PreservedPosition::Preamble) {
        out.push_str(line);
        out.push('\n');
    }

    out.push_str(CLASS_KEYWORD);
    out.push('\n');

    for line in preserved_at(document, PreservedPosition::Top) {
        out.push_str(line);
        out.push('\n');
    }

    if let Some(direction) = document.direction {
        let _ = writeln!(out, "{INDENT}direction {}", direction.keyword());
    }
    emit_items(&document.items, 1, &mut out);

    for line in preserved_at(document, PreservedPosition::Bottom) {
        out.push_str(line);
        out.push('\n');
    }

    while out.ends_with('\n') {
        out.pop();
    }
    out
}

fn preserved_at(
    document: &ClassDocument,
    position: PreservedPosition,
) -> impl Iterator<Item = &str> {
    document
        .preserved_lines
        .iter()
        .filter(move |line| line.position == position)
        .map(|line| line.text.as_str())
}

fn emit_items(items: &[ClassItem], depth: usize, out: &mut String) {
    let indent = INDENT.repeat(depth);

    for item in items {
        match item {
            ClassItem::Preserved(text) => {
                out.push_str(text);
                out.push('\n');
            }
            ClassItem::Class(declaration) => emit_class_decl(declaration, &indent, out),
            ClassItem::Member { reference, text } => match text.is_empty() {
                true => {
                    let _ = writeln!(out, "{indent}{} :", reference.text());
                }
                false => {
                    let _ = writeln!(out, "{indent}{} : {text}", reference.text());
                }
            },
            ClassItem::Annotation {
                reference,
                annotation,
                comment,
            } => {
                let statement = format!("{indent}<<{annotation}>> {}", reference.text());
                write_line(out, &statement, comment);
            }
            ClassItem::Relation(relation) => {
                let statement = format!("{indent}{}", relation_statement(relation));
                match (&relation.label, &relation.comment) {
                    // Written after a label, the comment would become part of
                    // it, so it goes on a line of its own just above.
                    (Some(_), Some(comment)) => {
                        let _ = writeln!(out, "{indent}{comment}");
                        write_line(out, &statement, &None);
                    }
                    _ => write_line(out, &statement, &relation.comment),
                }
            }
            ClassItem::Note(note) => {
                let statement = match &note.target {
                    Some(target) => format!("{indent}note for {target} \"{}\"", note.text),
                    None => format!("{indent}note \"{}\"", note.text),
                };
                write_line(out, &statement, &note.comment);
            }
            ClassItem::Namespace(namespace) => {
                let label = match &namespace.label {
                    Some(label) => format!("[\"{label}\"]"),
                    None => String::new(),
                };
                let head = format!("{indent}namespace {}{label} {{", namespace.name);
                write_line(out, &head, &namespace.comment);
                emit_items(&namespace.items, depth + 1, out);
                let _ = writeln!(out, "{indent}}}");
            }
        }
    }
}

/// Writes `statement` as one line, with the comment that was written at the
/// end of it.
fn write_line(out: &mut String, statement: &str, comment: &Option<String>) {
    match comment {
        Some(comment) => {
            let _ = writeln!(out, "{statement} {comment}");
        }
        None => {
            let _ = writeln!(out, "{statement}");
        }
    }
}

fn emit_class_decl(declaration: &ClassDecl, indent: &str, out: &mut String) {
    let label = match &declaration.label {
        Some(label) => format!("[\"{label}\"]"),
        None => String::new(),
    };
    let css = match &declaration.css_class {
        Some(css_class) => format!(":::{css_class}"),
        None => String::new(),
    };
    let head = format!("{indent}class {}{label}{css}", declaration.reference.text());
    let Some(members) = &declaration.members else {
        write_line(out, &head, &declaration.comment);
        return;
    };
    write_line(out, &format!("{head} {{"), &declaration.comment);
    for member in members {
        match member.is_empty() {
            true => out.push('\n'),
            false => {
                let _ = writeln!(out, "{indent}{INDENT}{member}");
            }
        }
    }
    let _ = writeln!(out, "{indent}}}");
}

fn relation_statement(relation: &ClassRelation) -> String {
    let mut statement = relation.left.text();
    if let Some(cardinality) = &relation.left_cardinality {
        let _ = write!(statement, " \"{cardinality}\"");
    }
    let _ = write!(statement, " {}", relation.arrow);
    if let Some(cardinality) = &relation.right_cardinality {
        let _ = write!(statement, " \"{cardinality}\"");
    }
    let _ = write!(statement, " {}", relation.right.text());
    match &relation.label {
        None => {}
        Some(label) if label.is_empty() => statement.push_str(" :"),
        Some(label) => {
            let _ = write!(statement, " : {label}");
        }
    }
    statement
}

/// Whether `name` can be written as a class or namespace name.
///
/// Mermaid's lexer ends a name at whitespace, at `{}[]",:<>`, at `--` and at
/// `..`, and reads a leading `~` as the start of a type parameter. A name that
/// begins with a digit is rewritten by Mermaid with a prefix of its own, so
/// that one is out too. Everything outside ASCII is allowed, so a Japanese
/// name works.
pub fn is_valid_class_name(name: &str) -> bool {
    is_valid_css_class(name) && !is_reserved_class_name(name)
}

/// Whether `name` can be written as the style class of a `class A:::name`
/// statement.
///
/// Mermaid ends it exactly where it ends a class name. The words that open a
/// statement are no trouble here, because a style class never stands at the
/// start of one.
pub fn is_valid_css_class(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|character: char| character.is_ascii_digit())
        && !name.starts_with(['-', '.'])
        && !name.ends_with(['-', '.'])
        && !name.contains("--")
        && !name.contains("..")
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, '_' | '.' | '-')
                || (!character.is_ascii() && !character.is_whitespace() && !character.is_control())
        })
}

/// The words Mermaid's lexer always turns into a keyword at the start of a
/// statement, so that a class named after one would not be read back as the
/// class the editor showed.
fn is_reserved_class_name(name: &str) -> bool {
    const RESERVED: [&str; 17] = [
        "class",
        "namespace",
        "note",
        "cssClass",
        "style",
        "classDef",
        "click",
        "link",
        "callback",
        "href",
        "accTitle",
        "accDescr",
        // Its lexer eats the word and the whitespace after it before it looks
        // for the direction, and keeps going from there when it finds none.
        "direction",
        "_self",
        "_blank",
        "_parent",
        "_top",
    ];
    RESERVED.contains(&name) || name == CLASS_KEYWORD
}

/// Whether `generic` can be written as the `~T~` of a class reference.
pub fn is_valid_generic(generic: &str) -> bool {
    !generic.is_empty()
        && !generic.contains('~')
        && generic.chars().all(|character| {
            !character.is_whitespace()
                && !character.is_control()
                && !matches!(
                    character,
                    '{' | '}' | '[' | ']' | '"' | ',' | ':' | '<' | '>'
                )
        })
        && !generic.contains("--")
        && !generic.contains("..")
}

/// Whether `text` can be written inside the quotes of a label, a note or a
/// cardinality: Mermaid ends a quoted string at the next quote, and a statement
/// at the end of the line.
pub fn is_valid_quoted_text(text: &str) -> bool {
    !text.contains('"')
        && !text.contains('\n')
        && !text.contains('\r')
        && !crate::mermaid_model::opens_directive(text)
}

/// Whether `annotation` can be written between `<<` and `>>`.
pub fn is_valid_annotation(annotation: &str) -> bool {
    !annotation.is_empty()
        && annotation.trim() == annotation
        && !annotation.contains('<')
        && !annotation.contains('>')
        && !annotation.contains('"')
        && !annotation.contains('\n')
        && !annotation.contains("%%")
}

/// Whether `text` can be written after the `:` of a relation statement.
///
/// The label runs to the end of its line and comes back trimmed. A `%%` in it
/// is part of the label: Mermaid's lexer reads a label to the end of the line
/// without looking for a comment, and so does this parser.
pub fn is_valid_relation_label(text: &str) -> bool {
    !text.contains('\n')
        && !text.contains('\r')
        && text.trim() == text
        && !crate::mermaid_model::opens_directive(text)
}

/// Whether `text` can be written as the member of an `A : メンバー` statement.
///
/// The same as a relation label, except that a line which is nothing but `}`
/// would read as the end of a block.
pub fn is_valid_member_text(text: &str) -> bool {
    is_valid_relation_label(text) && text != "}"
}

/// Whether `text` can be written as one line of a `class A { … }` block.
///
/// A block keeps its lines verbatim and Mermaid reads each one whole, so a `%%`
/// inside one belongs to the member rather than opening a comment — which is
/// why a comment written inside a block has to stay valid here, or a diagram
/// that holds one could be opened and never written back. An empty line is a
/// blank line in the block, which Mermaid skips. A line that starts with `}`
/// would close the block there.
pub fn is_valid_block_member_text(text: &str) -> bool {
    !text.contains('\n')
        && !text.contains('\r')
        && text.trim() == text
        && !text.starts_with('}')
        && !crate::mermaid_model::opens_directive(text)
}

/// Whether `arrow` is a relation operator Mermaid reads as one.
pub fn is_valid_relation_arrow(arrow: &str) -> bool {
    split_arrow(arrow).is_some_and(|(taken, rest)| taken.len() == arrow.len() && rest.is_empty())
}

/// Where in `line` a kept `style`, `cssClass`, `click`, `link` or `callback`
/// statement names the classes it addresses, as byte ranges of the line.
///
/// `None` when the line is not one of those statements, or names its class in
/// a spelling this cannot follow (a name in backquotes). A range covers the
/// class name and not the `~T~` Mermaid lets a statement write after it, since
/// Mermaid looks the class up by the name alone.
pub fn statement_targets(line: &str) -> Option<Vec<Range<usize>>> {
    let trimmed = line.trim_start();
    let keyword = TARGETED_STATEMENT_KEYWORDS
        .into_iter()
        .find(|keyword| word_tail(trimmed, keyword).is_some())?;
    let rest = word_tail(trimmed, keyword)?;
    // `rest` is a tail of `line`, so its offset in `line` is what is left over.
    let rest_start = line.len() - rest.len();

    if keyword != "cssClass" {
        if rest.starts_with('`') {
            return None;
        }
        let length = target_name_length(rest.get(..name_length(rest))?);
        return (length > 0).then(|| vec![rest_start..rest_start + length]);
    }

    // `cssClass "A,B" warn`: Mermaid splits the quoted text at the commas and
    // trims each entry.
    let content = rest.strip_prefix('"')?;
    let content_length = content.find('"')?;
    let content_start = rest_start + 1;
    let mut targets = Vec::new();
    let mut entry_start = content_start;
    for entry in content.get(..content_length)?.split(',') {
        let leading = entry.len() - entry.trim_start().len();
        let length = target_name_length(entry.trim());
        if length > 0 {
            let start = entry_start + leading;
            targets.push(start..start + length);
        }
        entry_start += entry.len() + 1;
    }
    Some(targets)
}

/// How much of a statement's target is the class name, which is all of it but
/// a `~T~` after it.
fn target_name_length(target: &str) -> usize {
    target.find('~').unwrap_or(target.len())
}

/// `line` with every class it addresses renamed through `renamed`, which says
/// the new name of a class that has one.
///
/// A line that addresses no class, or none that was renamed, comes back as it
/// was.
pub fn rename_statement_targets(line: &str, renamed: impl Fn(&str) -> Option<String>) -> String {
    let mut out = line.to_string();
    let Some(targets) = statement_targets(line) else {
        return out;
    };
    // From the end, so that a replacement does not move the ranges before it.
    for range in targets.into_iter().rev() {
        let Some(new_name) = line.get(range.clone()).and_then(&renamed) else {
            continue;
        };
        out.replace_range(range, &new_name);
    }
    out
}

/// `line` without the classes `keep` turns down, or `None` when that leaves it
/// addressing nothing.
///
/// A `style`, `click`, `link` or `callback` statement names one class, so it
/// either stays whole or goes. A `cssClass` statement names a list of them, and
/// only the entries turned down are taken out of the list; the line goes when
/// none are left. A line that addresses no class comes back as it was.
pub fn retain_statement_targets(line: &str, keep: impl Fn(&str) -> bool) -> Option<String> {
    let Some(targets) = statement_targets(line) else {
        return Some(line.to_string());
    };
    let names = targets
        .iter()
        .filter_map(|range| line.get(range.clone()))
        .collect::<Vec<_>>();
    if names.iter().all(|name| keep(name)) {
        return Some(line.to_string());
    }
    if !names.iter().any(|name| keep(name)) {
        return None;
    }

    // Only a `cssClass` list gets here: it has a name to keep and one to drop.
    let open = line.find('"')?;
    let close = open + 1 + line.get(open + 1..)?.find('"')?;
    let content = line.get(open + 1..close)?;
    let kept = content
        .split(',')
        .filter(|entry| {
            let entry = entry.trim();
            let name = entry.get(..target_name_length(entry)).unwrap_or_default();
            name.is_empty() || keep(name)
        })
        .collect::<Vec<_>>()
        .join(",");
    // The entry taken out may have been the first, and the space that followed
    // its comma would then open the list.
    let kept = match content.starts_with(char::is_whitespace) {
        true => kept,
        false => kept.trim_start().to_string(),
    };
    let mut out = line.to_string();
    out.replace_range(open + 1..close, &kept);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MermaidTheme, render_to_svg};

    /// The emitted source must parse to the same document and emit the same
    /// text, otherwise confirming the editor twice would keep rewriting the
    /// fence.
    fn assert_stable(source: &str) -> String {
        let first =
            parse_class(source).unwrap_or_else(|error| panic!("{source} should parse: {error}"));
        let first_text = emit_class(&first);
        let second = parse_class(&first_text)
            .unwrap_or_else(|error| panic!("emitted source should parse: {error}\n{first_text}"));
        let second_text = emit_class(&second);
        assert_eq!(
            first, second,
            "the document changed through a round trip of:\n{first_text}"
        );
        assert_eq!(
            first_text, second_text,
            "emitting twice changed the source:\n{first_text}\n---\n{second_text}"
        );
        first_text
    }

    /// Class diagrams draw with Zed's own font stack, unlike the state and ER
    /// diagrams, so the theme here is the one the editor really uses.
    fn assert_renders(source: &str) {
        let svg = render_to_svg(source, &MermaidTheme::default())
            .unwrap_or_else(|error| panic!("emitted source should render: {error}\n{source}"));
        assert!(
            !svg.contains("Syntax error in text"),
            "emitted source rendered an error diagram:\n{source}"
        );
    }

    const EXAMPLES: [&str; 14] = [
        "classDiagram\n    class Base {\n        +field: Type\n        +method()\n    }\n    class Derived {\n        +extra()\n    }\n    Base <|-- Derived",
        "classDiagram\n  A --> B",
        "classDiagram\n  direction LR\n  A --> B\n  B ..> C",
        "classDiagram\n  A \"1\" o-- \"0..*\" B : 所有する",
        "classDiagram\n  class 動物 {\n    +名前 string\n    +鳴く() void\n  }\n  class 犬\n  動物 <|-- 犬 : 継承",
        "classDiagram\n  class List~int~ {\n    +get(int) int\n  }\n  List --> 利用者",
        "classDiagram\n  <<interface>> Shape\n  class Shape\n  Shape <|.. Circle",
        "classDiagram\n  class A {\n    <<interface>>\n    +run()\n  }",
        "classDiagram\n  A : +foo()\n  A : -bar int",
        "classDiagram\n  namespace 図形 {\n    class 円\n    class 四角\n  }\n  円 --> 四角",
        "classDiagram\n  namespace 外 {\n    namespace 内 {\n      class A\n    }\n    note for A \"入れ子のメモ\"\n  }",
        "classDiagram\n  note \"浮いたメモ\"\n  class A\n  note for A \"A のメモ\"",
        "classDiagram\n  %% 先頭のメモ\n  class A\n\n  %% 末尾のメモ\n  classDef warn fill:#f96",
        "classDiagram\n  accTitle: 概要\n  accDescr {\n    説明の 1 行目\n    2 行目\n  }\n  class A[\"表示名\"]",
    ];

    /// The class diagram examples from Mermaid's own documentation, which is
    /// where the spellings a person actually writes come from: no space before
    /// a `:` or a `{`, generics in members, and statements at column zero.
    const UPSTREAM_EXAMPLES: [&str; 5] = [
        "classDiagram\n    Animal <|-- Duck\n    Animal <|-- Fish\n    Animal <|-- Zebra\n    Animal : +int age\n    Animal : +String gender\n    Animal: +isMammal()\n    Animal: +mate()\n    class Duck{\n        +String beakColor\n        +swim()\n        +quack()\n    }",
        "classDiagram\n    Customer \"1\" --> \"*\" Ticket\n    Student \"1\" --> \"1..*\" Course\n    Galaxy --> \"many\" Star : Contains",
        "classDiagram\nclass Square~Shape~{\n    int id\n    List~int~ position\n    setPoints(List~int~ points)\n    getPoints() List~int~\n}\n\nSquare : -List~string~ messages\nSquare : +setMessages(List~string~ messages)",
        "classDiagram\nnamespace BaseShapes {\n    class Triangle\n    class Rectangle {\n      double width\n      double height\n    }\n}",
        "classDiagram\n    note \"This is a general note\"\n    note for MyClass \"This is a note for a class\"\n    class MyClass{\n    }",
    ];

    #[test]
    fn examples_round_trip_and_render() {
        for source in EXAMPLES.iter().chain(&UPSTREAM_EXAMPLES) {
            let emitted = assert_stable(source);
            assert_renders(&emitted);
        }
    }

    #[test]
    fn the_insert_template_round_trips() {
        let template = "classDiagram\n    %% <|-- は継承、*-- は合成、o-- は集約\n    class Base {\n        +field: Type\n        +method()\n    }\n    class Derived {\n        +extra()\n    }\n    Base <|-- Derived";
        let document = parse_class(template).expect("the template should parse");
        assert_eq!(
            document.class_names(),
            vec!["Base".to_string(), "Derived".to_string()]
        );
        let emitted = assert_stable(template);
        assert_eq!(emitted, template, "the template should be written as it is");
        assert_renders(&emitted);
    }

    /// The trap the ER model fell into: counting every brace left the scanner
    /// believing it was inside a block, and everything below the first
    /// misjudged line stopped being preserved. A class diagram has braces in
    /// labels, in members and in generics, and only a `class … {` opens a
    /// block.
    #[test]
    fn a_brace_outside_a_member_block_does_not_swallow_the_lines_below_it() {
        let source = "classDiagram
  class A[\"表示 {名}\"]
  class B {
    +set(Map~String, {int}~ values)
    +f() {noop}
  }
  A --> B : {ラベル}
  %% 下のメモ
  classDef warn fill:#f96";
        let document = parse_class(source).expect("source should parse");
        assert_eq!(
            document
                .items
                .iter()
                .filter(
                    |item| matches!(item, ClassItem::Preserved(text) if !text.trim().is_empty())
                )
                .count(),
            2,
            "the comment and the classDef should both be preserved: {document:?}"
        );
        let emitted = assert_stable(source);
        for line in ["%% 下のメモ", "classDef warn fill:#f96", "+f() {noop}"] {
            assert!(
                emitted.contains(line),
                "`{line}` should survive:\n{emitted}"
            );
        }
    }

    #[test]
    fn preserved_lines_keep_their_position_after_a_round_trip() {
        let source = "classDiagram
  %% 先頭のメモ
  class A

  class B
  %% 末尾のメモ
  classDef warn fill:#f96";
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "classDiagram\n  %% 先頭のメモ\n    class A\n\n    class B\n  %% 末尾のメモ\n  classDef warn fill:#f96"
        );
        let position = |needle: &str| {
            emitted
                .lines()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("`{needle}` should be in:\n{emitted}"))
        };
        assert!(position("先頭のメモ") < position("class A"));
        assert!(position("class A") < position("class B"));
        assert!(position("class B") < position("末尾のメモ"));
        assert!(position("末尾のメモ") < position("classDef"));
    }

    /// A blank line at the end of the fence is trimmed off by the emitter, so
    /// keeping one as an item would take a line out of the diagram every time
    /// the editor was confirmed.
    #[test]
    fn a_trailing_blank_line_does_not_accumulate() {
        let document = parse_class("classDiagram\n  class A\n\n\n").expect("source should parse");
        assert_eq!(
            document.items,
            vec![ClassItem::Class(ClassDecl {
                reference: ClassRef::new("A"),
                label: None,
                css_class: None,
                members: None,
                comment: None,
            })]
        );
        assert_eq!(
            assert_stable("classDiagram\n  class A\n\n\n"),
            "classDiagram\n    class A"
        );
    }

    #[test]
    fn a_statement_is_one_item_even_when_it_repeats_a_class() {
        let source = "classDiagram\n  class A\n  A : +foo()\n  A : -bar int\n  class A";
        let document = parse_class(source).expect("source should parse");
        assert_eq!(document.items.len(), 4);
        assert_eq!(document.class_names(), vec!["A".to_string()]);
        assert_eq!(
            assert_stable(source),
            "classDiagram\n    class A\n    A : +foo()\n    A : -bar int\n    class A"
        );
    }

    /// The two spellings of a member say the same thing to Mermaid, so an
    /// emitter that picked one would rewrite every line of half the diagrams
    /// that are opened.
    #[test]
    fn both_member_spellings_come_back_the_way_they_were_written() {
        let source = "classDiagram\n  class A {\n    +foo()\n  }\n  A : -bar int";
        let document = parse_class(source).expect("source should parse");
        assert!(
            matches!(
                document.items.first(),
                Some(ClassItem::Class(declaration))
                    if declaration.members.as_deref() == Some(&["+foo()".to_string()])
            ),
            "{document:?}"
        );
        assert!(
            matches!(
                document.items.get(1),
                Some(ClassItem::Member { text, .. }) if text == "-bar int"
            ),
            "{document:?}"
        );
        assert_eq!(
            assert_stable(source),
            "classDiagram\n    class A {\n        +foo()\n    }\n    A : -bar int"
        );
    }

    /// `class A` and `class A { }` are different fences; Mermaid draws the
    /// second with an empty compartment.
    #[test]
    fn an_empty_member_block_is_not_the_same_as_no_block() {
        let without = parse_class("classDiagram\n  class A").expect("source should parse");
        let with = parse_class("classDiagram\n  class A {\n  }").expect("source should parse");
        assert_ne!(without.items, with.items);
        assert_eq!(
            assert_stable("classDiagram\n  class A"),
            "classDiagram\n    class A"
        );
        assert_eq!(
            assert_stable("classDiagram\n  class A {\n  }"),
            "classDiagram\n    class A {\n    }"
        );
    }

    #[test]
    fn a_comment_stays_inside_the_namespace_it_explains() {
        let source =
            "classDiagram\n  namespace N {\n    %% 中のメモ\n    class A\n  }\n  %% 外のメモ";
        let emitted = assert_stable(source);
        let position = |needle: &str| {
            emitted
                .lines()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("`{needle}` should be in:\n{emitted}"))
        };
        let close = emitted
            .lines()
            .position(|line| line.trim() == "}")
            .expect("the closing brace");
        assert!(
            position("中のメモ") < close,
            "the inner comment moved out:\n{emitted}"
        );
        assert!(
            close < position("外のメモ"),
            "the outer comment moved in:\n{emitted}"
        );
    }

    /// Deleting one item must not shift the fields of the ones beside it: the
    /// nested scopes are walked first so that items and their rows stay paired.
    #[test]
    fn removing_a_nested_item_leaves_its_neighbours_alone() {
        let source = "classDiagram
  namespace N {
    class A[\"あ\"]
    class B[\"い\"]
    class C[\"う\"]
  }";
        let mut document = parse_class(source).expect("source should parse");
        let Some(ClassItem::Namespace(namespace)) = document.items.first_mut() else {
            panic!("the namespace should be the first item: {document:?}");
        };
        namespace.items.remove(1);
        let labels = namespace
            .items
            .iter()
            .filter_map(|item| match item {
                ClassItem::Class(declaration) => declaration.label.clone(),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(labels, vec!["あ".to_string(), "う".to_string()]);
        assert_renders(&emit_class(&document));
    }

    #[test]
    fn a_namespace_may_nest_up_to_the_limit_and_no_further() {
        let nested = |depth: usize| {
            let mut source = String::from("classDiagram\n");
            for level in 0..depth {
                let _ = writeln!(source, "{}namespace n{level} {{", INDENT.repeat(level + 1));
            }
            let _ = writeln!(source, "{}class A", INDENT.repeat(depth + 1));
            for level in (0..depth).rev() {
                let _ = writeln!(source, "{}}}", INDENT.repeat(level + 1));
            }
            source
        };
        assert!(parse_class(&nested(MAX_NESTING_DEPTH)).is_ok());
        let error = parse_class(&nested(MAX_NESTING_DEPTH + 1))
            .err()
            .expect("one level past the limit should be refused");
        assert!(format!("{error}").contains("nested too deeply"), "{error}");
        // Deep enough to overflow the stack if the limit did not hold.
        assert!(parse_class(&nested(400)).is_err());
    }

    #[test]
    fn relations_keep_their_operator_cardinality_and_label() {
        let source = "classDiagram
  A <|-- B
  A --|> B
  A *-- B
  A --* B
  A o-- B
  A --o B
  A --> B
  A <-- B
  A -- B
  A ..> B
  A <.. B
  A ..|> B
  A <|.. B
  A .. B
  A ()-- B
  A --() B
  A <|--|> B
  A \"1\" --> \"0..*\" B : 持つ";
        let document = parse_class(source).expect("source should parse");
        let arrows = document
            .items
            .iter()
            .filter_map(|item| match item {
                ClassItem::Relation(relation) => Some(relation.arrow.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            arrows,
            vec![
                "<|--", "--|>", "*--", "--*", "o--", "--o", "-->", "<--", "--", "..>", "<..",
                "..|>", "<|..", "..", "()--", "--()", "<|--|>", "-->",
            ]
        );
        for (arrow, _) in RELATION_PRESETS {
            assert!(is_valid_relation_arrow(arrow), "`{arrow}` should be valid");
        }
        let Some(ClassItem::Relation(last)) = document.items.last() else {
            panic!("the last item is a relation: {document:?}");
        };
        assert_eq!(last.left_cardinality.as_deref(), Some("1"));
        assert_eq!(last.right_cardinality.as_deref(), Some("0..*"));
        assert_eq!(last.label.as_deref(), Some("持つ"));
        assert_renders(&assert_stable(source));
    }

    #[test]
    fn an_operator_is_only_one_where_mermaid_reads_one() {
        for arrow in [
            "--", "..", "-->", "<|--", "--|>", "o--", "*--", "()--", "<|--|>",
        ] {
            assert!(is_valid_relation_arrow(arrow), "`{arrow}` should be valid");
        }
        for arrow in ["", "-", "->", "--->", "|--", "x--", "-- ", " --", "--x"] {
            assert!(
                !is_valid_relation_arrow(arrow),
                "`{arrow}` should not be valid"
            );
        }
    }

    #[test]
    fn a_generic_travels_with_the_class_that_carries_it() {
        let source = "classDiagram\n  class Box~T~ {\n    +get() T\n  }\n  Box~T~ --> 利用者";
        let document = parse_class(source).expect("source should parse");
        assert_eq!(
            document.class_names(),
            vec!["Box".to_string(), "利用者".to_string()]
        );
        let Some(ClassItem::Class(declaration)) = document.items.first() else {
            panic!("{document:?}");
        };
        assert_eq!(declaration.reference.generic.as_deref(), Some("T"));
        assert_renders(&assert_stable(source));

        assert!(parse_class("classDiagram\n  class Box~T").is_err());
        assert!(parse_class("classDiagram\n  class Box~~").is_err());
        assert!(parse_class("classDiagram\n  class Box~List~T~~").is_err());
    }

    #[test]
    fn a_japanese_diagram_round_trips_through_every_construct() {
        let source = "classDiagram
  direction LR
  namespace 生き物 {
    class 動物[\"どうぶつ\"] {
      <<abstract>>
      +名前 string
      +鳴く() void
    }
    note for 動物 \"抽象クラスです\"
  }
  class 犬
  犬 : +吠える()
  <<具象>> 犬
  動物 \"1\" <|-- \"多\" 犬 : 継承する";
        let emitted = assert_stable(source);
        for line in [
            "class 動物[\"どうぶつ\"] {",
            "+鳴く() void",
            "note for 動物 \"抽象クラスです\"",
            "犬 : +吠える()",
            "<<具象>> 犬",
            "動物 \"1\" <|-- \"多\" 犬 : 継承する",
        ] {
            assert!(
                emitted.contains(line),
                "`{line}` should survive:\n{emitted}"
            );
        }
        assert_renders(&emitted);
    }

    #[test]
    fn statements_the_model_does_not_carry_survive_verbatim() {
        let source = "classDiagram\n  %% 見出し\n  classDef warn fill:#f96\n  accTitle: 概要\n  accDescr: 説明\n  class A\n  accDescr {\n    長い説明\n  }";
        let emitted = assert_stable(source);
        for line in [
            "%% 見出し",
            "classDef warn fill:#f96",
            "accTitle: 概要",
            "accDescr: 説明",
            "長い説明",
        ] {
            assert!(
                emitted.contains(line),
                "`{line}` should survive:\n{emitted}"
            );
        }
        assert_renders(&emitted);
    }

    #[test]
    fn constructs_that_would_be_dropped_are_refused() {
        for source in [
            // A statement that addresses a class in a spelling a rename cannot
            // follow, or where Mermaid does not take it.
            "classDiagram\n  class A\n  style `A` fill:#f96",
            "classDiagram\n  namespace N {\n    class A\n    style A fill:#f96\n  }",
            // `:::` is carried on a `class` statement and nowhere else, because
            // that is the only place Mermaid's grammar takes one.
            "classDiagram\n  class A\n  A:::warn",
            "classDiagram\n  class A\n  class B\n  A --> B:::warn",
            "classDiagram\n  class A:::\n  classDef warn fill:#f96",
            "classDiagram\n  class A:::1warn",
            "classDiagram\n  class A:::`warn`",
            "classDiagram\n  class A:::warn[\"表示名\"]",
            // Comments and braces Mermaid reads differently from a person.
            "classDiagram\n  class A {\n    +x()\n  } %% 行末のメモ",
            "classDiagram\n  namespace N {\n    class A\n  } %% 行末のメモ",
            // A `%%` right after a name is part of the name to Mermaid, and a
            // comment in the middle takes the rest of the statement with it.
            "classDiagram\n  class A%%メモ",
            "classDiagram\n  A --> B%%メモ",
            "classDiagram\n  A %% メモ --> B",
            // Mermaid reads a member line to its end, the `}` included.
            "classDiagram\n  class A { +x() }",
            "classDiagram\n  class A {\n    +x()",
            // A one line namespace holds one statement and closes on its line.
            "classDiagram\n  namespace N { class A\n  }",
            "classDiagram\n  namespace N { class A {\n    +x()\n  }\n  }",
            "classDiagram\n  namespace N {\n    class A",
            "classDiagram\n  }\n  class A",
            // Statements Mermaid does not take where they were written.
            "classDiagram\n  namespace N {\n    A --> B\n  }",
            "classDiagram\n  namespace N {\n    direction LR\n    class A\n  }",
            "classDiagram\n  namespace N {\n    classDef warn fill:#f96\n    class A\n  }",
            "classDiagram\n  direction TB\n  direction LR\n  class A",
            "classDiagram\n  direction LR 余分\n  class A",
            "classDiagram LR\n  class A",
            "classDiagram\n  class `Order Item`",
            "classDiagram\n  class 1A",
            "classDiagram\n  <<interface Shape",
            "classDiagram\n  note for A メモ",
            "classDiagram\n  class A\n  だけ",
        ] {
            let error = parse_class(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            let message = format!("{error}");
            assert_ne!(
                japanese_reason(source).as_deref(),
                Some(message.as_str()),
                "the message should be the editor's own sentence for `{source}`"
            );
        }
    }

    /// The reason `source` is refused, read with the Japanese catalog. A
    /// sentence the editor wrote goes through the catalog and so changes with
    /// the language, while an error `merman` raised would come back unchanged.
    fn japanese_reason(source: &str) -> Option<String> {
        let _japanese = crate::mermaid_model::japanese_for_this_thread();
        parse_class(source).err().map(|error| error.to_string())
    }

    /// `class A:::warn` names a style class rather than a class of the diagram,
    /// so nothing has to follow a rename into it — but dropping it would take
    /// the colour the `classDef` line gives the box.
    #[test]
    fn a_style_class_shorthand_survives_a_round_trip() {
        let source = "classDiagram\n  classDef warn fill:#f96\n  class A:::warn\n  class B[\"表示名\"]:::warn {\n    +x()\n  }\n  namespace N {\n    class 動物~T~:::warn\n  }\n  A --> B";
        let document = parse_class(source).expect("source should parse");
        let css = |item: &ClassItem| match item {
            ClassItem::Class(declaration) => declaration.css_class.clone(),
            _ => None,
        };
        assert_eq!(
            document.items.iter().filter_map(css).collect::<Vec<_>>(),
            vec!["warn".to_string(), "warn".to_string()]
        );
        let emitted = assert_stable(source);
        for line in [
            "class A:::warn",
            "class B[\"表示名\"]:::warn {",
            "class 動物~T~:::warn",
        ] {
            assert!(
                emitted.contains(line),
                "`{line}` should survive:\n{emitted}"
            );
        }
        assert_renders(&emitted);

        // The spellings Mermaid also takes come back in the one this emits.
        assert_eq!(
            assert_stable("classDiagram\n  class A :::warn"),
            "classDiagram\n    class A:::warn"
        );
        assert!(is_valid_css_class("warn"));
        assert!(is_valid_css_class("警告"));
        assert!(!is_valid_css_class(""));
        assert!(!is_valid_css_class("1warn"));
        assert!(!is_valid_css_class("a b"));
        // A word that opens a statement is a style class all the same: it never
        // stands at the start of one.
        assert!(is_valid_css_class("note"));
        assert!(!is_valid_class_name("note"));
    }

    /// A renamed class keeps its style class: `:::warn` names a `classDef`, not
    /// a class of the diagram.
    #[test]
    fn renaming_a_class_leaves_its_style_class_alone() {
        let mut document =
            parse_class("classDiagram\n  class A:::warn").expect("source should parse");
        let Some(ClassItem::Class(declaration)) = document.items.first_mut() else {
            panic!("{document:?}");
        };
        declaration.reference.name = "B".to_string();
        assert_eq!(emit_class(&document), "classDiagram\n    class B:::warn");
    }

    /// Mermaid closes a block at the `}` and reads the rest of that line on its
    /// own, while this emitter writes a bare `}`. Reading the line as a member
    /// instead would swallow every line below it, so the fence is refused with
    /// the reason rather than opened.
    #[test]
    fn a_closing_brace_with_something_after_it_says_so() {
        for source in [
            "classDiagram\n  class A {\n    +x()\n  } %% メモ\n  class B",
            "classDiagram\n  namespace N {\n    class A\n  } %% メモ",
        ] {
            let error = parse_class(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            assert!(
                format!("{error}").contains("alone on its line"),
                "`{source}`: {error}"
            );
        }
        // A `}` with nothing but whitespace after it is the closing brace it
        // looks like. `merman` 0.7 counted an ideographic space as whitespace
        // there; 0.8 does not, and refuses the line.
        assert_eq!(
            assert_stable("classDiagram\n  class A {\n    +x()\n  } \t\n  class B"),
            "classDiagram\n    class A {\n        +x()\n    }\n    class B"
        );
        let error = parse_class("classDiagram\n  class A {\n    +x()\n  }\u{3000}\n  class B")
            .expect_err("an ideographic space after the brace");
        assert!(
            format!("{error}").contains("Mermaid could not parse"),
            "{error}"
        );
    }

    /// A `%%` line inside a member block is part of the block, not a comment
    /// Mermaid strips, so the editor has to be able to write it back: a member
    /// validator that refused one would leave the diagram unconfirmable.
    #[test]
    fn a_comment_inside_a_member_block_is_carried_as_a_member() {
        let source = "classDiagram\n  class A {\n    +x()\n    %% 説明\n  }";
        let document = parse_class(source).expect("source should parse");
        let Some(ClassItem::Class(declaration)) = document.items.first() else {
            panic!("{document:?}");
        };
        assert_eq!(
            declaration.members.as_deref(),
            Some(&["+x()".to_string(), "%% 説明".to_string()][..])
        );
        assert!(is_valid_block_member_text("%% 説明"));
        assert!(is_valid_block_member_text("+x() %% 説明"));
        assert!(is_valid_block_member_text(""));
        assert!(!is_valid_block_member_text("}"));
        assert!(!is_valid_block_member_text(" 前後 "));
        // Mermaid reads a member statement to the end of its line, so the
        // `%%` is part of the member there too.
        assert!(is_valid_member_text("+x() %% 説明"));
        assert!(!is_valid_block_member_text("} 後"));
        assert_renders(&assert_stable(source));
    }

    /// A comment explains the statement it was written above, so taking that
    /// statement out has to leave the comment where the user put it rather than
    /// carrying it along or dropping it.
    #[test]
    fn a_comment_keeps_its_place_when_the_statement_below_it_goes() {
        let source =
            "classDiagram\n  %% A の説明\n  class A\n  %% B の説明\n  class B\n  %% 末尾のメモ";
        let mut document = parse_class(source).expect("source should parse");
        assert!(
            matches!(document.items.get(1), Some(ClassItem::Class(declaration)) if declaration.reference.name == "A"),
            "{document:?}"
        );
        document.items.remove(1);
        assert_eq!(
            emit_class(&document),
            "classDiagram\n  %% A の説明\n  %% B の説明\n    class B\n  %% 末尾のメモ",
            "each comment stays above the statement it was written above"
        );
    }

    /// Mermaid's lexer swallows `direction` and the whitespace after it before
    /// it looks for the direction word, so a line that opens with it and holds
    /// no direction is already broken — refusing it is what keeps the editor
    /// from writing back a line Mermaid reads differently.
    #[test]
    fn a_direction_statement_needs_one_of_the_four_words() {
        assert!(parse_class("classDiagram\n  direction --> A").is_err());
        assert!(parse_class("classDiagram\n  class direction").is_err());
        assert!(!is_valid_class_name("direction"));

        let document =
            parse_class("classDiagram\n  direction RL\n  A --> B").expect("source should parse");
        assert_eq!(document.direction, Some(FlowDirection::RightToLeft));
        assert!(emit_class(&document).contains("direction RL"));
        assert_eq!(
            assert_stable("classDiagram\n  A --> B\n  direction RL"),
            "classDiagram\n    direction RL\n    A --> B",
            "the direction should move under the diagram keyword and stay there"
        );
    }

    #[test]
    fn other_diagram_types_are_refused() {
        for source in [
            "flowchart TD\n  A --> B",
            "stateDiagram-v2\n  [*] --> A",
            "erDiagram\n  A ||--o{ B : x",
            "classDiagram-v2\n  class A",
        ] {
            assert!(!is_class_source(source), "{source}");
            assert!(parse_class(source).is_err(), "{source}");
        }
        assert!(is_class_source("classDiagram\n  class A"));
        assert!(is_class_source("%% メモ\nclassDiagram\n  class A"));
        assert!(is_class_source(
            "---\ntitle: x\n---\nclassDiagram\n  class A"
        ));
    }

    /// An empty fence has to open, and `merman` draws an empty class diagram,
    /// so there is nothing to refuse.
    #[test]
    fn an_empty_fence_opens_as_a_blank_diagram() {
        let document = parse_class("").expect("an empty fence should parse");
        assert_eq!(document, ClassDocument::empty());
        assert_eq!(emit_class(&document), CLASS_KEYWORD);
        // `merman` 0.8 no longer draws the keyword on its own (the editor asks
        // for a class before it writes the fence), but the editor still opens it.
        assert!(render_to_svg(CLASS_KEYWORD, &MermaidTheme::default()).is_err());
        assert_eq!(
            parse_class(CLASS_KEYWORD).expect("the keyword alone should open"),
            ClassDocument::empty()
        );
        assert_renders("classDiagram\n  %% メモ");
    }

    #[test]
    fn names_generics_and_texts_are_validated() {
        assert!(is_valid_class_name("A"));
        assert!(is_valid_class_name("Order_Item"));
        assert!(is_valid_class_name("動物"));
        assert!(is_valid_class_name("a.b"));
        assert!(is_valid_class_name("a-b"));
        assert!(!is_valid_class_name(""));
        assert!(!is_valid_class_name("a b"));
        assert!(!is_valid_class_name("a:b"));
        assert!(!is_valid_class_name("a~b"));
        assert!(!is_valid_class_name("a--b"));
        assert!(!is_valid_class_name("a..b"));
        assert!(!is_valid_class_name("-a"));
        assert!(!is_valid_class_name("a-"));
        assert!(!is_valid_class_name("1A"));
        assert!(!is_valid_class_name("class"));
        assert!(!is_valid_class_name("note"));
        assert!(!is_valid_class_name("_blank"));
        assert!(!is_valid_class_name("classDiagram"));

        assert!(is_valid_generic("T"));
        assert!(is_valid_generic("動物"));
        assert!(!is_valid_generic(""));
        assert!(!is_valid_generic("T~U"));
        assert!(!is_valid_generic("a b"));

        assert!(is_valid_quoted_text("1..*"));
        assert!(is_valid_quoted_text(""));
        assert!(!is_valid_quoted_text("引用 \" つき"));
        assert!(!is_valid_quoted_text("改行\nつき"));

        assert!(is_valid_annotation("interface"));
        assert!(is_valid_annotation("抽象"));
        assert!(!is_valid_annotation(""));
        assert!(!is_valid_annotation(" 前後 "));
        assert!(!is_valid_annotation("a>b"));

        assert!(is_valid_member_text("+foo() int"));
        assert!(is_valid_member_text("<<interface>>"));
        assert!(!is_valid_member_text("}"));
        assert!(!is_valid_member_text(" 前後 "));
        assert!(is_valid_member_text("+foo() %% メモ"));
        assert!(is_valid_member_text("+foo(\"%% \") int"));

        assert!(is_valid_relation_label("持つ"));
        assert!(is_valid_relation_label("}"));
        assert!(is_valid_relation_label("持つ %% メモ"));
        assert!(!is_valid_relation_label(" 前後 "));
    }

    /// A word that opens a statement of its own cannot also be a class name:
    /// the line would be read back as that statement, so the class the editor
    /// showed would be gone from the model on the next parse.
    #[test]
    fn a_word_that_opens_a_statement_is_not_a_valid_name() {
        for word in [
            "class",
            "namespace",
            "note",
            "style",
            "classDef",
            "cssClass",
            "click",
            "link",
            "callback",
            "href",
            "accTitle",
            "accDescr",
            "_blank",
            "direction",
            "A",
            "動物",
            "end",
        ] {
            let source = format!("classDiagram\n  {word} --> Z");
            let Ok(document) = parse_class(&source) else {
                assert!(
                    !is_valid_class_name(word),
                    "`{word} --> Z` is refused while is_valid_class_name says yes"
                );
                continue;
            };
            let carried = matches!(
                document.items.first(),
                Some(ClassItem::Relation(relation)) if relation.left.name == word
            );
            assert_eq!(
                carried,
                is_valid_class_name(word),
                "`{word} --> Z` read as {:?} while is_valid_class_name is {}",
                document.items,
                is_valid_class_name(word)
            );
        }
    }

    #[test]
    fn unused_name_skips_taken_names() {
        let source = "classDiagram\n  クラス --> クラス1\n  note for クラス2 \"メモ\"\n  namespace N {\n    class クラス3\n  }";
        let document = parse_class(source).expect("source should parse");
        assert_eq!(
            document.class_names(),
            vec![
                "クラス".to_string(),
                "クラス1".to_string(),
                "クラス2".to_string(),
                "クラス3".to_string()
            ],
            "every class the diagram mentions, however it mentions it"
        );
        assert_eq!(document.unused_name("クラス"), "クラス4");
        assert_eq!(document.unused_name("Z"), "Z");
    }

    #[test]
    fn crlf_and_trailing_whitespace_are_tolerated() {
        let source = "classDiagram\r\n  class A  \r\n  A --> B\r\n";
        let document = parse_class(source).expect("source should parse");
        assert_eq!(
            emit_class(&document),
            "classDiagram\n    class A\n    A --> B"
        );
    }

    #[test]
    fn preserved_lines_keep_their_side_of_the_diagram_keyword() {
        let source = "%%{init: {\"theme\": \"dark\"}}%%\nclassDiagram\n  class A";
        let document = parse_class(source).expect("source should parse");
        assert_eq!(
            document
                .preserved_lines
                .iter()
                .map(|line| (line.position, line.text.clone()))
                .collect::<Vec<_>>(),
            vec![(
                PreservedPosition::Preamble,
                "%%{init: {\"theme\": \"dark\"}}%%".to_string()
            )]
        );
        let emitted = assert_stable(source);
        assert!(
            emitted
                .lines()
                .next()
                .unwrap_or_default()
                .starts_with("%%{init"),
            "the init directive must stay before the diagram keyword:\n{emitted}"
        );
    }

    /// The editor writes the document back whatever the user typed, so a label
    /// or a note that would close its own quotes has to be caught before then.
    #[test]
    fn a_quote_in_a_label_or_a_note_is_refused_by_the_validators() {
        let document = ClassDocument {
            direction: None,
            items: vec![
                ClassItem::Class(ClassDecl {
                    reference: ClassRef::new("A"),
                    label: Some("引用 \" つき".into()),
                    css_class: None,
                    members: None,
                    comment: None,
                }),
                ClassItem::Note(ClassNote {
                    target: Some("A".into()),
                    text: "引用 \" つき".into(),
                    comment: None,
                }),
            ],
            preserved_lines: Vec::new(),
        };
        let emitted = emit_class(&document);
        assert!(parse_class(&emitted).is_err(), "{emitted}");
        for item in &document.items {
            let text = match item {
                ClassItem::Class(declaration) => declaration.label.clone(),
                ClassItem::Note(note) => Some(note.text.clone()),
                _ => None,
            };
            assert!(
                text.is_some_and(|text| !is_valid_quoted_text(&text)),
                "the validator has to catch what the emitter cannot escape"
            );
        }
    }

    /// A comment at the end of a statement is dropped by Mermaid, so the model
    /// carries it on the statement and writes it back on the same line — not
    /// just somewhere in the fence, which a check of `contains` would accept.
    #[test]
    fn a_comment_at_the_end_of_a_statement_stays_on_its_line() {
        let source = "classDiagram
    class A %% A の説明
    class B { %% B の説明
        +x()
    }
    A --> B %% 関係の説明
    <<interface>> A %% 注釈の説明
    note for A \"メモ %% 本文\" %% ノートの説明
    namespace N[\"名前\"] { %% 名前空間の説明
        class C[\"表示\"]%%直後
    }";
        let document = parse_class(source).expect("source should parse");
        let Some(ClassItem::Relation(relation)) = document.items.get(2) else {
            panic!("the third statement is the relation: {document:?}");
        };
        assert_eq!(relation.comment.as_deref(), Some("%% 関係の説明"));
        assert_eq!(relation.label, None);
        let Some(ClassItem::Note(note)) = document.items.get(4) else {
            panic!("the fifth statement is the note: {document:?}");
        };
        assert_eq!(note.text, "メモ %% 本文", "a `%%` in quotes is text");
        assert_eq!(note.comment.as_deref(), Some("%% ノートの説明"));

        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            source.replace("[\"表示\"]%%直後", "[\"表示\"] %%直後"),
            "every comment should come back at the end of its own line"
        );
        assert_renders(&emitted);
    }

    /// A label and a member statement run to the end of the line in Mermaid's
    /// lexer, so a `%%` there is text the picture shows, not a comment.
    #[test]
    fn a_percent_pair_in_a_label_or_a_member_statement_is_text() {
        let source = "classDiagram\n    A --> B : 持つ %% 注\n    A : +x() %% 注";
        let document = parse_class(source).expect("source should parse");
        assert!(
            matches!(
                document.items.first(),
                Some(ClassItem::Relation(relation))
                    if relation.label.as_deref() == Some("持つ %% 注") && relation.comment.is_none()
            ),
            "{document:?}"
        );
        assert!(
            matches!(
                document.items.get(1),
                Some(ClassItem::Member { text, .. }) if text == "+x() %% 注"
            ),
            "{document:?}"
        );
        let emitted = assert_stable(source);
        assert_eq!(emitted, source);
        assert_renders(&emitted);
    }

    /// Written after a label, a comment would become part of the label, so a
    /// relation that gains one moves its comment to the line above instead of
    /// either dropping it or changing what the picture says.
    #[test]
    fn a_comment_moves_above_a_relation_that_gains_a_label() {
        let mut document =
            parse_class("classDiagram\n    A --> B %% 説明").expect("source should parse");
        let Some(ClassItem::Relation(relation)) = document.items.first_mut() else {
            panic!("{document:?}");
        };
        relation.label = Some("持つ".to_string());
        let emitted = emit_class(&document);
        assert_eq!(emitted, "classDiagram\n    %% 説明\n    A --> B : 持つ");
        assert_eq!(assert_stable(&emitted), emitted);
        let reparsed = parse_class(&emitted).expect("the emitted source should parse");
        assert!(
            matches!(
                reparsed.items.get(1),
                Some(ClassItem::Relation(relation)) if relation.label.as_deref() == Some("持つ")
            ),
            "the label must not have taken the comment in: {reparsed:?}"
        );
    }

    /// Mermaid skips a blank line inside a member block, but the user put it
    /// there, so it comes back where it was.
    #[test]
    fn a_blank_line_inside_a_member_block_is_kept() {
        let source =
            "classDiagram\n    class A {\n        +x()\n\n        +y()\n\n    }\n    class B";
        let document = parse_class(source).expect("source should parse");
        let Some(ClassItem::Class(declaration)) = document.items.first() else {
            panic!("{document:?}");
        };
        assert_eq!(
            declaration.members,
            Some(vec![
                "+x()".to_string(),
                String::new(),
                "+y()".to_string(),
                String::new()
            ])
        );
        let emitted = assert_stable(source);
        assert_eq!(emitted, source);
        assert_renders(&emitted);
        // Whitespace on a blank line is the one thing the first emit settles.
        assert_eq!(
            assert_stable("classDiagram\n    class A {\n        +x()\n    \t\n    }"),
            "classDiagram\n    class A {\n        +x()\n\n    }"
        );
    }

    /// Mermaid's grammar takes a namespace whose one statement is on the line
    /// of its braces. It is written back over several lines, which Mermaid
    /// reads as the same namespace.
    #[test]
    fn a_namespace_on_one_line_opens_and_comes_back_on_several() {
        for (source, expected) in [
            (
                "classDiagram\n  namespace N { class A }\n  A --> B",
                "classDiagram\n    namespace N {\n        class A\n    }\n    A --> B",
            ),
            (
                "classDiagram\n  namespace N[\"名前\"] { note \"メモ\" } %% 説明",
                "classDiagram\n    namespace N[\"名前\"] { %% 説明\n        note \"メモ\"\n    }",
            ),
            (
                "classDiagram\n  classDef warn fill:#f96\n  namespace 外 { namespace 内 { class A[\"あ\"]:::warn } }",
                "classDiagram\n  classDef warn fill:#f96\n    namespace 外 {\n        namespace 内 {\n            class A[\"あ\"]:::warn\n        }\n    }",
            ),
        ] {
            let emitted = assert_stable(source);
            assert_eq!(emitted, expected, "for {source}");
            assert_renders(source);
            assert_renders(&emitted);
        }
        // An empty namespace still opens and comes back, but `merman` 0.8
        // fails to draw it (the editor asks for a class inside first).
        let source = "classDiagram\n  namespace N {}";
        assert_eq!(
            assert_stable(source),
            "classDiagram\n    namespace N {\n    }"
        );
        assert!(render_to_svg(source, &MermaidTheme::default()).is_err());
    }

    /// A member block on one line is not one Mermaid reads: its lexer takes a
    /// member to the end of the line, the closing brace included. The fence is
    /// refused by `merman` itself, before the model looks at it.
    #[test]
    fn a_member_block_on_one_line_is_refused_by_mermaid_itself() {
        let source = "classDiagram\n  class A { +int x }";
        let error = parse_class(source)
            .err()
            .expect("a one line member block should be refused");
        assert!(
            format!("{error}").contains("Mermaid could not parse"),
            "{error}"
        );
    }

    /// The statements that address a class by name are kept verbatim, and the
    /// editor finds the names in them to carry a rename or a deletion into.
    #[test]
    fn statements_that_address_a_class_are_kept_and_their_targets_found() {
        let source = "classDiagram
    class A
    class B~T~
    style A fill:#f96,stroke:#333 %% 色
    cssClass \"A, B~T~,A\" warn
    click A href \"https://example.com\" \"説明\" _blank
    click B call handler(\"x\")
    link A \"https://example.com\"
    callback B \"handler\"
    classDef warn fill:#f96";
        let document = parse_class(source).expect("source should parse");
        let preserved = document
            .items
            .iter()
            .filter_map(|item| match item {
                ClassItem::Preserved(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(preserved.len(), 7, "{document:?}");
        assert_eq!(assert_stable(source), source);
        assert_renders(source);

        let targets = |line: &str| {
            statement_targets(line)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|range| line.get(range).map(str::to_string))
                .collect::<Vec<_>>()
        };
        assert_eq!(targets("    style A fill:#f96"), vec!["A"]);
        assert_eq!(targets("cssClass \"A, B~T~,A\" warn"), vec!["A", "B", "A"]);
        assert_eq!(targets("click 動物 call f()"), vec!["動物"]);
        assert_eq!(targets("link A~T~ \"u\""), vec!["A"]);
        assert_eq!(statement_targets("classDef A fill:#f96"), None);
        assert_eq!(statement_targets("style `A` fill:#f96"), None);
        assert_eq!(statement_targets("styles A"), None);

        let renamed = |name: &str| (name == "A").then(|| "動物".to_string());
        assert_eq!(
            rename_statement_targets("    style A fill:#f96 %% A", renamed),
            "    style 動物 fill:#f96 %% A"
        );
        assert_eq!(
            rename_statement_targets("    cssClass \"A, B~T~,A\" warn", renamed),
            "    cssClass \"動物, B~T~,動物\" warn"
        );
        assert_eq!(
            rename_statement_targets("    link AB \"A\"", renamed),
            "    link AB \"A\"",
            "only a whole name is renamed"
        );
        assert_eq!(
            rename_statement_targets("    classDef A fill:#f96", renamed),
            "    classDef A fill:#f96"
        );

        let without_a = |name: &str| name != "A";
        assert_eq!(
            retain_statement_targets("    style A fill:#f96", without_a),
            None
        );
        assert_eq!(
            retain_statement_targets("    style B fill:#f96", without_a).as_deref(),
            Some("    style B fill:#f96")
        );
        assert_eq!(
            retain_statement_targets("    cssClass \"A, B~T~,A\" warn", without_a).as_deref(),
            Some("    cssClass \"B~T~\" warn")
        );
        assert_eq!(
            retain_statement_targets("    cssClass \" A,B \" warn", without_a).as_deref(),
            Some("    cssClass \"B \" warn")
        );
        assert_eq!(
            retain_statement_targets("    cssClass \"A,A\" warn", without_a),
            None
        );
        assert_eq!(
            retain_statement_targets("    classDef A fill:#f96", without_a).as_deref(),
            Some("    classDef A fill:#f96")
        );
    }

    #[test]
    fn statements_merman_reads_around_a_directive_are_refused() {
        crate::mermaid_model::preprocessing_cases::assert_directive_rules(
            "classDiagram",
            "A <|-- B",
            "A %%{init: {}}%% <|-- B",
        );
    }

    #[test]
    fn text_that_merman_rewrites_before_parsing_cannot_be_written() {
        for text in ["a %%{init: {}}%% b", "%%{"] {
            assert!(!is_valid_quoted_text(text), "{text}");
            assert!(!is_valid_relation_label(text), "{text}");
            assert!(!is_valid_member_text(text), "{text}");
            assert!(!is_valid_block_member_text(text), "{text}");
        }
        assert!(is_valid_relation_label("100%"));

        let mut document =
            parse_class("classDiagram\n    A --> B").expect("the diagram should open");
        let ClassItem::Relation(relation) = &mut document.items[0] else {
            panic!("expected a relation: {:?}", document.items);
        };
        // Each is a cardinality on its own; side by side on one line with no
        // `>` between them, the `="` rewrite would close the first and open
        // the second.
        relation.arrow = "--".to_string();
        relation.left_cardinality = Some("1<b=".to_string());
        relation.right_cardinality = Some("c>".to_string());
        let whole = crate::mermaid_model::MermaidDocument::Class(document.clone());
        assert!(whole.structure_error().is_some(), "{}", whole.emit());
        assert!(
            crate::render_to_svg(&whole.emit(), &crate::MermaidTheme::default()).is_err(),
            "the reason for the refusal no longer holds:
{}",
            whole.emit()
        );

        let ClassItem::Relation(relation) = &mut document.items[0] else {
            panic!("expected a relation");
        };
        relation.left_cardinality = Some("1".to_string());
        relation.right_cardinality = Some("*".to_string());
        relation.label = Some("<i title=\"t\">x</i>".to_string());
        let whole = crate::mermaid_model::MermaidDocument::Class(document);
        assert_eq!(whole.structure_error(), None);
    }
}
