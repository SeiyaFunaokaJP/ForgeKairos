//! A round-trippable editing model for Mermaid state diagrams.
//!
//! The shape of this module mirrors [`crate::flowchart_model`]: [`parse_state`]
//! reads the body of a ```` ```mermaid ```` fence into [`StateDocument`], and
//! [`emit_state`] writes it back. See [`crate::mermaid_model`] for the
//! invariants every diagram kind keeps.
//!
//! Unlike the flowchart model, this one does not read `merman`'s render model.
//! That model is built for layout: `[*]` has become a generated `root_start`
//! node, a note has become a node of its own, the `--` of a concurrent region
//! has become a `divider` node, and states and transitions live in separate
//! lists. A state diagram's meaning depends on the order its statements are
//! written in and on how they nest, and neither survives that translation. So
//! the source is scanned here, and `merman` is asked only whether the fence is a
//! state diagram it can draw at all — which also means every construct this
//! scanner does not recognize makes [`parse_state`] fail with a reason, rather
//! than being dropped.
//!
//! A statement, not a state, is the unit of the model: `A : one` followed by
//! `A : two` is two [`StateItem::State`] entries, because that is two lines of
//! the user's text. The editor shows what the fence says.
//!
//! What the parser rewrites, all of it stable from the first emit: indentation,
//! the case of a keyword, and the choice between the two spellings of a
//! description (`s1 : text` and `state "text" as s1`).

use std::fmt::Write as _;

use anyhow::{Result, anyhow, bail};
use merman::diagram::RenderSemanticModel;
use merman::{Engine, ParseOptions};

use crate::flowchart_model::{FlowDirection, PreservedLine, PreservedPosition};
use crate::mermaid_model::{
    INDENT, first_statement_line, join_lines, keyword_tail, scan_preamble, statement_keyword,
};

/// The endpoint that stands for where the diagram starts and where it ends.
pub const TERMINAL_STATE: &str = "[*]";

/// How deep a composite state may nest before the parser gives up. Mermaid's
/// own parser has a comparable limit; this one keeps a pathological fence from
/// recursing without bound.
const MAX_NESTING_DEPTH: usize = 16;

/// The words that open a statement the model does not carry, and that is
/// therefore held verbatim as a [`StateItem::Preserved`].
const PRESERVED_STATEMENT_KEYWORDS: [&str; 8] = [
    "classDef", "class", "style", "click", "accTitle", "accDescr", "hide", "scale",
];

/// The diagram keyword the source opened with. `stateDiagram` selects Mermaid's
/// old renderer and `stateDiagram-v2` the current one, so which one the user
/// typed has to survive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateKeyword {
    StateDiagram,
    StateDiagramV2,
}

impl StateKeyword {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StateDiagram => "stateDiagram",
            Self::StateDiagramV2 => "stateDiagram-v2",
        }
    }
}

/// What a state is drawn as. The three special kinds are spelled with a marker
/// (`state X <<choice>>`) and carry no description of their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateKind {
    Normal,
    Choice,
    Fork,
    Join,
}

impl StateKind {
    pub const ALL: [StateKind; 4] = [
        StateKind::Normal,
        StateKind::Choice,
        StateKind::Fork,
        StateKind::Join,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "通常の状態",
            Self::Choice => "分岐 <<choice>>",
            Self::Fork => "分流 <<fork>>",
            Self::Join => "合流 <<join>>",
        }
    }

    fn marker(self) -> Option<&'static str> {
        match self {
            Self::Normal => None,
            Self::Choice => Some("choice"),
            Self::Fork => Some("fork"),
            Self::Join => Some("join"),
        }
    }
}

/// Which side of its state a note is drawn on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotePlacement {
    LeftOf,
    RightOf,
}

impl NotePlacement {
    pub const ALL: [NotePlacement; 2] = [NotePlacement::LeftOf, NotePlacement::RightOf];

    pub fn label(self) -> &'static str {
        match self {
            Self::LeftOf => "左",
            Self::RightOf => "右",
        }
    }

    fn keyword(self) -> &'static str {
        match self {
            Self::LeftOf => "left of",
            Self::RightOf => "right of",
        }
    }
}

/// One state declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateEntry {
    pub id: String,
    pub description: Option<String>,
    pub kind: StateKind,
    /// The class applied with `id:::class`.
    pub style_class: Option<String>,
}

/// One transition. [`TERMINAL_STATE`] on the left is where the diagram starts,
/// and on the right where it ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateTransition {
    pub from: String,
    pub from_style_class: Option<String>,
    pub to: String,
    pub to_style_class: Option<String>,
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateNote {
    pub placement: NotePlacement,
    /// The state the note is attached to.
    pub target: String,
    /// The note's text. A text with a newline in it is written as a
    /// `note … end note` block.
    pub text: String,
}

/// A state with a diagram inside it: `state X { … }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompositeState {
    pub id: String,
    pub description: Option<String>,
    pub body: StateScope,
}

/// The statements of the diagram, or of one composite state, in the order they
/// are written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StateScope {
    pub direction: Option<FlowDirection>,
    pub items: Vec<StateItem>,
}

/// One statement.
///
/// Order matters: the `--` of [`StateItem::Divider`] splits the states around it
/// into concurrent regions, so a flat list of states and a flat list of
/// transitions could not say what the fence says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateItem {
    State(StateEntry),
    Transition(StateTransition),
    Composite(CompositeState),
    Note(StateNote),
    /// The `--` that separates two concurrent regions of a composite state.
    Divider,
    /// A line the model does not carry — a comment, `classDef` / `class` /
    /// `style` / `click`, an accessibility statement, `hide empty description`,
    /// `scale … width`, a floating `note "…" as id`, or a `state X` that opens
    /// no block and therefore declares nothing. Written back verbatim, in place,
    /// because a comment that moved out of the composite state it explains would
    /// be as bad as a comment that vanished.
    Preserved(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDocument {
    pub keyword: StateKeyword,
    pub body: StateScope,
    /// The frontmatter, `%%{init}%%` directives and comments that come before
    /// the diagram keyword. Everything inside the diagram is a
    /// [`StateItem::Preserved`] of the scope it was written in.
    pub preserved_lines: Vec<PreservedLine>,
}

impl StateDocument {
    /// A blank state diagram, used when the editor is opened on an empty fence.
    pub fn empty() -> Self {
        Self {
            keyword: StateKeyword::StateDiagramV2,
            body: StateScope::default(),
            preserved_lines: Vec::new(),
        }
    }

    /// Every state id the diagram names, in the order it first appears.
    /// [`TERMINAL_STATE`] is not one of them.
    pub fn state_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        collect_state_ids(&self.body, &mut ids);
        ids
    }

    /// An id no state is using, based on `prefix`.
    pub fn unused_id(&self, prefix: &str) -> String {
        let taken = self.state_ids();
        let taken = |candidate: &str| taken.iter().any(|id| id == candidate);
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

fn collect_state_ids(scope: &StateScope, ids: &mut Vec<String>) {
    fn push(id: &str, ids: &mut Vec<String>) {
        if id != TERMINAL_STATE && !ids.iter().any(|known| known == id) {
            ids.push(id.to_string());
        }
    }

    for item in &scope.items {
        match item {
            StateItem::State(entry) => push(&entry.id, ids),
            StateItem::Transition(transition) => {
                push(&transition.from, ids);
                push(&transition.to, ids);
            }
            StateItem::Composite(composite) => {
                push(&composite.id, ids);
                collect_state_ids(&composite.body, ids);
            }
            StateItem::Note(_) | StateItem::Divider | StateItem::Preserved(_) => {}
        }
    }
}

/// Whether `source` is a state diagram. Cheap enough to call before parsing.
pub fn is_state_source(source: &str) -> bool {
    first_statement_line(source).is_some_and(|line| header_keyword(line).is_some())
}

/// `merman`'s lexer ignores the case of the diagram keyword, but the detector
/// that runs before it does not: a fence that opens with `statediagram-v2` is
/// not recognized as a diagram at all. So the spelling has to match, and
/// `stateDiagram-v2` has to be tried before `stateDiagram`.
fn header_keyword(line: &str) -> Option<StateKeyword> {
    [StateKeyword::StateDiagramV2, StateKeyword::StateDiagram]
        .into_iter()
        .find(|keyword| keyword_tail(line, keyword.as_str()).is_some())
}

/// Reads `source` — the body of a ```` ```mermaid ```` fence — into an editable
/// document.
pub fn parse_state(source: &str) -> Result<StateDocument> {
    if !source.trim().is_empty() {
        let engine = Engine::new();
        let parsed = engine
            .parse_diagram_for_render_model_sync(source, ParseOptions::strict())
            .map_err(|error| {
                anyhow!("Mermaid の構文として読めませんでした ({error})。テキストのまま編集してください。")
            })?
            .ok_or_else(|| anyhow!("Mermaid の図として解釈できませんでした。"))?;
        if !matches!(parsed.model, RenderSemanticModel::State(_)) {
            bail!("この図は状態遷移図ではありません。");
        }
    }

    let lines = source.lines().collect::<Vec<_>>();
    let preamble = scan_preamble(&lines);

    // An empty fence: the editor opens on a blank diagram, and whatever
    // comments the user left keep their place.
    let Some(header_index) = preamble.header else {
        return Ok(StateDocument {
            keyword: StateKeyword::StateDiagramV2,
            body: StateScope::default(),
            preserved_lines: preamble.preserved,
        });
    };

    let header = lines.get(header_index).copied().unwrap_or_default().trim();
    let Some(keyword) = header_keyword(header) else {
        bail!("状態遷移図ではありません (`stateDiagram` で始まる図だけを構造化編集できます)。");
    };
    let rest = header
        .get(keyword.as_str().len()..)
        .unwrap_or_default()
        .trim();
    if !rest.is_empty() {
        bail!(
            "`{}` の行に `{rest}` が続いています。次の行に分けてからもう一度開いてください。",
            keyword.as_str()
        );
    }

    let mut parser = StateParser {
        lines: lines.iter().map(|line| line.to_string()).collect(),
        index: header_index + 1,
    };
    let body = parser.parse_scope(0)?;

    Ok(StateDocument {
        keyword,
        body,
        preserved_lines: preamble.preserved,
    })
}

/// A composite state written on one line (`state S { a --> b }`) spread over
/// a line per statement, the way the emitter writes it. Mermaid needs no line
/// break inside a block, so the two spellings draw the same diagram, and the
/// parser only has to know one.
///
/// The parser asks for this only for a line it reads as a `state` statement:
/// the same characters in a note's text or an `accDescr` block are prose and
/// must come back as they were written. A block nested on the same line is
/// spread when the parser reaches its own line, and a line holding more than
/// one statement after its `{` is refused there.
fn spread_one_line_block(line: &str) -> Option<Vec<String>> {
    let indent = line
        .get(..line.len() - line.trim_start().len())
        .unwrap_or_default();
    let (head, inner) = split_one_line_block(line.trim())?;
    let mut spread = vec![format!("{indent}{head} {{")];
    match inner.strip_suffix('}') {
        Some(body) => {
            let body = body.trim();
            if !body.is_empty() {
                spread.push(format!("{indent}{INDENT}{body}"));
            }
            spread.push(format!("{indent}}}"));
        }
        // The block goes on over the lines below, which the parser reads as
        // they are.
        None => spread.push(format!("{indent}{INDENT}{inner}")),
    }
    Some(spread)
}

/// The `state …` part and what follows the `{` of a `state` statement that
/// opens a block and goes on writing on the same line.
fn split_one_line_block(statement: &str) -> Option<(&str, &str)> {
    let rest = statement_keyword(statement, "state")?;
    let search_from = match rest.strip_prefix('"') {
        Some(quoted) => statement.len() - quoted.len() + quoted.find('"')? + 1,
        None => statement.len() - rest.len(),
    };
    let brace = search_from + statement.get(search_from..)?.find('{')?;
    let inner = statement.get(brace + 1..)?.trim();
    if inner.is_empty() {
        return None;
    }
    Some((statement.get(..brace)?.trim_end(), inner))
}

struct StateParser {
    lines: Vec<String>,
    index: usize,
}

impl StateParser {
    fn parse_scope(&mut self, depth: usize) -> Result<StateScope> {
        if depth > MAX_NESTING_DEPTH {
            bail!("複合状態の入れ子が深すぎます。テキストのまま編集してください。");
        }
        let mut scope = StateScope::default();

        while let Some(line) = self.lines.get(self.index).cloned() {
            let trimmed = line.trim();
            let start = self.index;
            self.index += 1;

            if trimmed.is_empty() {
                continue;
            }
            if trimmed == "}" {
                if depth == 0 {
                    bail!("対応する `{{` のない `}}` があります。");
                }
                return Ok(scope);
            }
            if trimmed.starts_with("%%") || trimmed.starts_with('#') {
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
                scope
                    .items
                    .push(StateItem::Preserved(self.preserved(start)));
                continue;
            }
            if trimmed == "--" {
                if depth == 0 {
                    bail!("並行区切り `--` は複合状態 (`state X {{ … }}`) の中だけに書けます。");
                }
                scope.items.push(StateItem::Divider);
                continue;
            }
            if let Some(rest) = statement_keyword(trimmed, "direction") {
                let Some(direction) = direction_from_keyword(rest) else {
                    bail!(
                        "`direction {rest}` は読めません (TB / BT / LR / RL のどれかを書いてください)。"
                    );
                };
                if scope.direction.is_some() {
                    bail!(
                        "`direction` が同じ場所に 2 回書かれています。1 つにまとめてからもう一度開いてください。"
                    );
                }
                scope.direction = Some(direction);
                continue;
            }
            if self.is_preserved_statement(trimmed) {
                scope
                    .items
                    .push(StateItem::Preserved(self.preserved(start)));
                continue;
            }
            if let Some(rest) = statement_keyword(trimmed, "note") {
                let item = self.parse_note(rest, trimmed, start)?;
                scope.items.push(item);
                continue;
            }
            if let Some(rest) = statement_keyword(trimmed, "state") {
                if let Some(spread) = spread_one_line_block(&line) {
                    self.lines.splice(start..self.index, spread);
                    self.index = start;
                    continue;
                }
                let item = self.parse_state_statement(rest, trimmed, start, depth)?;
                scope.items.push(item);
                continue;
            }
            // Mermaid lets a statement end with the separator, and a hand
            // written `A --> B;` is common enough that refusing to open the
            // diagram over it would be unkind. The emitter never writes one, so
            // confirming the editor drops it.
            let statement = match trimmed.strip_suffix(';') {
                Some(without_separator) if !ends_with_entity(trimmed) => {
                    without_separator.trim_end()
                }
                _ => trimmed,
            };
            if statement.is_empty() {
                continue;
            }
            scope.items.push(parse_plain_statement(statement, &line)?);
        }

        if depth > 0 {
            bail!("`{{` に対応する `}}` がありません。");
        }
        Ok(scope)
    }

    /// The lines from `start` up to the cursor, verbatim.
    fn preserved(&self, start: usize) -> String {
        let lines = self
            .lines
            .get(start..self.index)
            .unwrap_or_default()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        join_lines(&lines)
    }

    /// Whether `statement` is one of the statements the model does not carry.
    /// Advances the cursor past an `accDescr { … }` block.
    fn is_preserved_statement(&mut self, statement: &str) -> bool {
        if !PRESERVED_STATEMENT_KEYWORDS
            .iter()
            .any(|keyword| statement_keyword(statement, keyword).is_some())
        {
            return false;
        }
        if statement_keyword(statement, "accDescr").is_some_and(|rest| rest.starts_with('{'))
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
        true
    }

    fn parse_note(&mut self, rest: &str, statement: &str, start: usize) -> Result<StateItem> {
        // Mermaid parses a floating note and then ignores it, so there is
        // nothing to model and nothing to lose by keeping the line.
        if rest.starts_with('"') {
            return Ok(StateItem::Preserved(self.preserved(start)));
        }

        let (placement, after) = if let Some(after) = strip_prefix_ignoring_case(rest, "left of") {
            (NotePlacement::LeftOf, after)
        } else if let Some(after) = strip_prefix_ignoring_case(rest, "right of") {
            (NotePlacement::RightOf, after)
        } else {
            bail!("`note` のあとは `left of` か `right of` です ({statement})。");
        };

        let after = after.trim_start();
        let target_end = after
            .char_indices()
            .find(|(_, character)| {
                *character == ':' || *character == '-' || character.is_whitespace()
            })
            .map(|(offset, _)| offset)
            .unwrap_or(after.len());
        let target = after.get(..target_end).unwrap_or_default();
        if !is_valid_state_id(target) {
            bail!("`{target}` は状態 ID に使えません ({statement})。");
        }
        let tail = after.get(target_end..).unwrap_or_default().trim_start();

        if let Some(text) = tail.strip_prefix(':') {
            return Ok(StateItem::Note(StateNote {
                placement,
                target: target.to_string(),
                text: text.trim().to_string(),
            }));
        }

        let mut text_lines = Vec::new();
        if !tail.is_empty() {
            text_lines.push(tail.to_string());
        }
        loop {
            let Some(line) = self.lines.get(self.index).cloned() else {
                bail!("`note` に対応する `end note` がありません ({statement})。");
            };
            self.index += 1;
            let trimmed = line.trim();
            if trimmed.eq_ignore_ascii_case("end note") {
                break;
            }
            // Mermaid ends the block at the first `end note` it finds anywhere,
            // including in the middle of a line, and reads the rest of that line
            // as statements.
            if trimmed.to_ascii_lowercase().contains("end note") {
                bail!(
                    "`end note` を含む行はノートの本文に書けません ({trimmed})。テキストのまま編集してください。"
                );
            }
            text_lines.push(trimmed.to_string());
        }
        while text_lines.first().is_some_and(|line| line.is_empty()) {
            text_lines.remove(0);
        }
        while text_lines.last().is_some_and(|line| line.is_empty()) {
            text_lines.pop();
        }

        Ok(StateItem::Note(StateNote {
            placement,
            target: target.to_string(),
            text: text_lines.join("\n"),
        }))
    }

    fn parse_state_statement(
        &mut self,
        rest: &str,
        statement: &str,
        start: usize,
        depth: usize,
    ) -> Result<StateItem> {
        if let Some(quoted) = rest.strip_prefix('"') {
            let Some((description, after)) = quoted.split_once('"') else {
                bail!("`state \"説明\"` の引用符が閉じていません ({statement})。");
            };
            let Some(after_as) = strip_prefix_ignoring_case(after.trim_start(), "as") else {
                bail!("`state \"説明\" as ID` の形で書いてください ({statement})。");
            };
            let (id, opens_block) = split_block_open(after_as, statement)?;
            if !is_valid_state_id(&id) {
                bail!("`{id}` は状態 ID に使えません ({statement})。");
            }
            let description = Some(decode_entities(description.trim()));
            if opens_block || self.take_block_open() {
                return Ok(StateItem::Composite(CompositeState {
                    id,
                    description,
                    body: self.parse_scope(depth + 1)?,
                }));
            }
            return Ok(StateItem::State(StateEntry {
                id,
                description,
                kind: StateKind::Normal,
                style_class: None,
            }));
        }

        if let Some((id, kind)) = split_kind_marker(rest) {
            if !is_valid_state_id(&id) {
                bail!("`{id}` は状態 ID に使えません ({statement})。");
            }
            return Ok(StateItem::State(StateEntry {
                id,
                description: None,
                kind,
                style_class: None,
            }));
        }

        let (id, opens_block) = split_block_open(rest, statement)?;
        if !is_valid_state_id(&id) {
            bail!("`{id}` は状態 ID に使えません ({statement})。");
        }
        if opens_block || self.take_block_open() {
            return Ok(StateItem::Composite(CompositeState {
                id,
                description: None,
                body: self.parse_scope(depth + 1)?,
            }));
        }
        // `state X` with no block declares nothing in Mermaid. Reading it as a
        // state would add one to the picture on the next emit.
        Ok(StateItem::Preserved(self.preserved(start)))
    }

    /// Consumes a `{` that opens a block on a line of its own, which Mermaid
    /// accepts after `state X`. Only blank lines may come between the two.
    fn take_block_open(&mut self) -> bool {
        let mut look = self.index;
        while self
            .lines
            .get(look)
            .is_some_and(|line| line.trim().is_empty())
        {
            look += 1;
        }
        if self.lines.get(look).is_some_and(|line| line.trim() == "{") {
            self.index = look + 1;
            return true;
        }
        false
    }
}

/// A transition, or a state declaration without the `state` keyword.
fn parse_plain_statement(statement: &str, line: &str) -> Result<StateItem> {
    let (head, description) = split_description(statement)?;
    let head = head.trim();
    if head.contains("%%") {
        bail!(
            "文の末尾のコメントは構造化編集で失われます ({statement})。コメントを独立した行に移すか、テキストのまま編集してください。"
        );
    }

    if let Some((left, right)) = head.split_once("-->") {
        let (from, from_style_class) = parse_endpoint(left, statement)?;
        let (to, to_style_class) = parse_endpoint(right, statement)?;
        return Ok(StateItem::Transition(StateTransition {
            from,
            from_style_class,
            to,
            to_style_class,
            label: description,
        }));
    }

    // A lone `[*]` is not a state and declares nothing, so the line stays as it
    // is rather than being read as a state called `[*]`.
    if head == TERMINAL_STATE {
        return Ok(StateItem::Preserved(line.trim_end().to_string()));
    }

    let (id, style_class) = parse_endpoint(head, statement)?;
    if id == TERMINAL_STATE {
        return Ok(StateItem::Preserved(line.trim_end().to_string()));
    }
    Ok(StateItem::State(StateEntry {
        id,
        description,
        kind: StateKind::Normal,
        style_class,
    }))
}

/// Splits a statement at the `:` that starts its description, skipping the
/// `:::` that applies a class and the `#…;` entities that hold a `:` of their
/// own.
fn split_description(statement: &str) -> Result<(&str, Option<String>)> {
    let mut offset = 0;
    while let Some(rest) = statement.get(offset..) {
        let Some(character) = rest.chars().next() else {
            break;
        };
        if let Some(length) = entity_length(rest) {
            offset += length;
            continue;
        }
        if character != ':' {
            offset += character.len_utf8();
            continue;
        }
        if rest.starts_with(":::") {
            offset += 3;
            continue;
        }
        let head = statement.get(..offset).unwrap_or_default();
        let text = statement.get(offset + 1..).unwrap_or_default().trim();
        if let Some(separator) = bare_separator(text) {
            bail!(
                "説明の `{separator}` はそのままでは書けません ({statement})。`:` は `#58;`、`;` は `#59;` と書くか、テキストのまま編集してください。"
            );
        }
        return Ok((head, Some(decode_entities(text))));
    }
    Ok((statement, None))
}

/// The `:` or `;` in `text` that Mermaid's lexer would read as the end of a
/// description, i.e. one that is not inside a `#…;` entity.
fn bare_separator(text: &str) -> Option<char> {
    let mut offset = 0;
    while let Some(rest) = text.get(offset..) {
        let character = rest.chars().next()?;
        if let Some(length) = entity_length(rest) {
            offset += length;
            continue;
        }
        if character == ':' || character == ';' {
            return Some(character);
        }
        offset += character.len_utf8();
    }
    None
}

/// How long the `#…;` entity at the start of `text` is, if there is one.
///
/// Mermaid replaces these with placeholders before its lexer runs — that is the
/// only reason a description can hold a `:` at all — so a scanner that reads the
/// source itself has to skip them the same way.
fn entity_length(text: &str) -> Option<usize> {
    let rest = text.strip_prefix('#')?;
    let end = rest.find(';')?;
    let name = rest.get(..end)?;
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        return None;
    }
    Some('#'.len_utf8() + end + ';'.len_utf8())
}

/// Reads `id` or `id:::class`, and the `[*]` that stands for the start and the
/// end of the diagram.
fn parse_endpoint(text: &str, statement: &str) -> Result<(String, Option<String>)> {
    let text = text.trim();
    let (id, style_class) = match text.split_once(":::") {
        Some((id, class)) => (id.trim(), Some(class.trim().to_string())),
        None => (text, None),
    };
    if let Some(class) = &style_class
        && !is_valid_style_class(class)
    {
        bail!("`{class}` はクラス名に使えません ({statement})。");
    }
    if id == TERMINAL_STATE {
        return Ok((TERMINAL_STATE.to_string(), style_class));
    }
    if !is_valid_state_id(id) {
        bail!(
            "`{id}` は状態 ID に使えません ({statement})。空白・`:`・`;`・`-` を含む ID は Mermaid が読めません。"
        );
    }
    Ok((id.to_string(), style_class))
}

/// Splits the `{` that opens a composite state off the end of a `state`
/// statement.
fn split_block_open(text: &str, statement: &str) -> Result<(String, bool)> {
    match text.find('{') {
        Some(position) => {
            let after = text.get(position + 1..).unwrap_or_default().trim();
            if !after.is_empty() {
                bail!(
                    "`{{` のあとは改行してください ({statement})。1 行で書いた複合状態は構造化編集に対応していません。"
                );
            }
            Ok((
                text.get(..position).unwrap_or_default().trim().to_string(),
                true,
            ))
        }
        None => Ok((text.trim().to_string(), false)),
    }
}

/// Splits a `<<fork>>` / `<<join>>` / `<<choice>>` marker off the end of a
/// `state` statement. Mermaid also accepts the `[[fork]]` spelling.
fn split_kind_marker(text: &str) -> Option<(String, StateKind)> {
    let trimmed = text.trim();
    let lowercase = trimmed.to_ascii_lowercase();
    for (marker, kind) in [
        ("<<fork>>", StateKind::Fork),
        ("[[fork]]", StateKind::Fork),
        ("<<join>>", StateKind::Join),
        ("[[join]]", StateKind::Join),
        ("<<choice>>", StateKind::Choice),
        ("[[choice]]", StateKind::Choice),
    ] {
        if lowercase.ends_with(marker) {
            let base = trimmed.get(..trimmed.len() - marker.len())?.trim();
            return Some((base.to_string(), kind));
        }
    }
    None
}

fn direction_from_keyword(text: &str) -> Option<FlowDirection> {
    FlowDirection::ALL
        .into_iter()
        .find(|direction| direction.keyword() == text)
}

fn strip_prefix_ignoring_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    if !head.eq_ignore_ascii_case(prefix) {
        return None;
    }
    let rest = text.get(prefix.len()..)?;
    if rest.is_empty() || rest.starts_with(|character: char| character.is_whitespace()) {
        return Some(rest.trim_start());
    }
    None
}

/// Writes `document` back as Mermaid source, without a trailing newline.
pub fn emit_state(document: &StateDocument) -> String {
    let mut out = String::new();

    for line in preserved_at(document, PreservedPosition::Preamble) {
        out.push_str(line);
        out.push('\n');
    }

    out.push_str(document.keyword.as_str());
    out.push('\n');

    for line in preserved_at(document, PreservedPosition::Top) {
        out.push_str(line);
        out.push('\n');
    }

    emit_scope(&document.body, 1, &mut out);

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
    document: &StateDocument,
    position: PreservedPosition,
) -> impl Iterator<Item = &str> {
    document
        .preserved_lines
        .iter()
        .filter(move |line| line.position == position)
        .map(|line| line.text.as_str())
}

fn emit_scope(scope: &StateScope, depth: usize, out: &mut String) {
    let indent = INDENT.repeat(depth);

    if let Some(direction) = scope.direction {
        let _ = writeln!(out, "{indent}direction {}", direction.keyword());
    }

    for item in &scope.items {
        match item {
            StateItem::Preserved(text) => {
                out.push_str(text);
                out.push('\n');
            }
            StateItem::Divider => {
                let _ = writeln!(out, "{indent}--");
            }
            StateItem::State(entry) => emit_entry(entry, &indent, out),
            StateItem::Transition(transition) => {
                let _ = writeln!(out, "{indent}{}", transition_statement(transition));
            }
            StateItem::Note(note) => emit_note(note, &indent, out),
            StateItem::Composite(composite) => {
                match &composite.description {
                    Some(description) => {
                        let _ = writeln!(
                            out,
                            "{indent}state \"{}\" as {} {{",
                            encode_quoted(description),
                            composite.id
                        );
                    }
                    None => {
                        let _ = writeln!(out, "{indent}state {} {{", composite.id);
                    }
                }
                emit_scope(&composite.body, depth + 1, out);
                let _ = writeln!(out, "{indent}}}");
            }
        }
    }
}

fn emit_entry(entry: &StateEntry, indent: &str, out: &mut String) {
    // The marker spelling carries neither a description nor a class, so either
    // one needs a statement of its own. Re-reading the result gives one item per
    // line, and emitting that again writes the same two lines.
    if let Some(marker) = entry.kind.marker() {
        let _ = writeln!(out, "{indent}state {} <<{marker}>>", entry.id);
        if let Some(class) = &entry.style_class {
            let _ = writeln!(out, "{indent}{}:::{class}", entry.id);
        }
        if let Some(description) = &entry.description {
            let _ = writeln!(out, "{indent}{} : {}", entry.id, encode_bare(description));
        }
        return;
    }

    let head = match &entry.style_class {
        Some(class) => format!("{}:::{class}", entry.id),
        None => entry.id.clone(),
    };
    match &entry.description {
        None => {
            let _ = writeln!(out, "{indent}{head}");
        }
        // The quoted spelling reads better than an escaped colon, but it has
        // nowhere to put a class.
        Some(description) if entry.style_class.is_none() && description.contains([':', ';']) => {
            let _ = writeln!(
                out,
                "{indent}state \"{}\" as {}",
                encode_quoted(description),
                entry.id
            );
        }
        Some(description) if description.is_empty() => {
            let _ = writeln!(out, "{indent}{head} :");
        }
        Some(description) => {
            let _ = writeln!(out, "{indent}{head} : {}", encode_bare(description));
        }
    }
}

fn emit_note(note: &StateNote, indent: &str, out: &mut String) {
    if note.text.contains('\n') {
        let _ = writeln!(
            out,
            "{indent}note {} {}",
            note.placement.keyword(),
            note.target
        );
        for line in note.text.lines() {
            if line.is_empty() {
                out.push('\n');
                continue;
            }
            let _ = writeln!(out, "{indent}{INDENT}{line}");
        }
        let _ = writeln!(out, "{indent}end note");
        return;
    }
    if note.text.is_empty() {
        let _ = writeln!(
            out,
            "{indent}note {} {} :",
            note.placement.keyword(),
            note.target
        );
        return;
    }
    let _ = writeln!(
        out,
        "{indent}note {} {} : {}",
        note.placement.keyword(),
        note.target,
        note.text
    );
}

fn transition_statement(transition: &StateTransition) -> String {
    let from = endpoint_text(&transition.from, &transition.from_style_class);
    let to = endpoint_text(&transition.to, &transition.to_style_class);
    match &transition.label {
        None => format!("{from} --> {to}"),
        Some(label) if label.is_empty() => format!("{from} --> {to} :"),
        Some(label) => format!("{from} --> {to} : {}", encode_bare(label)),
    }
}

fn endpoint_text(id: &str, style_class: &Option<String>) -> String {
    match style_class {
        Some(class) => format!("{id}:::{class}"),
        None => id.to_string(),
    }
}

/// The `#…;` entity shorthand Mermaid decodes when it draws a label, for the
/// characters a statement cannot hold: `:` and `;` end a description, and `"`
/// ends a quoted one.
const ENTITIES: [(&str, char); 6] = [
    ("#58;", ':'),
    ("#colon;", ':'),
    ("#59;", ';'),
    ("#semi;", ';'),
    ("#34;", '"'),
    ("#quot;", '"'),
];

/// Decodes the entities [`encode_bare`] and [`encode_quoted`] write, so that
/// the editor shows the text the picture shows.
fn decode_entities(text: &str) -> String {
    if !text.contains('#') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    'scan: while let Some(position) = rest.find('#') {
        out.push_str(rest.get(..position).unwrap_or_default());
        let tail = rest.get(position..).unwrap_or_default();
        for (entity, character) in ENTITIES {
            if let Some(after) = tail.strip_prefix(entity) {
                out.push(character);
                rest = after;
                continue 'scan;
            }
        }
        out.push('#');
        rest = tail.get(1..).unwrap_or_default();
    }
    out.push_str(rest);
    out
}

/// A description or a transition label as it appears after a `:`, which runs to
/// the end of the line and stops at `:` or `;`.
///
/// One pass, because escaping `:` first and `;` second would turn the `;` of a
/// freshly written `#58;` into `#59;` in turn.
fn encode_bare(text: &str) -> String {
    encode(text, |character| match character {
        ':' => Some("#58;"),
        ';' => Some("#59;"),
        _ => None,
    })
}

/// A description as it appears inside the quotes of `state "…" as id`, which
/// stops at the next quote.
fn encode_quoted(text: &str) -> String {
    encode(text, |character| match character {
        '"' => Some("#quot;"),
        _ => None,
    })
}

fn encode(text: &str, entity: impl Fn(char) -> Option<&'static str>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if let Some(entity) = entity(character) {
            out.push_str(entity);
            continue;
        }
        match character {
            '\r' => {
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
                out.push_str("<br/>");
            }
            '\n' => out.push_str("<br/>"),
            _ => out.push(character),
        }
    }
    out
}

/// Whether the `;` at the end of `statement` closes a `#…;` entity — `#quot;`
/// and its like — rather than ending the statement. Dropping that one would
/// turn the character back into the text that spells it.
fn ends_with_entity(statement: &str) -> bool {
    let Some(body) = statement.strip_suffix(';') else {
        return false;
    };
    let Some(hash) = body.rfind('#') else {
        return false;
    };
    let name = body.get(hash + 1..).unwrap_or_default();
    !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
}

/// Whether `id` can be written as a state id.
///
/// Mermaid's lexer ends an id at whitespace, `:`, `-` and `{`, and the
/// characters that spell the other constructs — `[*]`, `<<fork>>`, `:::`, a
/// comment — would be read as those constructs. Everything outside ASCII is
/// allowed, so a Japanese id works.
pub fn is_valid_state_id(id: &str) -> bool {
    !id.is_empty()
        && id.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || character == '_'
                || character == '.'
                || (!character.is_ascii() && !character.is_whitespace() && !character.is_control())
        })
        && !is_reserved_state_id(id)
}

/// Words that start a statement of their own, whatever their case, and that a
/// state of the same name would be read as.
///
/// [`PRESERVED_STATEMENT_KEYWORDS`] is part of the list rather than repeated in
/// it: a line that opens with one of those words becomes a
/// [`StateItem::Preserved`], so a state named after one would leave the model on
/// the next parse and the editor would show the row as an unreadable line.
fn is_reserved_state_id(id: &str) -> bool {
    const RESERVED: [&str; 5] = ["state", "direction", "note", "href", "as"];
    RESERVED
        .iter()
        .chain(PRESERVED_STATEMENT_KEYWORDS.iter())
        .any(|reserved| reserved.eq_ignore_ascii_case(id))
        || header_keyword(id).is_some()
}

/// Whether `name` can be written as the class of `id:::class`.
pub fn is_valid_style_class(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Whether `text` can be written as a note's text and read back unchanged.
///
/// A text of several lines is written as a `note … end note` block, which the
/// terminator cannot appear in, and whose lines come back trimmed — Mermaid
/// itself trims them, so a text this rejects is one the picture would not show
/// either.
pub fn is_valid_note_text(text: &str) -> bool {
    if text.contains('\r') {
        return false;
    }
    if !text.contains('\n') {
        return text.trim() == text;
    }
    !text.to_ascii_lowercase().contains("end note")
        && !text.starts_with('\n')
        && !text.ends_with('\n')
        && text.lines().all(|line| line.trim() == line)
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
            parse_state(source).unwrap_or_else(|error| panic!("{source} should parse: {error}"));
        let first_text = emit_state(&first);
        let second = parse_state(&first_text)
            .unwrap_or_else(|error| panic!("emitted source should parse: {error}\n{first_text}"));
        let second_text = emit_state(&second);
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

    /// Mermaid ends a statement at `;` as well as at the end of the line, and
    /// a diagram written that way has to open rather than be refused. The
    /// emitter drops the separator, and the `;` that closes an entity stays.
    #[test]
    fn a_statement_may_end_with_the_separator() {
        let text = assert_stable(
            "stateDiagram-v2
    [*] --> A;
    A --> [*];
    ;",
        );
        assert_eq!(
            text,
            "stateDiagram-v2
    [*] --> A
    A --> [*]",
            "the separator should not be written back"
        );

        let document = parse_state(
            "stateDiagram-v2
    A : #quot;引用#quot;",
        )
        .expect("an entity at the end of a description should survive");
        let Some(StateItem::State(entry)) = document.body.items.first() else {
            panic!("the diagram declares one state");
        };
        assert_eq!(entry.description.as_deref(), Some("\"引用\""));
    }

    fn assert_renders(source: &str) {
        let svg = render_to_svg(source, &MermaidTheme::default())
            .unwrap_or_else(|error| panic!("emitted source should render: {error}\n{source}"));
        assert!(
            !svg.contains("Syntax error in text"),
            "emitted source rendered an error diagram:\n{source}"
        );
    }

    const EXAMPLES: [&str; 17] = [
        "stateDiagram-v2\n    [*] --> Idle\n    Idle --> Running: 開始\n    Running --> Idle: 完了\n    Running --> [*]: 中止",
        "stateDiagram\n  [*] --> A\n  A --> [*]",
        "stateDiagram-v2\n  direction LR\n  [*] --> A\n  A --> B\n  B --> [*]",
        "stateDiagram-v2\n  s1 : 説明つきの状態\n  state \"引用つきの説明\" as s2\n  s1 --> s2",
        "stateDiagram-v2\n  state 分岐 <<choice>>\n  state 分流 <<fork>>\n  state 合流 <<join>>\n  [*] --> 分岐",
        "stateDiagram-v2\n  state Outer {\n    direction TB\n    A --> B\n  }\n  [*] --> Outer",
        "stateDiagram-v2\n  state \"外側\" as Outer {\n    state Inner {\n      A --> B\n    }\n    C --> Inner\n  }",
        "stateDiagram-v2\n  state Parallel {\n    A --> B\n    --\n    C --> D\n  }",
        "stateDiagram-v2\n  A --> B\n  note left of A : 左のノート\n  note right of B\n    1 行目\n    2 行目\n  end note",
        "stateDiagram-v2\n  %% 先頭のメモ\n  A --> B\n  %% 末尾のメモ\n  classDef warn fill:#f96\n  class A warn\n  style B fill:#eee",
        "stateDiagram-v2\n  accTitle: 概要\n  accDescr {\n    説明の 1 行目\n    2 行目\n  }\n  A --> B",
        "stateDiagram-v2\n  A:::warn --> B\n  classDef warn fill:#f96",
        "stateDiagram-v2\n  [*] --> 開始\n  開始 --> 終了 : 日本語のイベント\n  終了 --> [*]",
        "stateDiagram-v2\n  state A {\n    [*] --> a1\n    a1 --> [*]\n  }\n  state B {\n    [*] --> b1\n  }\n  A --> B",
        // The fork, choice and concurrency examples from Mermaid's own
        // documentation.
        "stateDiagram-v2\n    state fork_state <<fork>>\n      [*] --> fork_state\n      fork_state --> State2\n      fork_state --> State3\n      state join_state <<join>>\n      State2 --> join_state\n      State3 --> join_state\n      join_state --> State4\n      State4 --> [*]",
        "stateDiagram-v2\n    state if_state <<choice>>\n    [*] --> IsPositive\n    IsPositive --> if_state\n    if_state --> False: if n < 0\n    if_state --> True : if n >= 0",
        "stateDiagram-v2\n    [*] --> Active\n    state Active {\n        [*] --> NumLockOff\n        NumLockOff --> NumLockOn : EvNumLockPressed\n        NumLockOn --> NumLockOff : EvNumLockPressed\n        --\n        [*] --> CapsLockOff\n        CapsLockOff --> CapsLockOn : EvCapsLockPressed\n    }",
    ];

    #[test]
    fn examples_round_trip_and_render() {
        for source in EXAMPLES {
            let emitted = assert_stable(source);
            assert_renders(&emitted);
        }
    }

    #[test]
    fn the_insert_template_round_trips() {
        let template = "stateDiagram-v2\n    %% [*] は開始と終了。矢印のあとの : がイベント名\n    [*] --> Idle\n    Idle --> Running: 開始\n    Running --> Idle: 完了\n    Running --> [*]: 中止";
        let document = parse_state(template).expect("the template should parse");
        assert_eq!(document.keyword, StateKeyword::StateDiagramV2);
        assert_eq!(
            document.state_ids(),
            vec!["Idle".to_string(), "Running".to_string()]
        );
        let emitted = assert_stable(template);
        assert!(
            emitted.contains("%% [*] は開始と終了"),
            "the template comment should survive:\n{emitted}"
        );
        assert_renders(&emitted);
    }

    #[test]
    fn the_diagram_keyword_keeps_its_version() {
        for (source, keyword) in [
            ("stateDiagram\n  A --> B", StateKeyword::StateDiagram),
            ("stateDiagram-v2\n  A --> B", StateKeyword::StateDiagramV2),
        ] {
            let document = parse_state(source).expect("source should parse");
            assert_eq!(document.keyword, keyword, "for {source}");
            assert!(emit_state(&document).starts_with(keyword.as_str()));
        }

        // `merman`'s detector is case-sensitive, so a lowercase keyword is not a
        // diagram it draws and not one this editor opens either.
        assert!(!is_state_source("statediagram-v2\n  A --> B"));
        assert!(parse_state("statediagram-v2\n  A --> B").is_err());
    }

    #[test]
    fn a_statement_is_one_item_even_when_it_repeats_a_state() {
        let source = "stateDiagram-v2\n  A : ひとつ目\n  A : ふたつ目";
        let document = parse_state(source).expect("source should parse");
        assert_eq!(document.body.items.len(), 2);
        assert_eq!(document.state_ids(), vec!["A".to_string()]);
        assert_eq!(
            assert_stable(source),
            "stateDiagram-v2\n    A : ひとつ目\n    A : ふたつ目"
        );
    }

    #[test]
    fn concurrent_regions_keep_the_divider_in_place() {
        let source = "stateDiagram-v2\n  state P {\n    A --> B\n    --\n    C --> D\n  }";
        let document = parse_state(source).expect("source should parse");
        let Some(StateItem::Composite(composite)) = document.body.items.first() else {
            panic!("the composite state should be the first item: {document:?}");
        };
        assert_eq!(
            composite
                .body
                .items
                .iter()
                .map(|item| matches!(item, StateItem::Divider))
                .collect::<Vec<_>>(),
            vec![false, true, false]
        );
        assert_renders(&assert_stable(source));
    }

    #[test]
    fn a_comment_stays_inside_the_composite_state_it_explains() {
        let source =
            "stateDiagram-v2\n  state P {\n    %% 中のメモ\n    A --> B\n  }\n  %% 外のメモ";
        let emitted = assert_stable(source);
        let inner = emitted
            .lines()
            .position(|line| line.contains("中のメモ"))
            .expect("the inner comment");
        let close = emitted
            .lines()
            .position(|line| line.trim() == "}")
            .expect("the closing brace");
        let outer = emitted
            .lines()
            .position(|line| line.contains("外のメモ"))
            .expect("the outer comment");
        assert!(inner < close, "the inner comment moved out:\n{emitted}");
        assert!(close < outer, "the outer comment moved in:\n{emitted}");
    }

    #[test]
    fn a_block_that_opens_on_the_next_line_is_read_as_one() {
        let source = "stateDiagram-v2\n  state P\n  {\n    A --> B\n  }";
        let document = parse_state(source).expect("source should parse");
        assert!(
            matches!(
                document.body.items.first(),
                Some(StateItem::Composite(composite)) if composite.id == "P"
            ),
            "{document:?}"
        );
        assert_renders(&assert_stable(source));
    }

    /// `state X` with no block declares nothing in Mermaid, so reading it as a
    /// state would add one to the picture.
    #[test]
    fn a_state_keyword_without_a_block_stays_a_line() {
        let source = "stateDiagram-v2\n  state P\n  A --> B";
        let document = parse_state(source).expect("source should parse");
        assert!(
            matches!(
                document.body.items.first(),
                Some(StateItem::Preserved(text)) if text.trim() == "state P"
            ),
            "{document:?}"
        );
        assert_eq!(
            assert_stable(source),
            "stateDiagram-v2\n  state P\n    A --> B"
        );
    }

    #[test]
    fn descriptions_and_labels_with_colons_survive() {
        let document = StateDocument {
            keyword: StateKeyword::StateDiagramV2,
            body: StateScope {
                direction: None,
                items: vec![
                    StateItem::State(StateEntry {
                        id: "A".into(),
                        description: Some("10:00 開始".into()),
                        kind: StateKind::Normal,
                        style_class: None,
                    }),
                    StateItem::State(StateEntry {
                        id: "B".into(),
                        description: Some("引用 \" つき".into()),
                        kind: StateKind::Normal,
                        style_class: None,
                    }),
                    StateItem::State(StateEntry {
                        id: "C".into(),
                        description: Some("セミコロン ; つき".into()),
                        kind: StateKind::Normal,
                        style_class: Some("warn".into()),
                    }),
                    StateItem::Transition(StateTransition {
                        from: "A".into(),
                        from_style_class: None,
                        to: "B".into(),
                        to_style_class: None,
                        label: Some("12:30 まで".into()),
                    }),
                    StateItem::Preserved("    classDef warn fill:#f96".into()),
                ],
            },
            preserved_lines: Vec::new(),
        };
        let emitted = emit_state(&document);
        let parsed = parse_state(&emitted)
            .unwrap_or_else(|error| panic!("emitted source should parse: {error}\n{emitted}"));
        assert_eq!(parsed, document, "round trip of:\n{emitted}");
        assert_renders(&emitted);

        // The transition label has nowhere to put a quoted spelling, so its
        // colon travels as the `#58;` entity. Mermaid decodes that when it
        // draws the label, and the picture has to show the colon back.
        assert!(
            emitted.contains("12#58;30"),
            "the transition label should escape its colon:\n{emitted}"
        );
        let svg = render_to_svg(&emitted, &MermaidTheme::default()).expect("should render");
        assert!(
            svg.contains("12:30"),
            "the escaped colon should be drawn as a colon:\n{emitted}"
        );
    }

    #[test]
    fn a_marker_state_carries_its_description_on_a_line_of_its_own() {
        let document = StateDocument {
            keyword: StateKeyword::StateDiagramV2,
            body: StateScope {
                direction: None,
                items: vec![StateItem::State(StateEntry {
                    id: "F".into(),
                    description: Some("分岐する".into()),
                    kind: StateKind::Choice,
                    style_class: None,
                })],
            },
            preserved_lines: Vec::new(),
        };
        let emitted = emit_state(&document);
        assert_eq!(
            emitted,
            "stateDiagram-v2\n    state F <<choice>>\n    F : 分岐する"
        );
        // Reading the two lines back gives one item each, and emitting those
        // writes the same two lines.
        assert_eq!(assert_stable(&emitted), emitted);
    }

    #[test]
    fn a_note_keeps_its_placement_and_its_lines() {
        let source = "stateDiagram-v2\n  A --> B\n  note left of A : 一行\n  note right of B\n    1 行目\n    2 行目\n  end note";
        let document = parse_state(source).expect("source should parse");
        let notes = document
            .body
            .items
            .iter()
            .filter_map(|item| match item {
                StateItem::Note(note) => Some((note.placement, note.text.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            notes,
            vec![
                (NotePlacement::LeftOf, "一行".to_string()),
                (NotePlacement::RightOf, "1 行目\n2 行目".to_string()),
            ]
        );
        assert_renders(&assert_stable(source));

        let with_a_blank_line =
            "stateDiagram-v2\n  A --> B\n  note left of A\n    1 行目\n\n    3 行目\n  end note";
        let document = parse_state(with_a_blank_line).expect("source should parse");
        assert!(
            matches!(
                document.body.items.get(1),
                Some(StateItem::Note(note)) if note.text == "1 行目\n\n3 行目"
            ),
            "{document:?}"
        );
        assert_stable(with_a_blank_line);

        // Mermaid ends a note block at the first `end note` it finds, wherever
        // it is on the line, so a body line that holds one cannot be written
        // back.
        assert!(
            parse_state("stateDiagram-v2\n  note left of A\n    see end note here\n  end note")
                .is_err()
        );
    }

    #[test]
    fn statements_the_model_does_not_carry_survive_verbatim() {
        let source = "stateDiagram-v2\n  hide empty description\n  scale 300 width\n  A --> B\n  note \"浮いたノート\" as N\n  click A href \"https://example.com\"\n  classDef warn fill:#f96\n  class A warn\n  style B fill:#eee\n  accTitle: 概要\n  accDescr: 説明";
        let emitted = assert_stable(source);
        for line in [
            "hide empty description",
            "scale 300 width",
            "note \"浮いたノート\" as N",
            "click A href \"https://example.com\"",
            "classDef warn fill:#f96",
            "class A warn",
            "style B fill:#eee",
            "accTitle: 概要",
            "accDescr: 説明",
        ] {
            assert!(
                emitted.contains(line),
                "`{line}` should survive:\n{emitted}"
            );
        }
        assert_renders(&emitted);
    }

    #[test]
    fn the_bracket_marker_spelling_and_a_terminal_class_survive() {
        let source =
            "stateDiagram-v2\n  state F [[fork]]\n  [*]:::warn --> F\n  classDef warn fill:#f96";
        let document = parse_state(source).expect("source should parse");
        assert!(
            matches!(
                document.body.items.first(),
                Some(StateItem::State(entry)) if entry.kind == StateKind::Fork
            ),
            "{document:?}"
        );
        let emitted = assert_stable(source);
        assert!(
            emitted.contains("state F <<fork>>"),
            "the marker should be written in one spelling:\n{emitted}"
        );
        assert!(
            emitted.contains("[*]:::warn --> F"),
            "the class on the start marker should survive:\n{emitted}"
        );
        assert_renders(&emitted);
    }

    #[test]
    fn an_entity_in_a_description_is_read_as_its_character() {
        let source = "stateDiagram-v2\n  A --> B : 10#58;00#59;まで\n  C : #quot;引用#quot;";
        let document = parse_state(source).expect("source should parse");
        let texts = document
            .body
            .items
            .iter()
            .filter_map(|item| match item {
                StateItem::Transition(transition) => transition.label.clone(),
                StateItem::State(entry) => entry.description.clone(),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            texts,
            vec!["10:00;まで".to_string(), "\"引用\"".to_string()]
        );
        assert_renders(&assert_stable(source));
    }

    #[test]
    fn an_empty_description_round_trips() {
        for source in ["stateDiagram-v2\n  A :", "stateDiagram-v2\n  A --> B :"] {
            let emitted = assert_stable(source);
            assert!(emitted.ends_with(':'), "{emitted}");
        }
    }

    #[test]
    fn an_empty_fence_opens_as_a_blank_diagram() {
        let document = parse_state("").expect("an empty fence should parse");
        assert_eq!(document, StateDocument::empty());
        assert_eq!(emit_state(&document), "stateDiagram-v2");
    }

    #[test]
    fn constructs_that_would_be_dropped_are_refused() {
        for source in [
            "stateDiagram-v2\n  A --> B %% 行末のメモ",
            "stateDiagram-v2\n  direction TB\n  direction LR\n  A --> B",
            "stateDiagram-v2\n  direction TB 余分\n  A --> B",
            "stateDiagram-v2 LR\n  A --> B",
            "stateDiagram-v2\n  state P {\n    A --> B",
            "stateDiagram-v2\n  }\n  A --> B",
            "stateDiagram-v2\n  A --> B --> C",
            "stateDiagram-v2\n  my-state --> B",
            // Two statements after the `{` of a one-line block.
            "stateDiagram-v2\n  state P { A --> B C --> D }",
        ] {
            let error = parse_state(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            let message = format!("{error}");
            assert!(
                message.chars().any(|character| character > '\u{7f}'),
                "the message should be Japanese for `{source}`: {message}"
            );
        }
    }

    /// A chain is not something Mermaid draws: its grammar takes one arrow
    /// per statement. So the editor has nothing to split it into, and the
    /// refusal above stays for as long as `merman` says so.
    #[test]
    fn a_chain_of_transitions_is_not_a_diagram_mermaid_draws() {
        assert!(
            render_to_svg("stateDiagram-v2\n  A --> B --> C", &MermaidTheme::default()).is_err()
        );
    }

    /// A composite state written on one line opens spread over a line per
    /// statement, which is how the editor writes it back. The whole text is
    /// compared, so a statement lost on the way would show.
    #[test]
    fn a_composite_state_on_one_line_opens_spread_out() {
        for (source, expected) in [
            (
                "stateDiagram-v2\n  state P { A --> B }\n  [*] --> P",
                "stateDiagram-v2\n    state P {\n        A --> B\n    }\n    [*] --> P",
            ),
            (
                "stateDiagram-v2\n  state \"外側 { 波かっこ }\" as P { A --> B }",
                "stateDiagram-v2\n    state \"外側 { 波かっこ }\" as P {\n        A --> B\n    }",
            ),
            (
                "stateDiagram-v2\n  state P { state Q { a --> b } }",
                "stateDiagram-v2\n    state P {\n        state Q {\n            a --> b\n        }\n    }",
            ),
            (
                "stateDiagram-v2\n  state P { A --> B\n    B --> C\n  }",
                "stateDiagram-v2\n    state P {\n        A --> B\n        B --> C\n    }",
            ),
            (
                "stateDiagram-v2\n  state P {}\n  [*] --> P",
                "stateDiagram-v2\n    state P {\n    }\n    [*] --> P",
            ),
        ] {
            assert_renders(source);
            let emitted = assert_stable(source);
            assert_eq!(emitted, expected, "for {source}");
            assert_renders(&emitted);
        }
    }

    /// Only a line the parser reads as a `state` statement is spread out. The
    /// same characters in a note's text or an `accDescr` block are prose and
    /// have to come back byte for byte.
    #[test]
    fn a_one_line_block_in_prose_is_left_as_written() {
        for source in [
            "stateDiagram-v2
    s1 --> s2
    note right of s1
        state X { y }
        2 行目
    end note",
            "stateDiagram-v2
  accDescr {
    state X { y
  }
    s1 --> s2",
            "stateDiagram-v2
    s1 : {x}
    s1 --> s2
    note right of s1 : a { b",
        ] {
            assert_renders(source);
            let emitted = assert_stable(source);
            assert_eq!(emitted, source);
        }
    }

    #[test]
    fn other_diagram_types_are_refused() {
        for source in [
            "flowchart TD\n  A --> B",
            "pie title x\n  \"a\" : 1",
            "sequenceDiagram\n  A->>B: hi",
        ] {
            assert!(!is_state_source(source), "{source}");
            assert!(parse_state(source).is_err(), "{source}");
        }
        assert!(is_state_source("stateDiagram-v2\n  A --> B"));
        assert!(is_state_source("%% メモ\nstateDiagram\n  A --> B"));
        assert!(is_state_source(
            "---\ntitle: x\n---\nstateDiagram-v2\n  A --> B"
        ));
    }

    #[test]
    fn ids_classes_and_note_texts_are_validated() {
        assert!(is_valid_state_id("A"));
        assert!(is_valid_state_id("state_1"));
        assert!(is_valid_state_id("開始"));
        assert!(!is_valid_state_id(""));
        assert!(!is_valid_state_id("a b"));
        assert!(!is_valid_state_id("a-b"));
        assert!(!is_valid_state_id("a:b"));
        assert!(!is_valid_state_id("[*]"));
        assert!(!is_valid_state_id("state"));
        assert!(!is_valid_state_id("NOTE"));
        assert!(!is_valid_state_id("stateDiagram-v2"));
        assert!(!is_valid_state_id("あ い"));

        assert!(is_valid_style_class("warn"));
        assert!(!is_valid_style_class(""));
        assert!(!is_valid_style_class("warn!"));

        assert!(is_valid_note_text("end note を含む一行"));
        assert!(!is_valid_note_text("1 行目\nend note\n2 行目"));
        assert!(is_valid_note_text("1 行目\n2 行目"));
        assert!(!is_valid_note_text(" 前後の空白 "));
        assert!(!is_valid_note_text("1 行目\n  2 行目"));
        assert!(!is_valid_note_text("1 行目\n"));
        assert!(!is_valid_note_text("\n2 行目"));
        assert!(!is_valid_note_text("1 行目\r\n2 行目"));
    }

    /// A word that opens a preserved statement cannot also be a state id: the
    /// line would be read back as a preserved line, so the state the editor
    /// showed would be gone from the model on the next parse.
    #[test]
    fn a_word_that_is_read_as_a_preserved_statement_is_not_a_valid_id() {
        for word in [
            "hide",
            "scale",
            "click",
            "style",
            "class",
            "classDef",
            "accTitle",
            "accDescr",
            "state",
            "note",
            "direction",
            "A",
            "end",
            "graph",
            "Idle",
        ] {
            let source = format!("stateDiagram-v2\n  {word} --> Z");
            let Ok(document) = parse_state(&source) else {
                continue;
            };
            let swallowed = matches!(
                document.body.items.first(),
                Some(StateItem::Preserved(_)) | None
            );
            assert_ne!(
                swallowed,
                is_valid_state_id(word),
                "`{word} --> Z` read as {:?} while is_valid_state_id is {}",
                document.body.items,
                is_valid_state_id(word)
            );
        }
    }

    /// The depth limit is what keeps a pathological fence from recursing
    /// without bound, so it has to bite exactly where it says it does.
    #[test]
    fn a_composite_state_may_nest_up_to_the_limit_and_no_further() {
        let nested = |depth: usize| {
            let mut source = String::from("stateDiagram-v2\n");
            for level in 0..depth {
                let _ = writeln!(source, "{}state s{level} {{", INDENT.repeat(level + 1));
            }
            let _ = writeln!(source, "{}A --> B", INDENT.repeat(depth + 1));
            for level in (0..depth).rev() {
                let _ = writeln!(source, "{}}}", INDENT.repeat(level + 1));
            }
            source
        };
        assert!(parse_state(&nested(MAX_NESTING_DEPTH)).is_ok());
        let error = parse_state(&nested(MAX_NESTING_DEPTH + 1))
            .err()
            .expect("one level past the limit should be refused");
        assert!(format!("{error}").contains("入れ子"), "{error}");
        // Deep enough to overflow the stack if the limit did not hold.
        assert!(parse_state(&nested(400)).is_err());
    }

    #[test]
    fn unused_id_skips_taken_names() {
        let source = "stateDiagram-v2\n  N --> N1\n  state N2 {\n    N3 --> N4\n  }";
        let document = parse_state(source).expect("source should parse");
        assert_eq!(document.unused_id("N"), "N5");
        assert_eq!(document.unused_id("M"), "M");
    }

    #[test]
    fn crlf_and_trailing_whitespace_are_tolerated() {
        let source = "stateDiagram-v2\r\n  [*] --> A  \r\n  A --> [*]\r\n";
        let document = parse_state(source).expect("source should parse");
        assert_eq!(
            emit_state(&document),
            "stateDiagram-v2\n    [*] --> A\n    A --> [*]"
        );
    }

    #[test]
    fn preserved_lines_keep_their_side_of_the_diagram_keyword() {
        let source = "%%{init: {\"theme\": \"dark\"}}%%\nstateDiagram-v2\n  A --> B";
        let document = parse_state(source).expect("source should parse");
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
}
