//! A round-trippable editing model for Mermaid kanban boards.
//!
//! The shape of this module mirrors [`crate::mindmap_model`]: [`parse_kanban`]
//! reads the body of a ```` ```mermaid ```` fence into [`KanbanDocument`], and
//! [`emit_kanban`] writes it back. See [`crate::mermaid_model`] for the
//! invariants every diagram kind keeps.
//!
//! `merman` reads a board by indentation, but only one level of it counts: the
//! nodes indented like the first one are the columns, and every node indented
//! deeper belongs to the column above it, however deep it is. The model keeps
//! that reading — columns holding cards — and the emitter writes every column
//! and every card at one fixed width, which reads back the same.
//!
//! A card's details (`assigned`, `ticket`, `priority` and anything else) are
//! written as YAML after the node, `@{ … }`. The model keeps each value as the
//! source spells it and only rewrites the ones the form changes, so values it
//! does not edit — `icon`, `label`, keys it has never heard of — go back
//! untouched. Only YAML it can read back exactly is accepted: plain,
//! single-quoted and backslash-free double-quoted scalars.
//!
//! What this module refuses rather than rewrites, all of it with the reason in
//! Japanese:
//!
//! - a node written on the `kanban` line itself, and a node indented less than
//!   the first column, which `merman` either rejects or files under the wrong
//!   column;
//! - a `%%` comment at the end of a `::icon(…)` or `:::` line or of a node
//!   whose `@{` block runs over several lines (one at the end of any other
//!   node line stays on that line);
//! - a statement after a `%%{…}%%` directive on the same line, which `merman`
//!   reads where it is written;
//! - text after the closing `}` of a `@{ … }` block, and a `@{` with no node in
//!   front of it, both of which `merman` drops silently;
//! - YAML beyond the scalars above (nested maps, lists, block scalars, quoted
//!   values that run over a line);
//! - a `::icon(…)` or `:::class` line with nothing to attach to, a second one on
//!   the same node, and an icon given both ways;
//! - anything `merman` reads differently from this module, which is checked by
//!   asking it after every parse.
//!
//! What the parser rewrites, all of it stable from the first emit: the
//! indentation, the whitespace around `@{ … }` and between its entries, blank
//! lines inside a multi-line block, and comment lines between nodes, which move
//! below the board the way they do in a mindmap.

use anyhow::{Result, anyhow, bail};
use merman::diagram::RenderSemanticModel;

use crate::flowchart_model::{PreservedLine, PreservedPosition};
use crate::mermaid_model::{INDENT, first_statement_line, join_lines, keyword_tail, scan_preamble};

const HEADER: &str = "kanban";
const DECORATION_ICON_PREFIX: &str = "::icon(";

/// The metadata keys the form edits.
pub const ASSIGNED_KEY: &str = "assigned";
pub const TICKET_KEY: &str = "ticket";
pub const PRIORITY_KEY: &str = "priority";
const LABEL_KEY: &str = "label";
const ICON_KEY: &str = "icon";

/// The priorities `merman` draws with a colour of their own, with the name the
/// form shows. It compares them exactly, so `high` is drawn like no priority.
pub const KANBAN_PRIORITIES: [(&str, &str); 5] = [
    ("Very High", i18n::msg!("Very High")),
    ("High", i18n::msg!("High")),
    ("Medium", i18n::msg!("Medium")),
    ("Low", i18n::msg!("Low")),
    ("Very Low", i18n::msg!("Very Low")),
];

/// The brackets `merman` recognizes, in the order it tries them. A kanban board
/// draws every one of them as the same card, so they are only kept to write
/// the node back the way it was.
const DELIMITER_PAIRS: [(&str, &str); 8] = [
    ("(-", "-)"),
    ("-)", "(-"),
    ("((", "))"),
    ("))", "(("),
    ("{{", "}}"),
    ("[", "]"),
    (")", "("),
    ("(", ")"),
];

/// The brackets a node gets when it needs some and had none.
const SQUARE: (&str, &str) = ("[", "]");

/// One `key: value` entry of a node's `@{ … }` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KanbanMetadataEntry {
    pub key: String,
    /// The YAML scalar as the source spells it, quotes included.
    pub raw_value: String,
}

/// A column or a card. Both are written the same way; which one a node is
/// depends only on where it sits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KanbanNode {
    /// What `merman` identifies the node by: the text before the brackets, or
    /// the label itself when there is none.
    pub id: String,
    /// The text between the brackets, without the quotes around it, or the
    /// whole node when it has no brackets.
    pub label: String,
    /// `None` for a node written as bare text, which is its own id and label.
    pub brackets: Option<(&'static str, &'static str)>,
    /// Whether the id is written in front of the brackets. Without it the
    /// label doubles as the id.
    pub id_written: bool,
    /// Whether the label is written between `"` inside the brackets.
    pub quoted: bool,
    /// Whether the label is written as a Markdown string, `"` + backtick.
    pub markdown: bool,
    pub metadata: Vec<KanbanMetadataEntry>,
    /// Whether the `@{ … }` block is written over several lines, one entry per
    /// line. YAML reads commas differently in the two forms, so a block keeps
    /// the form it was written in.
    pub metadata_multiline: bool,
    /// The text of a `::icon(…)` line.
    pub icon: Option<String>,
    /// The text of a `:::` line.
    pub classes: Option<String>,
    /// The `%%` comment at the end of the node's line, which Mermaid drops
    /// and which is written back after the node.
    pub comment: Option<String>,
}

impl KanbanNode {
    /// A node written as `id[label]` with nothing else.
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            brackets: Some(SQUARE),
            id_written: true,
            quoted: false,
            markdown: false,
            metadata: Vec::new(),
            metadata_multiline: false,
            icon: None,
            classes: None,
            comment: None,
        }
    }

    /// The text drawn on the node: the `label` in its metadata when it has one,
    /// which `merman` puts in place of the text in the brackets.
    pub fn title(&self) -> String {
        self.metadata_value(LABEL_KEY)
            .unwrap_or_else(|| self.label.clone())
    }

    /// Changes the drawn text, wherever it is written.
    pub fn set_title(&mut self, title: &str) {
        if self.metadata_entry(LABEL_KEY).is_some() {
            self.set_metadata_value(LABEL_KEY, Some(title));
        } else {
            self.label = title.to_string();
        }
    }

    fn metadata_entry(&self, key: &str) -> Option<&KanbanMetadataEntry> {
        self.metadata.iter().find(|entry| entry.key == key)
    }

    /// The value of a metadata key the way `merman` reads it.
    pub fn metadata_value(&self, key: &str) -> Option<String> {
        self.metadata_entry(key)
            .and_then(|entry| decode_metadata_value(&entry.raw_value, !self.metadata_multiline))
    }

    /// Sets a metadata key, or removes it for `None` or an empty value. A value
    /// that reads the same as the one already there keeps its spelling.
    pub fn set_metadata_value(&mut self, key: &str, value: Option<&str>) {
        let Some(value) = value.filter(|value| !value.is_empty()) else {
            self.metadata.retain(|entry| entry.key != key);
            return;
        };
        if self.metadata_value(key).as_deref() == Some(value) {
            return;
        }
        let raw_value = encode_metadata_value(key, value);
        match self.metadata.iter_mut().find(|entry| entry.key == key) {
            Some(entry) => entry.raw_value = raw_value,
            None => self.metadata.push(KanbanMetadataEntry {
                key: key.to_string(),
                raw_value,
            }),
        }
    }

    /// Quotes the label when the brackets would otherwise cut it short, and
    /// drops the quotes when they are what makes it unreadable. A label that
    /// can be written as it is keeps the spelling it already had.
    pub fn fit_label_quoting(&mut self) {
        if self.markdown || spec_reads_back(self) {
            return;
        }
        let mut other = self.clone();
        other.quoted = !self.quoted;
        if spec_reads_back(&other) {
            self.quoted = other.quoted;
        }
    }

    /// The icon `merman` draws: a `::icon(…)` line wins over the metadata,
    /// because it is applied after it.
    fn drawn_icon(&self) -> Option<String> {
        self.icon.clone().or_else(|| self.metadata_value(ICON_KEY))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KanbanColumn {
    pub node: KanbanNode,
    pub cards: Vec<KanbanNode>,
}

/// An editable kanban board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KanbanDocument {
    pub columns: Vec<KanbanColumn>,
    pub preserved_lines: Vec<PreservedLine>,
}

impl KanbanDocument {
    /// A blank kanban board, used when the editor is opened on an empty fence.
    pub fn empty() -> Self {
        Self {
            columns: Vec::new(),
            preserved_lines: Vec::new(),
        }
    }

    /// Every node, columns and cards alike, in the order the fence writes them.
    pub fn nodes(&self) -> impl Iterator<Item = &KanbanNode> {
        self.columns
            .iter()
            .flat_map(|column| std::iter::once(&column.node).chain(column.cards.iter()))
    }

    /// An id no node has yet, `prefix` followed by the first free number.
    pub fn unused_id(&self, prefix: &str) -> String {
        (1..)
            .map(|number| format!("{prefix}{number}"))
            .find(|id| self.nodes().all(|node| node.id != *id))
            .unwrap_or_else(|| prefix.to_string())
    }

    /// Why the document cannot be written back as a diagram Mermaid accepts, in
    /// the display language, or `None` when it can.
    pub fn structure_error(&self) -> Option<String> {
        for column in &self.columns {
            if let Some(problem) =
                kanban_node_problem(&column.node, i18n::tc!("kanban node", "column"))
            {
                return Some(problem);
            }
            for card in &column.cards {
                if let Some(problem) = kanban_node_problem(card, i18n::tc!("kanban node", "card")) {
                    return Some(problem);
                }
            }
        }
        // `merman` files a card under its column by id, so two columns with one
        // id would each draw the cards of both.
        let mut seen: Vec<&str> = Vec::new();
        for node in self.nodes() {
            if seen.contains(&node.id.as_str()) {
                return Some(i18n::t!(
                    "The ID `{}` is used in two places. Columns and cards all need different IDs.",
                    node.id
                ));
            }
            seen.push(&node.id);
        }
        None
    }
}

/// Whether `source` is a kanban board: its first statement is `kanban`.
pub fn is_kanban_source(source: &str) -> bool {
    first_statement_line(source).is_some_and(|line| keyword_tail(line, HEADER).is_some())
}

/// Reads `source` — the body of a ```` ```mermaid ```` fence — into an editable
/// document.
pub fn parse_kanban(source: &str) -> Result<KanbanDocument> {
    let document = scan_kanban(source)?;
    // `merman` is asked last, so that a construct this module knows about is
    // reported with the reason that says what to do.
    if !source.trim().is_empty() {
        check_against_merman(source, &document)?;
    }
    Ok(document)
}

fn scan_kanban(source: &str) -> Result<KanbanDocument> {
    let lines = source.lines().collect::<Vec<_>>();
    let preamble = scan_preamble(&lines);
    let mut preserved_lines = preamble.preserved;

    let Some(header_index) = preamble.header else {
        return Ok(KanbanDocument {
            columns: Vec::new(),
            preserved_lines,
        });
    };
    let header = lines.get(header_index).copied().unwrap_or_default().trim();
    let Some(tail) = keyword_tail(header, HEADER) else {
        bail!(i18n::t!(
            "This is not a kanban board (only diagrams that start with `kanban` can be edited in the structured editor)."
        ));
    };
    if !tail.is_empty() {
        bail!(i18n::t!(
            "`{tail}` on the `kanban` line is not supported by the structured editor. Write columns on the next line."
        ));
    }

    let mut written: Vec<(usize, KanbanNode)> = Vec::new();
    let mut index = header_index + 1;
    let mut position = PreservedPosition::Top;
    while let Some(line) = lines.get(index).copied() {
        let start = index;
        index += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("%%") {
            if trimmed.starts_with("%%{") && !trimmed.contains("}%%") {
                while lines.get(index).is_some_and(|line| !line.contains("}%%")) {
                    index += 1;
                }
                if index < lines.len() {
                    index += 1;
                }
            }
            let text = join_lines(lines.get(start..index).unwrap_or_default());
            if let Some(statement) = statement_after_directive(&text) {
                bail!(i18n::t!(
                    "`{statement}` after `%%{{…}}%%` is not supported by the structured editor. Put the directive and the statement on separate lines, then open it again."
                ));
            }
            preserved_lines.push(PreservedLine { position, text });
            continue;
        }

        let statement = strip_inline_comment(line);
        let comment = line
            .get(statement.len()..)
            .unwrap_or_default()
            .trim_end()
            .to_string();
        let comment_unsupported = || {
            anyhow!(i18n::t!(
                "The structured editor cannot keep the comment at the end of this line ({trimmed}). Move the comment to a line of its own, or edit it as text."
            ))
        };
        // `merman` counts whitespace characters, so a tab or a full-width
        // space is one level of indentation like a space is.
        let rest = statement.trim_start();
        let indent = statement
            .get(..statement.len() - rest.len())
            .unwrap_or_default()
            .chars()
            .count();
        let rest = rest.trim_end();

        match classify_statement(rest)? {
            Statement::Icon(_) | Statement::Classes(_) if !comment.is_empty() => {
                return Err(comment_unsupported());
            }
            Statement::Icon(icon) => {
                let Some((_, node)) = written.last_mut() else {
                    bail!(i18n::t!("There is no node before `::icon(…)` ({trimmed})."));
                };
                if node.icon.is_some() || node.metadata_entry(ICON_KEY).is_some() {
                    bail!(i18n::t!(
                        "`{}` has two icons. Mermaid uses only the last one, so merge them into one, then open it again.",
                        node.id
                    ));
                }
                node.icon = Some(icon);
            }
            Statement::Classes(classes) => {
                let Some((_, node)) = written.last_mut() else {
                    bail!(i18n::t!("There is no node before `:::` ({trimmed})."));
                };
                if node.classes.is_some() {
                    bail!(i18n::t!(
                        "`{}` has `:::` twice. Mermaid uses only the last one, so merge them into one, then open it again.",
                        node.id
                    ));
                }
                node.classes = Some(classes);
            }
            Statement::Node {
                spec,
                metadata_opening,
            } => {
                let mut node = spec.into_node();
                if let Some(opening) = metadata_opening {
                    let (entries, multiline) = read_metadata(opening, &lines, &mut index)?;
                    node.metadata = entries;
                    node.metadata_multiline = multiline;
                }
                if !comment.is_empty() {
                    if node.metadata_multiline {
                        return Err(comment_unsupported());
                    }
                    node.comment = Some(comment);
                }
                if node.title().is_empty() {
                    bail!(i18n::t!("There is a node with an empty label ({trimmed})."));
                }
                written.push((indent, node));
                position = PreservedPosition::Bottom;
            }
        }
    }

    let mut columns: Vec<KanbanColumn> = Vec::new();
    let column_indent = written.first().map(|(indent, _)| *indent).unwrap_or(0);
    for (indent, node) in written {
        if indent < column_indent {
            bail!(i18n::t!(
                "`{}` is indented less than the first column. Indent columns as deep as the first column, and cards deeper than that.",
                node.id
            ));
        }
        if indent == column_indent {
            columns.push(KanbanColumn {
                node,
                cards: Vec::new(),
            });
        } else if let Some(column) = columns.last_mut() {
            column.cards.push(node);
        }
    }

    Ok(KanbanDocument {
        columns,
        preserved_lines,
    })
}

/// The statement written after a `%%{…}%%` directive on its closing line.
/// `merman` cuts out only the directive and reads what follows, so moving the
/// line with the comments would move that statement too.
pub(crate) fn statement_after_directive(text: &str) -> Option<&str> {
    if !text.trim_start().starts_with("%%{") {
        return None;
    }
    let end = text.find("}%%")?;
    let statement = text.get(end + 3..)?.trim();
    (!statement.is_empty()).then_some(statement)
}

/// Whether Mermaid rewrites `text` before a diagram's own parser reads it: it
/// cuts out `%%{…}%%` directives (and everything after one left open), turns
/// `#name;` into an entity, and changes `="…"` to `='…'` in what looks like an
/// HTML tag. A field holding such text would be drawn as something else and
/// could stop the line from being read at all.
pub(crate) fn mermaid_rewrites(text: &str) -> bool {
    if text.contains("%%{") || text.contains("=\"") {
        return true;
    }
    text.match_indices('#').any(|(offset, _)| {
        let name = text
            .get(offset + 1..)
            .unwrap_or_default()
            .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .next()
            .unwrap_or_default();
        !name.is_empty()
            && text
                .get(offset + 1 + name.len()..)
                .is_some_and(|rest| rest.starts_with(';'))
    })
}

/// Compares what `merman` reads out of `source` with what the model does, so
/// that a board the two read differently is refused instead of rewritten.
fn check_against_merman(source: &str, document: &KanbanDocument) -> Result<()> {
    let parsed = crate::parse_for_render_model(source)
        .map_err(|error| {
            anyhow!(i18n::t!(
                "Mermaid could not parse this diagram ({error}). Edit it as text."
            ))
        })?
        .ok_or_else(|| anyhow!(i18n::t!("Mermaid could not read this as a diagram.")))?;
    let RenderSemanticModel::Kanban(model) = parsed.into_parts().1 else {
        bail!(i18n::t!("This diagram is not a kanban board."));
    };

    // `merman` lists under each column every card filed under its id, so two
    // columns with one id each list the cards of both. The id check reports
    // that once the board is open; the comparison still has to hold for it.
    let mut expected = Vec::new();
    for column in &document.columns {
        expected.push(DrawnNode {
            id: column.node.id.clone(),
            label: column.node.title(),
            parent: None,
            ticket: column.node.metadata_value(TICKET_KEY),
            priority: None,
            assigned: None,
            icon: None,
        });
        let cards = document
            .columns
            .iter()
            .filter(|other| other.node.id == column.node.id)
            .flat_map(|other| other.cards.iter());
        for card in cards {
            expected.push(DrawnNode {
                id: card.id.clone(),
                label: card.title(),
                parent: Some(column.node.id.clone()),
                ticket: card.metadata_value(TICKET_KEY),
                priority: card.metadata_value(PRIORITY_KEY),
                assigned: card.metadata_value(ASSIGNED_KEY),
                icon: card.drawn_icon(),
            });
        }
    }
    let drawn = model
        .nodes
        .into_iter()
        .map(|node| DrawnNode {
            id: node.id,
            label: node.label,
            parent: node.parent_id,
            ticket: node.ticket,
            priority: node.priority,
            assigned: node.assigned,
            icon: node.icon,
        })
        .collect::<Vec<_>>();

    if drawn.len() != expected.len() {
        bail!(i18n::t!(
            "Mermaid read this kanban board as {} nodes, but the structured editor read {}. Edit it as text.",
            drawn.len(),
            expected.len()
        ));
    }
    for (drawn, expected) in drawn.iter().zip(&expected) {
        if !drawn.matches(expected) {
            bail!(i18n::t!(
                "Could not read `{}` the way Mermaid does. Edit it as text.",
                expected.id
            ));
        }
    }
    Ok(())
}

#[derive(Debug)]
struct DrawnNode {
    id: String,
    label: String,
    parent: Option<String>,
    ticket: Option<String>,
    priority: Option<String>,
    assigned: Option<String>,
    icon: Option<String>,
}

impl DrawnNode {
    /// `merman` passes ids and labels through its HTML sanitizer, which only
    /// touches text with markup in it; such text is compared by position alone.
    fn matches(&self, expected: &DrawnNode) -> bool {
        let text_matches =
            |drawn: &str, expected: &str| drawn == expected || expected.contains(['<', '&']);
        text_matches(&self.id, &expected.id)
            && text_matches(&self.label, &expected.label)
            && match (&self.parent, &expected.parent) {
                (Some(drawn), Some(expected)) => text_matches(drawn, expected),
                (None, None) => true,
                _ => false,
            }
            && self.ticket == expected.ticket
            && self.priority == expected.priority
            && self.assigned == expected.assigned
            && self.icon.is_some() == expected.icon.is_some()
    }
}

/// What one statement of a board body is, decided the way `merman` decides it.
enum Statement<'a> {
    Icon(String),
    Classes(String),
    Node {
        spec: NodeSpec,
        /// What follows `@{` on the node's line, when it has a metadata block.
        metadata_opening: Option<&'a str>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NodeSpec {
    id: String,
    label: String,
    brackets: Option<(&'static str, &'static str)>,
    id_written: bool,
    quoted: bool,
    markdown: bool,
}

impl NodeSpec {
    fn into_node(self) -> KanbanNode {
        KanbanNode {
            id: self.id,
            label: self.label,
            brackets: self.brackets,
            id_written: self.id_written,
            quoted: self.quoted,
            markdown: self.markdown,
            metadata: Vec::new(),
            metadata_multiline: false,
            icon: None,
            classes: None,
            comment: None,
        }
    }
}

/// Classifies a statement with its indentation and line-end comment removed.
fn classify_statement(rest: &str) -> Result<Statement<'_>> {
    if rest
        .get(..DECORATION_ICON_PREFIX.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(DECORATION_ICON_PREFIX))
    {
        let after = rest.get(DECORATION_ICON_PREFIX.len()..).unwrap_or_default();
        let Some(end) = after.find(')') else {
            bail!(i18n::t!(
                "`::icon(` has no closing parenthesis ({rest}). Mermaid skips this line, so close it or edit it as text."
            ));
        };
        let tail = after.get(end + 1..).unwrap_or_default().trim();
        if !tail.is_empty() {
            bail!(i18n::t!(
                "Mermaid skips `{tail}` after `::icon(…)` ({rest}). Delete it or edit it as text."
            ));
        }
        let icon = after.get(..end).unwrap_or_default().to_string();
        if !is_valid_kanban_icon(&icon) {
            bail!(i18n::t!(
                "The icon `{rest}` is not supported by the structured editor."
            ));
        }
        return Ok(Statement::Icon(icon));
    }
    if let Some(after) = rest.strip_prefix(":::") {
        let classes = after.trim().to_string();
        if !is_valid_kanban_classes(&classes) {
            bail!(i18n::t!(
                "`:::` without a class is not supported by the structured editor ({rest})."
            ));
        }
        return Ok(Statement::Classes(classes));
    }

    let (node_part, metadata_opening) = match find_metadata_start(rest) {
        Some(start) => (
            rest.get(..start).unwrap_or_default().trim_end(),
            Some(rest.get(start + 2..).unwrap_or_default()),
        ),
        None => (rest, None),
    };
    if node_part.trim().is_empty() {
        bail!(i18n::t!(
            "There is no node before `@{{` ({rest}). Mermaid skips this line, so edit it as text."
        ));
    }
    let spec = parse_node_spec(node_part).map_err(|error| match error {
        SpecError::Unterminated => anyhow!(i18n::t!(
            "The label of node `{node_part}` is not closed. Labels that span several lines are not supported by the structured editor. Edit it as text."
        )),
        SpecError::Malformed => anyhow!(i18n::t!(
            "Could not read this as a kanban node ({node_part}). Edit it as text."
        )),
    })?;
    Ok(Statement::Node {
        spec,
        metadata_opening,
    })
}

/// Reads the `@{ … }` block that opened with `opening` — the rest of the node's
/// line after `@{` — consuming the lines of a multi-line block from `index`.
fn read_metadata(
    opening: &str,
    lines: &[&str],
    index: &mut usize,
) -> Result<(Vec<KanbanMetadataEntry>, bool)> {
    if let Some(end) = find_closing_brace(opening) {
        let tail = opening.get(end + 1..).unwrap_or_default().trim();
        if !tail.is_empty() {
            bail!(i18n::t!(
                "Mermaid skips `{tail}` after `@{{ … }}`. Remove it or edit it as text."
            ));
        }
        let inner = opening.get(..end).unwrap_or_default();
        let mut entries = Vec::new();
        if inner.trim().is_empty() {
            return Ok((entries, false));
        }
        for part in split_flow_entries(inner)? {
            let part = part.trim();
            if part.is_empty() {
                bail!(i18n::t!("`@{{{inner}}}` has an empty entry."));
            }
            entries.push(read_metadata_entry(part, true)?);
        }
        return Ok((entries, false));
    }

    if !opening.trim().is_empty() {
        bail!(i18n::t!(
            "A multi-line `@{{` is supported only when its first line ends with `@{{` and the lines after it are written as `key: value`."
        ));
    }
    let mut entries = Vec::new();
    loop {
        let Some(line) = lines.get(*index).copied() else {
            bail!(i18n::t!("`@{{` is not closed with `}}`."));
        };
        *index += 1;
        let trimmed = line.trim();
        if trimmed == "}" {
            return Ok((entries, true));
        }
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.contains('}') {
            bail!(i18n::t!(
                "`{trimmed}` is not supported by the structured editor. Close a multi-line `@{{` with a line that is only `}}`."
            ));
        }
        entries.push(read_metadata_entry(trimmed, false)?);
    }
}

fn read_metadata_entry(text: &str, flow: bool) -> Result<KanbanMetadataEntry> {
    let unsupported = || {
        anyhow!(i18n::t!(
            "The metadata `{text}` is not supported by the structured editor (only `key: value`, with a single-line string as the value). Edit it as text."
        ))
    };
    let (key, value) = text.split_once(':').ok_or_else(unsupported)?;
    let key = key.trim_end();
    if !is_metadata_key(key) || !value.starts_with([' ', '\t']) {
        return Err(unsupported());
    }
    let raw_value = value.trim();
    if decode_metadata_value(raw_value, flow).is_none() {
        return Err(unsupported());
    }
    Ok(KanbanMetadataEntry {
        key: key.to_string(),
        raw_value: raw_value.to_string(),
    })
}

fn is_metadata_key(key: &str) -> bool {
    let mut characters = key.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '-'
        })
}

/// Splits the inside of a one-line `@{ … }` at its commas, leaving the ones in
/// quoted values alone.
fn split_flow_entries(inner: &str) -> Result<Vec<&str>> {
    let mut parts = Vec::new();
    let mut quote: Option<char> = None;
    let mut start = 0;
    for (offset, character) in inner.char_indices() {
        match quote {
            Some(open) if character == open => quote = None,
            Some(_) => {}
            None if character == '"' || character == '\'' => quote = Some(character),
            None if character == ',' => {
                parts.push(inner.get(start..offset).unwrap_or_default());
                start = offset + 1;
            }
            None => {}
        }
    }
    if quote.is_some() {
        bail!(i18n::t!("The quotes in `@{{{inner}}}` are not closed."));
    }
    parts.push(inner.get(start..).unwrap_or_default());
    Ok(parts)
}

/// The value a YAML scalar spells, or `None` for YAML this module does not
/// read. `flow` is whether it sits in a one-line `@{ … }`, where a comma ends
/// it.
pub fn decode_metadata_value(raw: &str, flow: bool) -> Option<String> {
    if raw.is_empty() || raw.contains(['\n', '\r']) {
        return None;
    }
    if let Some(inner) = raw.strip_prefix('\'') {
        let inner = inner.strip_suffix('\'')?;
        // Inside single quotes a quote is written twice; a lone one would end
        // the scalar early.
        let mut characters = inner.chars();
        let mut value = String::new();
        while let Some(character) = characters.next() {
            if character == '\'' && characters.next() != Some('\'') {
                return None;
            }
            value.push(character);
        }
        return Some(value);
    }
    if let Some(inner) = raw.strip_prefix('"') {
        let inner = inner.strip_suffix('"')?;
        if inner.contains(['"', '\\']) {
            return None;
        }
        return Some(inner.to_string());
    }
    let starts_like_syntax = raw.starts_with([
        '?', ':', ',', '[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '%', '@', '`',
    ]) || raw.starts_with("- ")
        || raw == "-";
    if starts_like_syntax
        || raw.contains(" #")
        || raw.contains(": ")
        || raw.ends_with(':')
        || raw.contains(['"', '\''])
        || (flow && raw.contains([',', '[', ']', '{', '}']))
    {
        return None;
    }
    Some(raw.to_string())
}

/// How a value the form was given is written: bare when YAML reads it back as
/// the same text, in single quotes otherwise. A priority is always quoted,
/// the way Mermaid's documentation writes it.
fn encode_metadata_value(key: &str, value: &str) -> String {
    let reads_as_other_type = matches!(
        value.to_ascii_lowercase().as_str(),
        "true" | "false" | "null" | "yes" | "no" | "on" | "off" | "y" | "n"
    );
    let mut characters = value.chars();
    let plain = key != PRIORITY_KEY
        && !reads_as_other_type
        && characters.next().is_some_and(char::is_alphabetic)
        && characters.all(|character| {
            character.is_alphanumeric() || matches!(character, ' ' | '.' | '_' | '/' | '-')
        })
        && !value.ends_with(' ');
    if plain {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "''"))
    }
}

/// The byte offset of the first `@{` outside a quoted string.
fn find_metadata_start(text: &str) -> Option<usize> {
    let mut in_quote = false;
    let mut in_backtick_quote = false;
    let mut characters = text.char_indices().peekable();
    while let Some((offset, character)) = characters.next() {
        if in_backtick_quote {
            if character == '`' && characters.peek().is_some_and(|(_, next)| *next == '"') {
                in_backtick_quote = false;
                characters.next();
            }
            continue;
        }
        if in_quote {
            if character == '"' {
                in_quote = false;
            }
            continue;
        }
        if character == '"' {
            if characters.peek().is_some_and(|(_, next)| *next == '`') {
                in_backtick_quote = true;
                characters.next();
                continue;
            }
            in_quote = true;
            continue;
        }
        if character == '@' && characters.peek().is_some_and(|(_, next)| *next == '{') {
            return Some(offset);
        }
    }
    None
}

/// The byte offset of the first `}` outside a `"` string, which is where
/// `merman` ends a metadata block. It does not know single quotes.
fn find_closing_brace(text: &str) -> Option<usize> {
    let mut in_quote = false;
    for (offset, character) in text.char_indices() {
        if character == '"' {
            in_quote = !in_quote;
        } else if character == '}' && !in_quote {
            return Some(offset);
        }
    }
    None
}

/// Cuts `line` at the first `%%` outside a quoted string, the way `merman`
/// does before it reads a statement.
fn strip_inline_comment(line: &str) -> &str {
    let mut in_quote = false;
    let mut in_backtick_quote = false;
    let mut characters = line.char_indices().peekable();
    while let Some((offset, character)) = characters.next() {
        if in_backtick_quote {
            if character == '`' && characters.peek().is_some_and(|(_, next)| *next == '"') {
                in_backtick_quote = false;
                characters.next();
            }
            continue;
        }
        if in_quote {
            if character == '"' {
                in_quote = false;
            }
            continue;
        }
        if character == '"' {
            if characters.peek().is_some_and(|(_, next)| *next == '`') {
                in_backtick_quote = true;
                characters.next();
                continue;
            }
            in_quote = true;
            continue;
        }
        if character == '%' && characters.peek().is_some_and(|(_, next)| *next == '%') {
            return line.get(..offset).unwrap_or_default();
        }
    }
    line
}

enum SpecError {
    Unterminated,
    Malformed,
}

fn delimiter_pair_at_start(input: &str) -> Option<(&'static str, &'static str)> {
    DELIMITER_PAIRS
        .into_iter()
        .find(|(start, _)| input.starts_with(start))
}

/// Reads a node the way `merman`'s `parse_node_spec` does.
fn parse_node_spec(input: &str) -> Result<NodeSpec, SpecError> {
    let input = input.trim_end();
    if input.is_empty() {
        return Err(SpecError::Malformed);
    }

    if let Some(brackets) = delimiter_pair_at_start(input) {
        let (inner, tail) = extract_delimited(input, brackets)?;
        if !tail.trim().is_empty() {
            return Err(SpecError::Malformed);
        }
        let (label, quoted, markdown) = unquote(inner);
        return Ok(NodeSpec {
            id: label.clone(),
            label,
            brackets: Some(brackets),
            id_written: false,
            quoted,
            markdown,
        });
    }

    let split = input.find(['(', ')', '[', '{', '}']).unwrap_or(input.len());
    let id = input.get(..split).unwrap_or_default();
    let rest = input.get(split..).unwrap_or_default().trim_end();
    if rest.is_empty() {
        return Ok(NodeSpec {
            id: input.to_string(),
            label: input.to_string(),
            brackets: None,
            id_written: false,
            quoted: false,
            markdown: false,
        });
    }

    let Some(brackets) = delimiter_pair_at_start(rest) else {
        return Err(SpecError::Malformed);
    };
    let (inner, tail) = extract_delimited(rest, brackets)?;
    if !tail.trim().is_empty() {
        return Err(SpecError::Malformed);
    }
    let (label, quoted, markdown) = unquote(inner);
    Ok(NodeSpec {
        id: id.to_string(),
        label,
        brackets: Some(brackets),
        id_written: true,
        quoted,
        markdown,
    })
}

/// The text between the opening bracket and the first closing one outside a
/// quoted string, and what follows it.
fn extract_delimited<'a>(
    input: &'a str,
    (start, end): (&str, &str),
) -> Result<(&'a str, &'a str), SpecError> {
    let body = input.get(start.len()..).ok_or(SpecError::Malformed)?;
    let mut in_quote = false;
    let mut in_backtick_quote = false;
    let mut characters = body.char_indices().peekable();
    while let Some((offset, character)) = characters.next() {
        if in_backtick_quote {
            if character == '`' && characters.peek().is_some_and(|(_, next)| *next == '"') {
                in_backtick_quote = false;
                characters.next();
            }
            continue;
        }
        if in_quote {
            if character == '"' {
                in_quote = false;
            }
            continue;
        }
        if character == '"' {
            if characters.peek().is_some_and(|(_, next)| *next == '`') {
                in_backtick_quote = true;
                characters.next();
                continue;
            }
            in_quote = true;
            continue;
        }
        let here = body.get(offset..).unwrap_or_default();
        if here.starts_with(end) {
            let inner = body.get(..offset).unwrap_or_default();
            let tail = here.get(end.len()..).unwrap_or_default();
            return Ok((inner, tail));
        }
    }
    Err(SpecError::Unterminated)
}

/// The label inside the brackets, and whether it was written as a quoted or a
/// Markdown string.
fn unquote(raw: &str) -> (String, bool, bool) {
    if let Some(inner) = raw
        .strip_prefix("\"`")
        .and_then(|inner| inner.strip_suffix("`\""))
    {
        return (inner.to_string(), false, true);
    }
    if let Some(inner) = raw
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
    {
        return (inner.to_string(), true, false);
    }
    (raw.to_string(), false, false)
}

/// The node without its metadata, the way it is written on its line.
fn emit_node_spec(node: &KanbanNode) -> String {
    if node.brackets.is_none() && node.id == node.label {
        return node.label.clone();
    }
    let (open, close) = node.brackets.unwrap_or(SQUARE);
    let label = if node.markdown {
        format!("\"`{}`\"", node.label)
    } else if node.quoted {
        format!("\"{}\"", node.label)
    } else {
        node.label.clone()
    };
    // A node written without an id takes its label as one, so the id is left
    // out for as long as that still gives the right id.
    if node.brackets.is_some() && !node.id_written && node.id == node.label {
        format!("{open}{label}{close}")
    } else {
        format!("{}{open}{label}{close}", node.id)
    }
}

/// The node and its one-line `@{ … }` block, without the comment.
fn emit_one_line_node(node: &KanbanNode) -> String {
    let mut line = emit_node_spec(node);
    if !node.metadata.is_empty() {
        let entries = node
            .metadata
            .iter()
            .map(|entry| format!("{}: {}", entry.key, entry.raw_value))
            .collect::<Vec<_>>();
        line.push_str("@{ ");
        line.push_str(&entries.join(", "));
        line.push_str(" }");
    }
    line
}

/// Whether the node's comment is still cut off as a comment, and only it,
/// once it follows the node's line as written now.
fn comment_reads_back(node: &KanbanNode) -> bool {
    let Some(comment) = &node.comment else {
        return true;
    };
    if node.metadata_multiline && !node.metadata.is_empty() {
        return false;
    }
    let statement = emit_one_line_node(node);
    let line = format!("{statement} {comment}");
    comment.starts_with("%%")
        && !comment.contains(['\n', '\r'])
        && !comment.contains("%%{")
        && strip_inline_comment(&line) == format!("{statement} ")
}

fn emit_node(out: &mut String, node: &KanbanNode, indent: &str) {
    out.push_str(indent);
    if node.metadata_multiline && !node.metadata.is_empty() {
        out.push_str(&emit_node_spec(node));
        out.push_str("@{\n");
        for entry in &node.metadata {
            out.push_str(indent);
            out.push_str(INDENT);
            out.push_str(&entry.key);
            out.push_str(": ");
            out.push_str(&entry.raw_value);
            out.push('\n');
        }
        out.push_str(indent);
        out.push('}');
    } else {
        out.push_str(&emit_one_line_node(node));
        if let Some(comment) = &node.comment {
            out.push(' ');
            out.push_str(comment);
        }
    }
    out.push('\n');
    // The decoration lines attach to the node above them whatever their
    // indentation; writing them at the node's own level is how Mermaid's
    // documentation writes them.
    if let Some(icon) = &node.icon {
        out.push_str(indent);
        out.push_str(DECORATION_ICON_PREFIX);
        out.push_str(icon);
        out.push_str(")\n");
    }
    if let Some(classes) = &node.classes {
        out.push_str(indent);
        out.push_str(":::");
        out.push_str(classes);
        out.push('\n');
    }
}

/// Writes the document back as Mermaid source, without a trailing newline.
pub fn emit_kanban(document: &KanbanDocument) -> String {
    let mut out = String::new();
    for line in preserved_at(document, PreservedPosition::Preamble) {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(HEADER);
    out.push('\n');
    for line in preserved_at(document, PreservedPosition::Top) {
        out.push_str(line);
        out.push('\n');
    }
    let card_indent = INDENT.repeat(2);
    for column in &document.columns {
        emit_node(&mut out, &column.node, INDENT);
        for card in &column.cards {
            emit_node(&mut out, card, &card_indent);
        }
    }
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
    document: &KanbanDocument,
    position: PreservedPosition,
) -> impl Iterator<Item = &str> {
    document
        .preserved_lines
        .iter()
        .filter(move |line| line.position == position)
        .map(|line| line.text.as_str())
}

/// Whether the node's line reads back as the same id and label. Which texts
/// can be written depends on the brackets, on the quoting and on how `merman`
/// scans for comments, metadata and decoration lines, so rather than repeat
/// those rules the line is written and read back.
fn spec_reads_back(node: &KanbanNode) -> bool {
    let line = emit_node_spec(node);
    if strip_inline_comment(&line).len() != line.len() || line.trim() != line {
        return false;
    }
    match classify_statement(&line) {
        Ok(Statement::Node {
            spec,
            metadata_opening: None,
        }) => spec.id == node.id && spec.label == node.label,
        _ => false,
    }
}

/// Whether `id` can be written in front of a node's brackets.
pub fn is_valid_kanban_id(id: &str) -> bool {
    !id.is_empty()
        && id.trim() == id
        && !id.contains(['(', ')', '[', ']', '{', '}', '"', '\n', '\r'])
        && !id.contains("%%")
        && !id.contains("@{")
        && !mermaid_rewrites(id)
        && !id.starts_with(":::")
        && !id
            .get(..DECORATION_ICON_PREFIX.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(DECORATION_ICON_PREFIX))
}

/// Whether the node's title can be written the way the node spells it.
pub fn is_valid_kanban_title(node: &KanbanNode) -> bool {
    let title = node.title();
    if title.trim().is_empty() || title.contains(['\n', '\r']) || mermaid_rewrites(&title) {
        return false;
    }
    let label_entry_writable = node
        .metadata
        .iter()
        .filter(|entry| entry.key == LABEL_KEY)
        .all(|entry| is_writable_metadata_entry(entry, node.metadata_multiline));
    label_entry_writable && spec_reads_back(node)
}

/// Whether a value typed into one of the form's metadata fields can be
/// written. `merman` ends the block at the first `}` and follows only double
/// quotes while it looks for it, and it cuts the line at `%%`.
pub fn is_valid_kanban_metadata_value(value: &str) -> bool {
    value.is_empty()
        || (!value.contains(['\n', '\r', '}', '"'])
            && !value.contains("%%")
            && !mermaid_rewrites(value)
            && value.trim() == value)
}

fn is_writable_metadata_entry(entry: &KanbanMetadataEntry, multiline: bool) -> bool {
    decode_metadata_value(&entry.raw_value, !multiline)
        .is_some_and(|value| is_valid_kanban_metadata_value(&value))
}

/// Whether `icon` can be written inside `::icon(…)`.
pub fn is_valid_kanban_icon(icon: &str) -> bool {
    !icon.trim().is_empty() && !icon.contains([')', '\n', '\r'])
}

/// Whether `classes` can be written after `:::`.
pub fn is_valid_kanban_classes(classes: &str) -> bool {
    !classes.trim().is_empty() && classes.trim() == classes && !classes.contains(['\n', '\r'])
}

/// The first thing wrong with one node, in Japanese. `what` names the kind of
/// node, 列 or カード.
pub fn kanban_node_problem(node: &KanbanNode, what: &str) -> Option<String> {
    if node.id.is_empty() {
        return Some(i18n::t!("A {what} has an empty ID."));
    }
    if !is_valid_kanban_id(&node.id) {
        return Some(i18n::t!(
            "The {what} ID `{}` cannot be used (no brackets, `\"`, `%%`, `@{{`, `#…;` or whitespace at either end).",
            node.id
        ));
    }
    let title = node.title();
    if title.trim().is_empty() {
        return Some(i18n::t!("The title of {what} `{}` is empty.", node.id));
    }
    if !is_valid_kanban_title(node) {
        return Some(i18n::t!(
            "The title `{title}` of {what} `{}` cannot be written (no line breaks, `%%{{`, `=\"` or `#…;`, and closing brackets cannot be used together with `\"`).",
            node.id
        ));
    }
    for entry in &node.metadata {
        if !is_writable_metadata_entry(entry, node.metadata_multiline) {
            return Some(i18n::t!(
                "The {what} `{}` has a value for `{}` that cannot be written (no `}}`, `\"`, `%%`, `#…;`, line breaks or whitespace at either end).",
                node.id,
                entry.key
            ));
        }
    }
    if !comment_reads_back(node) {
        return Some(i18n::t!(
            "The comment at the end of the line of {what} `{}` cannot be kept with the line written this way (use `\"` in the title in pairs).",
            node.id
        ));
    }
    if let Some(icon) = &node.icon
        && !is_valid_kanban_icon(icon)
    {
        return Some(i18n::t!(
            "The icon `{icon}` of {what} `{}` cannot be written.",
            node.id
        ));
    }
    if let Some(classes) = &node.classes
        && !is_valid_kanban_classes(classes)
    {
        return Some(i18n::t!(
            "The class `{classes}` of {what} `{}` cannot be written.",
            node.id
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MermaidTheme, render_to_svg};
    use merman::{Engine, ParseOptions};

    const TEMPLATE: &str = "kanban\n    %% 字下げの浅い行が列、深い行がカード\n    todo[未着手]\n        spec[仕様を書く]\n        screen[画面を作る]\n    doing[作業中]\n        api[API を作る]\n    done[完了]\n        setup[環境を整える]";

    fn parse(source: &str) -> KanbanDocument {
        parse_kanban(source).unwrap_or_else(|error| panic!("{source} should parse: {error}"))
    }

    fn refusal(source: &str) -> String {
        match parse_kanban(source) {
            Ok(document) => panic!("{source} should be refused, got {document:?}"),
            Err(error) => error.to_string(),
        }
    }

    /// Emitting must reach a fixed point after one pass, and the text is what
    /// is compared, so a line that vanished from both documents still fails.
    fn assert_stable(source: &str) -> String {
        let first = parse(source);
        let first_text = emit_kanban(&first);
        let second = parse(&first_text);
        let second_text = emit_kanban(&second);
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

    fn assert_renders(source: &str) {
        let svg = render_to_svg(source, &MermaidTheme::default())
            .unwrap_or_else(|error| panic!("emitted source should render: {error}\n{source}"));
        assert!(
            !svg.contains("Syntax error in text"),
            "emitted source rendered an error diagram:\n{source}"
        );
    }

    fn board(document: &KanbanDocument) -> Vec<(String, Vec<String>)> {
        document
            .columns
            .iter()
            .map(|column| {
                (
                    column.node.id.clone(),
                    column.cards.iter().map(|card| card.id.clone()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn the_insert_template_round_trips_byte_for_byte() {
        let document = parse(TEMPLATE);
        assert_eq!(
            board(&document),
            vec![
                (
                    "todo".to_string(),
                    vec!["spec".to_string(), "screen".to_string()]
                ),
                ("doing".to_string(), vec!["api".to_string()]),
                ("done".to_string(), vec!["setup".to_string()]),
            ]
        );
        assert_eq!(
            document.columns.first().map(|column| column.node.title()),
            Some("未着手".to_string())
        );
        assert_eq!(document.structure_error(), None);
        assert_eq!(emit_kanban(&document), TEMPLATE);
        assert_renders(TEMPLATE);
    }

    #[test]
    fn metadata_is_read_and_written_back_as_it_was_spelled() {
        let source = "kanban\n    id10[Ready for test]\n        id4[Create parsing tests]@{ ticket: MC-2038, assigned: 'K.Sveidqvist', priority: 'High' }\n        id66[last item]@{ priority: 'Very Low', assigned: \"knsv\", icon: star, descr: 'it''s, done' }";
        let document = parse(source);
        let card = document
            .columns
            .first()
            .and_then(|column| column.cards.first())
            .expect("the first card is there");
        assert_eq!(card.metadata_value(TICKET_KEY).as_deref(), Some("MC-2038"));
        assert_eq!(
            card.metadata_value(ASSIGNED_KEY).as_deref(),
            Some("K.Sveidqvist")
        );
        assert_eq!(card.metadata_value(PRIORITY_KEY).as_deref(), Some("High"));
        let last = document
            .columns
            .first()
            .and_then(|column| column.cards.get(1))
            .expect("the second card is there");
        assert_eq!(last.metadata_value("descr").as_deref(), Some("it's, done"));
        assert_eq!(assert_stable(source), source);
        assert_renders(source);
    }

    #[test]
    fn changing_one_value_leaves_the_others_as_they_were_spelled() {
        let source =
            "kanban\n    todo[Todo]\n        a[A]@{ ticket: MC-1, assigned: \"knsv\", icon: star }";
        let mut document = parse(source);
        let card = document
            .columns
            .get_mut(0)
            .and_then(|column| column.cards.get_mut(0))
            .expect("the card is there");
        card.set_metadata_value(TICKET_KEY, Some("MC-1"));
        card.set_metadata_value(ASSIGNED_KEY, Some("山田 太郎"));
        card.set_metadata_value(PRIORITY_KEY, Some("Very High"));
        let emitted = emit_kanban(&document);
        assert_eq!(
            emitted,
            "kanban\n    todo[Todo]\n        a[A]@{ ticket: MC-1, assigned: 山田 太郎, icon: star, priority: 'Very High' }"
        );
        assert_eq!(assert_stable(&emitted), emitted);
        assert_renders(&emitted);

        let card = document
            .columns
            .get_mut(0)
            .and_then(|column| column.cards.get_mut(0))
            .expect("the card is there");
        card.set_metadata_value(ASSIGNED_KEY, Some("it's true"));
        card.set_metadata_value(TICKET_KEY, Some("true"));
        let emitted = emit_kanban(&document);
        assert!(
            emitted.contains("assigned: 'it''s true'") && emitted.contains("ticket: 'true'"),
            "values YAML would read differently are quoted:\n{emitted}"
        );
        assert_eq!(parse(&emitted), document);

        let card = document
            .columns
            .get_mut(0)
            .and_then(|column| column.cards.get_mut(0))
            .expect("the card is there");
        for key in [TICKET_KEY, ASSIGNED_KEY, PRIORITY_KEY, ICON_KEY] {
            card.set_metadata_value(key, None);
        }
        assert_eq!(
            emit_kanban(&document),
            "kanban\n    todo[Todo]\n        a[A]"
        );
    }

    #[test]
    fn a_multi_line_block_stays_multi_line() {
        let source = "kanban\n  id1[Todo]\n    id2[Create JISON]\n\n  id2@{\n    assigned: knsv\n    icon: heart\n\n    priority: high\n    descr: \"Lorem ipsum, dolor sit amet\"\n  }";
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "kanban\n    id1[Todo]\n        id2[Create JISON]\n    id2@{\n        assigned: knsv\n        icon: heart\n        priority: high\n        descr: \"Lorem ipsum, dolor sit amet\"\n    }"
        );
        let document = parse(source);
        // `id2` is written twice, once as a card and once as a column.
        assert!(
            document
                .structure_error()
                .is_some_and(|error| error.contains("`id2`")),
            "{:?}",
            document.structure_error()
        );
        assert_renders(&emitted);
    }

    #[test]
    fn deeper_levels_become_cards_of_their_column() {
        // `merman` does not tell a card's children from its siblings.
        let source = "kanban\nroot\n      child1\n        leaf1\n   child2\nnext\n\tx";
        let document = parse(source);
        assert_eq!(
            board(&document),
            vec![
                (
                    "root".to_string(),
                    vec![
                        "child1".to_string(),
                        "leaf1".to_string(),
                        "child2".to_string()
                    ]
                ),
                ("next".to_string(), vec!["x".to_string()]),
            ]
        );
        assert_eq!(
            assert_stable(source),
            "kanban\n    root\n        child1\n        leaf1\n        child2\n    next\n        x"
        );
    }

    #[test]
    fn every_spelling_of_a_node_survives() {
        let source = "kanban\n    Todo\n        [Create Documentation]\n        docs(Blog)\n        c((円))\n        d{{六角形}}\n        e(-奇妙-)\n        q[\"括弧 [] 入り\"]\n        m[\"`**太字**`\"]\n        plain text\n        ::icon(bomb)\n        :::hot cold";
        let emitted = assert_stable(source);
        assert_eq!(emitted, source);
        let document = parse(source);
        let cards = &document.columns.first().expect("one column").cards;
        let first = cards.first().expect("a card");
        assert_eq!(first.id, "Create Documentation");
        assert!(!first.id_written);
        let plain = cards.last().expect("the plain card");
        assert_eq!(plain.icon.as_deref(), Some("bomb"));
        assert_eq!(plain.classes.as_deref(), Some("hot cold"));
        assert_renders(&emitted);
    }

    #[test]
    fn renaming_keeps_the_cards_under_their_column_and_writes_the_id() {
        let mut document = parse(TEMPLATE);
        let column = document.columns.get_mut(0).expect("a column");
        column.node.id = "backlog".to_string();
        let card = column.cards.get_mut(0).expect("a card");
        card.set_title("仕様 [草案] を書く");
        card.fit_label_quoting();
        let emitted = emit_kanban(&document);
        assert!(
            emitted.contains("    backlog[未着手]\n        spec[\"仕様 [草案] を書く\"]\n"),
            "{emitted}"
        );
        assert_eq!(document.structure_error(), None);
        assert_eq!(parse(&emitted), document);
        assert_renders(&emitted);

        // A node written as bare text gets brackets once its id and its label
        // part ways, and one written without an id gets its id written.
        let mut document = parse("kanban\n    Todo\n        [Write]");
        let column = document.columns.get_mut(0).expect("a column");
        column.node.set_title("やること");
        let card = column.cards.get_mut(0).expect("a card");
        card.id = "write".to_string();
        let emitted = emit_kanban(&document);
        assert_eq!(emitted, "kanban\n    Todo[やること]\n        write[Write]");
        assert_eq!(assert_stable(&emitted), emitted);
    }

    #[test]
    fn the_title_follows_a_label_in_the_metadata() {
        let source = "kanban\n    todo[Todo]\n        a[old]@{ icon: star, label: 'fix things' }";
        let mut document = parse(source);
        let card = document
            .columns
            .get_mut(0)
            .and_then(|column| column.cards.get_mut(0))
            .expect("the card is there");
        assert_eq!(card.title(), "fix things");
        card.set_title("直す");
        let emitted = emit_kanban(&document);
        assert_eq!(
            emitted,
            "kanban\n    todo[Todo]\n        a[old]@{ icon: star, label: 直す }"
        );
        assert_renders(&emitted);
        assert_eq!(parse(&emitted), document);
    }

    #[test]
    fn comments_and_the_preamble_keep_their_side_of_the_board() {
        let source = "---\nconfig:\n  kanban:\n    ticketBaseUrl: 'https://example.com/browse/#TICKET#'\n---\nkanban\n  %% 先頭のメモ\n  todo[Todo]\n    %% 途中のメモ\n    a[A]@{ ticket: MC-1 }";
        let document = parse(source);
        let positions = document
            .preserved_lines
            .iter()
            .map(|line| (line.position, line.text.trim().to_string()))
            .collect::<Vec<_>>();
        assert_eq!(positions.len(), 3);
        assert_eq!(
            positions.get(1),
            Some(&(PreservedPosition::Top, "%% 先頭のメモ".to_string()))
        );
        assert_eq!(
            positions.get(2),
            Some(&(PreservedPosition::Bottom, "%% 途中のメモ".to_string()))
        );
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "---\nconfig:\n  kanban:\n    ticketBaseUrl: 'https://example.com/browse/#TICKET#'\n---\nkanban\n  %% 先頭のメモ\n    todo[Todo]\n        a[A]@{ ticket: MC-1 }\n    %% 途中のメモ"
        );
        assert_renders(&emitted);
    }

    #[test]
    fn what_mermaid_would_drop_or_misread_is_refused() {
        for (source, reason) in [
            ("kanban root\n    a", "on the `kanban` line"),
            ("kanban\n    a\n  b", "indented less than the first column"),
            (
                "kanban\n    a\n        b\n        :::hot %% メモ",
                "comment at the end of this line",
            ),
            (
                "kanban\n    a\n        b@{ %% メモ\n          icon: star\n        }",
                "comment at the end of this line",
            ),
            ("kanban\n    a\n        b@{ ticket: 1 } x", "Mermaid skips"),
            ("kanban\n    a\n    @{ ticket: 1 }", "no node before `@{`"),
            (
                "kanban\n    a\n        b@{ tags: [x, y] }",
                "not supported by the structured editor",
            ),
            (
                "kanban\n    a\n        b@{ label: \"line1\n      line2\" }",
                "first line",
            ),
            (
                "kanban\n    a\n        b@{\n          icon: star\n",
                "is not closed with",
            ),
            ("kanban\n    a\n        ::icon(bomb", "closing parenthesis"),
            ("kanban\n    ::icon(bomb)\n    a", "no node before"),
            (
                "kanban\n    a\n    ::icon(x)\n    ::icon(y)",
                "Mermaid uses only the last one",
            ),
            (
                "kanban\n    a@{ icon: x }\n    ::icon(y)",
                "Mermaid uses only the last one",
            ),
            ("kanban\n    a\n        b[unclosed", "is not closed"),
            ("kanban\n    a\n        b[x]y", "Could not read"),
            ("kanban\n    a\n        b[]", "empty label"),
            // YAML reads `1.50` as the number 1.5, which is what is drawn.
            (
                "kanban\n    a\n        b@{ ticket: 1.50 }",
                "the way Mermaid does",
            ),
        ] {
            let message = refusal(source);
            assert!(
                message.contains(reason),
                "{source:?} should be refused for {reason:?}, got {message}"
            );
        }
    }

    #[test]
    fn the_structure_check_catches_what_cannot_be_written() {
        let mut document = parse(TEMPLATE);
        let column = document.columns.get_mut(1).expect("a column");
        column.node.id = "todo".to_string();
        assert!(
            document
                .structure_error()
                .is_some_and(|error| error.contains("`todo` is used in two places")),
        );

        for (edit, reason) in [
            (
                (|node: &mut KanbanNode| node.id = "a[b".to_string()) as fn(&mut KanbanNode),
                "ID `a[b`",
            ),
            (|node| node.id = String::new(), "empty ID"),
            (
                |node| node.set_title(" "),
                "The title of card `spec` is empty",
            ),
            (|node| node.set_title("a ] \" b"), "cannot be written"),
            (
                |node| node.set_metadata_value(ASSIGNED_KEY, Some("a}b")),
                "value for `assigned`",
            ),
        ] {
            let mut document = parse(TEMPLATE);
            let card = document
                .columns
                .get_mut(0)
                .and_then(|column| column.cards.get_mut(0))
                .expect("a card");
            edit(card);
            card.fit_label_quoting();
            let error = document.structure_error();
            assert!(
                error.as_deref().is_some_and(|error| error.contains(reason)),
                "expected {reason:?}, got {error:?}"
            );
        }

        // Quoting makes a closing bracket writable.
        let mut node = KanbanNode::new("a", "x ] y");
        assert!(!is_valid_kanban_title(&node));
        node.fit_label_quoting();
        assert!(node.quoted && is_valid_kanban_title(&node));
    }

    #[test]
    fn a_statement_after_a_directive_is_refused_rather_than_moved() {
        // `merman` reads the `:::hot` as `todo`'s, and moving the line below
        // `doing` would give it to `doing`.
        for source in [
            "kanban\n    todo[Todo]\n    %%{init: {}}%% :::hot\n    doing[Doing]",
            "kanban\n    todo[Todo]\n    %%{init:\n  {}}%% ::icon(x)\n    doing[Doing]",
        ] {
            let message = refusal(source);
            assert!(
                message.contains("directive and the statement on separate lines"),
                "{source:?}: {message}"
            );
        }
        let source = "kanban\n    todo[Todo]\n    %%{init: {}}%%\n    doing[Doing]";
        assert_eq!(parse(source).preserved_lines.len(), 1);
    }

    #[test]
    fn a_comment_at_the_end_of_a_node_line_stays_on_that_line() {
        let source = "kanban\n  root(Root) %% 列のメモ\n    a(a)   %% カードのメモ\n    b[x]@{ ticket: MC-1 } %% \"引用\" のメモ\n    c[\"%% ではない\"]";
        let document = parse(source);
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "kanban\n    root(Root) %% 列のメモ\n        a(a) %% カードのメモ\n        b[x]@{ ticket: MC-1 } %% \"引用\" のメモ\n        c[\"%% ではない\"]"
        );
        assert_renders(&emitted);

        // Moving a card takes its comment along.
        let mut moved = document;
        let column = moved.columns.get_mut(0).expect("a column");
        column.cards.swap(0, 1);
        let emitted = emit_kanban(&moved);
        assert!(
            emitted.contains(
                "        b[x]@{ ticket: MC-1 } %% \"引用\" のメモ\n        a(a) %% カードのメモ\n"
            ),
            "{emitted}"
        );
        assert_eq!(parse(&emitted), moved);

        // A multi-line block would put the comment after its `}`, where
        // Mermaid does not look.
        let mut node = KanbanNode::new("a", "b");
        node.comment = Some("%% メモ".to_string());
        assert_eq!(kanban_node_problem(&node, "card"), None);
        node.set_metadata_value(TICKET_KEY, Some("MC-1"));
        node.metadata_multiline = true;
        assert!(
            kanban_node_problem(&node, "card")
                .is_some_and(|error| error.contains("comment at the end of the line"))
        );
    }

    #[test]
    fn columns_sharing_an_id_are_still_compared_with_merman() {
        let source =
            "kanban\n    todo[Todo]\n        a[x]\n    todo[Again]\n        b[y]@{ ticket: MC-1 }";
        let document = parse(source);
        assert!(
            document
                .structure_error()
                .is_some_and(|error| error.contains("`todo` is used in two places"))
        );
        assert_eq!(emit_kanban(&document), source);

        let message = refusal(
            "kanban\n    todo[Todo]\n        a[x]\n    todo[Again]\n        b[y]@{ ticket: 1.50 }",
        );
        assert!(message.contains("the way Mermaid does"), "{message}");
    }

    #[test]
    fn text_mermaid_rewrites_before_parsing_cannot_be_typed() {
        for title in ["a %%{ b", "<i x=\"1]\">", "a#35;b", "#quot;"] {
            let mut document = parse(TEMPLATE);
            let card = document
                .columns
                .get_mut(0)
                .and_then(|column| column.cards.get_mut(0))
                .expect("a card");
            card.set_title(title);
            card.fit_label_quoting();
            assert!(!is_valid_kanban_title(card), "{title:?}");
            assert!(
                document
                    .structure_error()
                    .is_some_and(|error| error.contains("The title")),
                "{title:?} should be refused, wrote:\n{}",
                emit_kanban(&document)
            );
        }
        for value in ["a#35;b", "x=\"1\""] {
            assert!(!is_valid_kanban_metadata_value(value), "{value:?}");
        }
        assert!(!is_valid_kanban_id("a#35;b"));
        for title in ["Issue #12", "C#;", "a = \"b\"", "50%"] {
            let mut node = KanbanNode::new("a", title);
            node.fit_label_quoting();
            assert!(is_valid_kanban_title(&node), "{title:?}");
            let source = format!("kanban\n    {}", emit_node_spec(&node));
            assert_eq!(assert_stable(&source), source);
            assert_renders(&source);
        }
    }

    #[test]
    fn a_blank_board_writes_only_the_keyword() {
        let document = KanbanDocument::empty();
        assert_eq!(emit_kanban(&document), "kanban");
        assert_eq!(parse("kanban"), document);
        assert_eq!(document.unused_id("column"), "column1");
    }

    /// Everything `merman` hands the renderer, in a form that can be compared.
    fn merman_reading(source: &str) -> String {
        let parsed = Engine::new()
            .parse_diagram_for_render_model_sync(source, ParseOptions::strict())
            .unwrap_or_else(|error| panic!("merman should read {source}: {error}"))
            .expect("a diagram");
        format!("{:?}", parsed.model())
    }

    /// Every example `merman`'s own tests draw either opens and survives a
    /// round trip that still draws, or is refused with a reason.
    #[test]
    fn merman_examples_open_and_round_trip_or_are_refused() {
        let fixtures = crate::merman_fixtures("kanban");
        if fixtures.is_empty() {
            eprintln!("merman's kanban fixtures were not found; skipping");
            return;
        }
        let mut opened = 0;
        let mut refused = Vec::new();
        for (path, text) in &fixtures {
            let source = text.replace("\r\n", "\n");
            let source = source.trim_end();
            match parse_kanban(source) {
                Ok(document) => {
                    opened += 1;
                    let emitted = emit_kanban(&document);
                    let again = parse_kanban(&emitted).unwrap_or_else(|error| {
                        panic!("{path:?} wrote back source that does not open: {error}\n{emitted}")
                    });
                    assert_eq!(again, document, "{path:?} changed through a round trip");
                    assert_eq!(emit_kanban(&again), emitted, "{path:?} is not stable");
                    assert_eq!(
                        merman_reading(&emitted),
                        merman_reading(source),
                        "{path:?} is read differently by merman once written back:\n{emitted}"
                    );
                    if !document.columns.is_empty() {
                        assert_renders(&emitted);
                    }
                }
                Err(error) => refused.push(format!(
                    "{}: {error}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                )),
            }
        }
        eprintln!(
            "kanban fixtures: {opened} opened, {} refused",
            refused.len()
        );
        for reason in &refused {
            eprintln!("  {reason}");
        }
        assert!(
            opened * 4 >= fixtures.len() * 3,
            "too few of merman's examples open: {opened} of {}",
            fixtures.len()
        );
    }

    #[test]
    fn the_keyword_is_recognized_the_way_merman_recognizes_it() {
        let source = "kanban\n    todo[未着手]";
        assert!(
            is_kanban_source(source),
            "{source} should be a kanban board"
        );
        for source in [
            "kanbans\n  todo",
            "Kanban\n  todo",
            "",
            "%% kanban",
            "flowchart TD\n    A --> B",
        ] {
            assert!(
                !is_kanban_source(source),
                "{source} should not be a kanban board"
            );
        }
    }
}
