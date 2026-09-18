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
//! keyword), and the whitespace inside a relation statement.

use std::fmt::Write as _;

use anyhow::{Result, anyhow, bail};
use merman::diagram::RenderSemanticModel;
use merman::{Engine, ParseOptions};

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

/// The words that open a statement which addresses classes in a spelling the
/// model cannot follow a rename or a deletion into. Opening the diagram would
/// mean either dropping the statement or leaving it pointing at a class that no
/// longer exists, which Mermaid would draw as an extra, empty box — so the
/// diagram is refused instead, with the reason shown to the user.
const UNSUPPORTED_STATEMENT_KEYWORDS: [&str; 6] =
    ["style", "cssClass", "click", "link", "callback", "href"];

/// The relation operators the editor's menu offers, with the words it shows for
/// them. An operator the list does not hold still survives, because
/// [`ClassRelation::arrow`] keeps whatever the fence spelled.
pub const RELATION_PRESETS: [(&str, &str); 14] = [
    ("<|--", "継承 (左が親)"),
    ("--|>", "継承 (右が親)"),
    ("*--", "合成 (左が全体)"),
    ("--*", "合成 (右が全体)"),
    ("o--", "集約 (左が全体)"),
    ("--o", "集約 (右が全体)"),
    ("-->", "関連 (右向き)"),
    ("<--", "関連 (左向き)"),
    ("--", "関連 (線のみ)"),
    ("..>", "依存 (右向き)"),
    ("<..", "依存 (左向き)"),
    ("..|>", "実現 (右が抽象)"),
    ("<|..", "実現 (左が抽象)"),
    ("..", "破線 (線のみ)"),
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
    /// would have to put them back in a spelling of its own choosing.
    pub members: Option<Vec<String>>,
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
    pub label: Option<String>,
}

/// One `note "…"` or `note for A "…"` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassNote {
    /// `None` for a floating note that is attached to no class.
    pub target: Option<String>,
    pub text: String,
}

/// One `namespace N { … }` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassNamespace {
    pub name: String,
    pub label: Option<String>,
    /// Mermaid only takes class declarations, notes and further namespaces
    /// inside a namespace, so a relation written in one makes the parse fail.
    pub items: Vec<ClassItem>,
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
    },
    Relation(ClassRelation),
    Note(ClassNote),
    Namespace(ClassNamespace),
    /// A line the model does not carry — a blank line, a `%%` comment, a
    /// `classDef` or an accessibility statement. Written back verbatim, in
    /// place, because a comment that moved out of the namespace it explains
    /// would be as bad as a comment that vanished.
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
    if !source.trim().is_empty() {
        let engine = Engine::new();
        let parsed = engine
            .parse_diagram_for_render_model_sync(source, ParseOptions::strict())
            .map_err(|error| {
                anyhow!("Mermaid の構文として読めませんでした ({error})。テキストのまま編集してください。")
            })?
            .ok_or_else(|| anyhow!("Mermaid の図として解釈できませんでした。"))?;
        if !matches!(parsed.model, RenderSemanticModel::Class(_)) {
            bail!("この図はクラス図ではありません。");
        }
    }

    let lines = source.lines().collect::<Vec<_>>();
    let preamble = scan_preamble(&lines);

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
        bail!("クラス図ではありません (`classDiagram` で始まる図だけを構造化編集できます)。");
    };
    if !rest.is_empty() {
        bail!(
            "`{CLASS_KEYWORD}` の行に `{rest}` が続いています。次の行に分けてからもう一度開いてください。"
        );
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
            bail!("`namespace` の入れ子が深すぎます。テキストのまま編集してください。");
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
                    bail!("対応する `{{` のない `}}` があります。");
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
            // Mermaid's lexer eats a comment wherever it finds one, so a
            // statement with one at its end would come back without it.
            if outside_quotes(trimmed).contains("%%") {
                bail!(
                    "文の末尾のコメントは構造化編集で失われます ({trimmed})。コメントを独立した行に移すか、テキストのまま編集してください。"
                );
            }

            if let Some(direction) = self.take_direction(trimmed, in_namespace)? {
                self.direction = Some(direction);
                continue;
            }
            if let Some(item) = self.take_preserved_statement(trimmed, start, in_namespace)? {
                items.push(item);
                continue;
            }
            for keyword in UNSUPPORTED_STATEMENT_KEYWORDS {
                if word_tail(trimmed, keyword).is_some() {
                    bail!(
                        "`{keyword}` の文は構造化エディターに対応していません ({trimmed})。テキストのまま編集してください。"
                    );
                }
            }
            if let Some(rest) = word_tail(trimmed, "namespace") {
                items.push(self.parse_namespace(rest, trimmed, depth)?);
                continue;
            }
            if let Some(rest) = word_tail(trimmed, "class") {
                items.push(ClassItem::Class(self.parse_class_statement(rest, trimmed)?));
                continue;
            }
            if let Some(rest) = word_tail(trimmed, "note") {
                items.push(ClassItem::Note(parse_note(rest, trimmed)?));
                continue;
            }

            if in_namespace {
                bail!(
                    "`namespace` の中に書けるのはクラス宣言・ノート・入れ子の `namespace` だけです ({trimmed})。"
                );
            }
            items.push(parse_plain_statement(trimmed)?);
        }

        if in_namespace {
            bail!("`namespace` の `{{` に対応する `}}` がありません。");
        }
        Ok(items)
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
            bail!(
                "`direction` のあとには TB / BT / LR / RL のどれかを書いてください ({statement})。"
            );
        };
        let after = rest
            .get(direction.keyword().len()..)
            .unwrap_or_default()
            .trim();
        if !after.is_empty() {
            // Mermaid reads the rest of the line and throws it away.
            bail!(
                "`direction {}` のあとの `{after}` は Mermaid が読み飛ばします ({statement})。消してからもう一度開いてください。",
                direction.keyword()
            );
        }
        if in_namespace {
            bail!("`direction` は `namespace` の中には書けません ({statement})。");
        }
        if self.direction.is_some() {
            bail!(
                "`direction` が 2 回書かれています。1 つにまとめてからもう一度開いてください ({statement})。"
            );
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
            bail!("`{keyword}` は `namespace` の中には書けません ({statement})。");
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

    fn parse_namespace(&mut self, rest: &str, statement: &str, depth: usize) -> Result<ClassItem> {
        let (name, rest) = take_name(rest, statement)?;
        if !is_valid_class_name(&name) {
            bail!("`{name}` は `namespace` の名前に使えません ({statement})。");
        }
        let (label, rest) = take_label(rest, statement)?;
        let rest = rest.trim();
        let Some(after_brace) = rest.strip_prefix('{') else {
            bail!("`namespace {name} {{` の形で書いてください ({statement})。");
        };
        if !after_brace.trim().is_empty() {
            bail!(
                "`{{` のあとは改行してください ({statement})。1 行で書いた `namespace` は構造化編集に対応していません。"
            );
        }
        Ok(ClassItem::Namespace(ClassNamespace {
            name,
            label,
            items: self.parse_scope(depth + 1, true)?,
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
                    bail!("`class` の文のあとの `{rest}` が読めません ({statement})。");
                }
                None
            }
            Some(after_brace) => {
                if !after_brace.trim().is_empty() {
                    bail!(
                        "`{{` のあとは改行してください ({statement})。1 行で書いたメンバーブロックは Mermaid も読めません。"
                    );
                }
                Some(self.parse_members(statement)?)
            }
        };
        Ok(ClassDecl {
            reference,
            label,
            css_class,
            members,
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
                bail!("`class` のメンバーブロックに対応する `}}` がありません ({statement})。");
            };
            self.index += 1;
            let trimmed = line.trim();
            if trimmed == "}" {
                return Ok(members);
            }
            refuse_trailing_brace(trimmed)?;
            // Mermaid drops a blank line inside the block, so keeping one would
            // add a line to the fence that the picture never had.
            if trimmed.is_empty() {
                continue;
            }
            members.push(trimmed.to_string());
        }
    }
}

/// A relation, a member statement or an annotation statement.
fn parse_plain_statement(statement: &str) -> Result<ClassItem> {
    if let Some(rest) = statement.strip_prefix("<<") {
        let Some((annotation, after)) = rest.split_once(">>") else {
            bail!("`<<注釈>>` が閉じていません ({statement})。");
        };
        let annotation = annotation.trim();
        if !is_valid_annotation(annotation) {
            bail!("`{annotation}` は注釈に使えません ({statement})。");
        }
        let (reference, after) = take_reference(after.trim_start(), statement)?;
        if !after.trim().is_empty() {
            bail!(
                "`<<注釈>> クラス名` のあとの `{}` が読めません ({statement})。",
                after.trim()
            );
        }
        return Ok(ClassItem::Annotation {
            reference,
            annotation: annotation.to_string(),
        });
    }

    let (reference, rest) = take_reference(statement, statement)?;
    let rest = rest.trim_start();

    if rest.starts_with(":::") {
        bail!(
            "`:::` でスタイルクラスを付けた図は構造化エディターに対応していません ({statement})。テキストのまま編集してください。"
        );
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
        bail!(
            "`{statement}` が読めません。クラス図の文は `class A`・`A : メンバー`・`A --> B`・`<<注釈>> A`・`note for A \"…\"` のどれかです。"
        );
    };
    let (right_cardinality, rest) = take_cardinality(rest.trim_start(), statement)?;
    let (right, rest) = take_reference(rest.trim_start(), statement)?;
    let rest = rest.trim();
    let label = match rest.strip_prefix(':') {
        Some(_) if rest.starts_with(":::") => bail!(
            "`:::` でスタイルクラスを付けた図は構造化エディターに対応していません ({statement})。テキストのまま編集してください。"
        ),
        Some(label) => Some(label.trim().to_string()),
        None if rest.is_empty() => None,
        None => bail!("関係の文のあとの `{rest}` が読めません ({statement})。"),
    };

    Ok(ClassItem::Relation(ClassRelation {
        left: reference,
        left_cardinality,
        arrow: arrow.to_string(),
        right_cardinality,
        right,
        label,
    }))
}

fn parse_note(rest: &str, statement: &str) -> Result<ClassNote> {
    let (target, rest) = match word_tail(rest, "for") {
        Some(after_for) => {
            let (name, rest) = take_name(after_for, statement)?;
            if !is_valid_class_name(&name) {
                bail!("`{name}` はクラス名に使えません ({statement})。");
            }
            (Some(name), rest)
        }
        None => (None, rest),
    };
    let rest = rest.trim_start();
    let Some(quoted) = rest.strip_prefix('"') else {
        bail!("`note \"本文\"` の形で書いてください ({statement})。");
    };
    let Some((text, after)) = quoted.split_once('"') else {
        bail!("ノート本文の引用符が閉じていません ({statement})。");
    };
    if !after.trim().is_empty() {
        bail!(
            "ノートのあとの `{}` が読めません ({statement})。",
            after.trim()
        );
    }
    Ok(ClassNote {
        target,
        text: text.to_string(),
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
        bail!(
            "`` ` `` で囲んだ名前は構造化エディターに対応していません ({statement})。テキストのまま編集してください。"
        );
    }
    let length = name_length(text);
    let Some(name) = text.get(..length).filter(|name| !name.is_empty()) else {
        bail!("クラス名が読めません ({statement})。");
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
                bail!("総称型は `{name}~T~` の形で書いてください ({statement})。");
            };
            if !is_valid_generic(generic) {
                bail!("`{generic}` は総称型に使えません ({statement})。");
            }
            ClassRef {
                name: name.to_string(),
                generic: Some(generic.to_string()),
            }
        }
    };
    if !is_valid_class_name(&reference.name) {
        bail!(
            "`{}` はクラス名に使えません ({statement})。空白・記号・Mermaid の予約語は使えません。",
            reference.name
        );
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
        bail!("`[\"表示名\"]` の形で書いてください ({statement})。");
    };
    let Some((label, after)) = quoted.split_once('"') else {
        bail!("表示名の引用符が閉じていません ({statement})。");
    };
    let Some(rest) = after.trim_start().strip_prefix(']') else {
        bail!("`[\"表示名\"]` の `]` がありません ({statement})。");
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
        bail!(
            "`` ` `` で囲んだスタイルクラス名は構造化エディターに対応していません ({statement})。テキストのまま編集してください。"
        );
    }
    let length = name_length(after_marker);
    let Some(name) = after_marker.get(..length).filter(|name| !name.is_empty()) else {
        bail!("`:::` のあとにスタイルクラス名がありません ({statement})。");
    };
    if !is_valid_css_class(name) {
        bail!("`{name}` は `:::` のスタイルクラス名に使えません ({statement})。");
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
        bail!(
            "`}}` の行に `{}` が続いています ({trimmed})。`}}` だけの行にしてからもう一度開いてください。",
            rest.trim()
        );
    }
    Ok(())
}

/// Splits a `"1..*"` cardinality off the start of `text`, if there is one.
fn take_cardinality<'a>(text: &'a str, statement: &str) -> Result<(Option<String>, &'a str)> {
    let Some(quoted) = text.strip_prefix('"') else {
        return Ok((None, text));
    };
    let Some((cardinality, rest)) = quoted.split_once('"') else {
        bail!("多重度の引用符が閉じていません ({statement})。");
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

/// `statement` with every quoted string blanked out, so that a `%%` the user
/// wrote in a label is not mistaken for a comment.
fn outside_quotes(statement: &str) -> String {
    let mut out = String::with_capacity(statement.len());
    let mut quoted = false;
    for character in statement.chars() {
        if character == '"' {
            quoted = !quoted;
            out.push(' ');
            continue;
        }
        out.push(if quoted { ' ' } else { character });
    }
    out
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
            } => {
                let _ = writeln!(out, "{indent}<<{annotation}>> {}", reference.text());
            }
            ClassItem::Relation(relation) => {
                let _ = writeln!(out, "{indent}{}", relation_statement(relation));
            }
            ClassItem::Note(note) => match &note.target {
                Some(target) => {
                    let _ = writeln!(out, "{indent}note for {target} \"{}\"", note.text);
                }
                None => {
                    let _ = writeln!(out, "{indent}note \"{}\"", note.text);
                }
            },
            ClassItem::Namespace(namespace) => {
                let label = match &namespace.label {
                    Some(label) => format!("[\"{label}\"]"),
                    None => String::new(),
                };
                let _ = writeln!(out, "{indent}namespace {}{label} {{", namespace.name);
                emit_items(&namespace.items, depth + 1, out);
                let _ = writeln!(out, "{indent}}}");
            }
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
        let _ = writeln!(out, "{head}");
        return;
    };
    let _ = writeln!(out, "{head} {{");
    for member in members {
        let _ = writeln!(out, "{indent}{INDENT}{member}");
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
    !text.contains('"') && !text.contains('\n') && !text.contains('\r')
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
/// The label runs to the end of its line and comes back trimmed, and Mermaid's
/// lexer would eat a `%%` and everything after it.
pub fn is_valid_relation_label(text: &str) -> bool {
    !text.contains('\n')
        && !text.contains('\r')
        && text.trim() == text
        && !outside_quotes(text).contains("%%")
}

/// Whether `text` can be written as the member of an `A : メンバー` statement.
///
/// The same as a relation label, except that a line which is nothing but `}`
/// would read as the end of a block. A `%%` is out because this parser refuses
/// to reopen a statement that holds one, so writing it would leave a fence the
/// editor could no longer open.
pub fn is_valid_member_text(text: &str) -> bool {
    is_valid_relation_label(text) && text != "}"
}

/// Whether `text` can be written as one line of a `class A { … }` block.
///
/// A block keeps its lines verbatim and Mermaid reads each one whole, so a `%%`
/// inside one belongs to the member rather than opening a comment — which is
/// why a comment written inside a block has to stay valid here, or a diagram
/// that holds one could be opened and never written back. Only a line that is
/// nothing but `}` would close the block early, and a blank one would be
/// dropped the next time the fence was read.
pub fn is_valid_block_member_text(text: &str) -> bool {
    !text.is_empty()
        && !text.contains('\n')
        && !text.contains('\r')
        && text.trim() == text
        && text != "}"
}

/// Whether `arrow` is a relation operator Mermaid reads as one.
pub fn is_valid_relation_arrow(arrow: &str) -> bool {
    split_arrow(arrow).is_some_and(|(taken, rest)| taken.len() == arrow.len() && rest.is_empty())
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
        assert!(format!("{error}").contains("入れ子"), "{error}");
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
            // A statement that addresses classes in a spelling a rename cannot
            // follow.
            "classDiagram\n  class A\n  style A fill:#f96",
            "classDiagram\n  class A\n  class B\n  cssClass \"A,B\" warn",
            "classDiagram\n  class A\n  click A href \"https://example.com\"",
            "classDiagram\n  class A\n  link A \"https://example.com\"",
            "classDiagram\n  class A\n  callback A \"fn\"",
            // `:::` is carried on a `class` statement and nowhere else, because
            // that is the only place Mermaid's grammar takes one.
            "classDiagram\n  class A\n  A:::warn",
            "classDiagram\n  class A\n  class B\n  A --> B:::warn",
            "classDiagram\n  class A:::\n  classDef warn fill:#f96",
            "classDiagram\n  class A:::1warn",
            "classDiagram\n  class A:::`warn`",
            "classDiagram\n  class A:::warn[\"表示名\"]",
            // Comments and braces Mermaid reads differently from a person.
            "classDiagram\n  class A %% 行末のメモ",
            "classDiagram\n  class A {\n    +x()\n  } %% 行末のメモ",
            "classDiagram\n  namespace N {\n    class A\n  } %% 行末のメモ",
            "classDiagram\n  A : +foo() %% 行末のメモ",
            "classDiagram\n  A --> B : ラベル %% 行末のメモ",
            "classDiagram\n  class A { +x() }",
            "classDiagram\n  class A {\n    +x()",
            "classDiagram\n  namespace N { class A }",
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
            assert!(
                message.chars().any(|character| character > '\u{7f}'),
                "the message should be Japanese for `{source}`: {message}"
            );
        }
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
                format!("{error}").contains("だけの行"),
                "`{source}`: {error}"
            );
        }
        // A `}` with nothing but whitespace after it — an ideographic space
        // included — is the closing brace it looks like.
        assert_eq!(
            assert_stable("classDiagram\n  class A {\n    +x()\n  }\u{3000}\n  class B"),
            "classDiagram\n    class A {\n        +x()\n    }\n    class B"
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
        assert!(!is_valid_block_member_text(""));
        assert!(!is_valid_block_member_text("}"));
        assert!(!is_valid_block_member_text(" 前後 "));
        // The statement spelling is another matter: this parser refuses to
        // reopen `A : +x() %% 説明`, so the editor must not write one.
        assert!(!is_valid_member_text("+x() %% 説明"));
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
        assert_renders(CLASS_KEYWORD);
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
        assert!(!is_valid_member_text("+foo() %% メモ"));
        assert!(is_valid_member_text("+foo(\"%% \") int"));

        assert!(is_valid_relation_label("持つ"));
        assert!(is_valid_relation_label("}"));
        assert!(!is_valid_relation_label("持つ %% メモ"));
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
                }),
                ClassItem::Note(ClassNote {
                    target: Some("A".into()),
                    text: "引用 \" つき".into(),
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
}
