//! Locates the label of a node or an edge inside a mermaid fence body, so that
//! editing a label in the rendered preview can be written back as a single
//! replacement of the token the user actually wrote.
//!
//! This is a deliberately small parser, one per diagram kind, that walks each
//! statement far enough to know which element it declares and where that
//! element's label is written. Anything it cannot read confidently is reported
//! as a refusal so the caller leaves the source alone, which is always safe.
//!
//! What counts as "the label" differs per kind, and only a token whose meaning
//! is the drawn text may be rewritten — never an id, since every other mention
//! of it would have to follow (that is the structured editor's job):
//!
//! * flowchart: the text inside a node's shape or an edge's `|…|`. A node
//!   written bare (`A --> B`) is drawn with its id, and gains a `["…"]`.
//! * ER: a relationship's label after `:`, and an entity's alias `A[…]`, which
//!   an entity declared on a line of its own can be given. merman rejects an
//!   alias written in a relationship statement, so an entity that only ever
//!   appears in one has nowhere to put a label.
//! * state: a transition's label after `:`, and a state's description
//!   (`s1 : text` or `state "text" as s1`).
//! * class: a class's `["…"]` display name and a relation's label after `:`
//!   (see [`class`]).
//! * sequence: the text of a message or a note, and a participant's display
//!   name after `as` (see [`sequence`]).
//! * pie, gantt and an ER entity's attributes: see [`chart_labels`].

mod chart_labels;
mod class;
mod sequence;

use std::ops::Range;

use mermaid_render::mermaid_model::{MermaidDiagramKind, detect_kind};

pub(crate) use chart_labels::{
    find_item_label, reserved_labels, write_refusal as chart_label_write_refusal,
};

/// How a replacement label has to be written for the place it is going into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MermaidLabelQuoting {
    /// Inside a delimiter pair such as `A[…]`, `A{…}` or `-->|…|`. The label is
    /// quoted only when it contains characters that would otherwise close the
    /// delimiter.
    Delimited,
    /// The value of `label:` inside an `A@{ … }` metadata block, which is
    /// always written quoted.
    Metadata,
    /// The element is written with no label at all (the `A` of `A --> B`), so
    /// there is no label token: the span covers the bare reference, which is
    /// rewritten as itself followed by a whole `["…"]`.
    Insert,
    /// An ER relationship's label, which follows the `:` and is always written
    /// quoted because a bare word there collides with the cardinality keywords
    /// merman's lexer reserves.
    ErLabel,
    /// An ER entity's alias, inside the `[…]` that follows its name. Always
    /// written quoted, since an unquoted alias may not even hold a space.
    ErAlias,
    /// An alias for an ER entity that is declared without one. Like
    /// [`Self::Insert`], the span covers the name and the whole `["…"]` is
    /// written after it.
    ErAliasInsert,
    /// A state's description or a transition's label, which follows the `:` and
    /// runs to the end of the line unquoted.
    StateBare,
    /// The description inside the quotes of `state "…" as s1`.
    StateQuoted,
    /// A pie chart slice's label, always written in double quotes with `\`
    /// escapes, which is how merman reads it.
    PieLabel,
    /// A pie chart's title, which runs to the end of its line.
    PieTitle,
    /// A gantt task's name, before the `:`.
    GanttTask,
    /// A gantt section's name, which runs to the end of its line.
    GanttSection,
    /// A gantt chart's title, which runs to the end of its line.
    GanttTitle,
    /// An ER attribute's name, written bare after its type.
    ErAttributeName,
    /// A class's display name inside the `[…]` of `class A["…"]`, quotes
    /// included, which is always written quoted.
    ClassLabel,
    /// A class relation's label, which follows the `:` and runs to the end of
    /// the line unquoted.
    ClassRelationLabel,
    /// The text of a sequence diagram's message or note, or a participant's
    /// display name, which runs unquoted to the end of the statement.
    SequenceText,
    /// A description for a state that is written with none. The span covers
    /// the indentation and the id at the start of the first statement that
    /// opens with the state, and a `s1 : …` line is written in front of it.
    StateInsert,
}

/// Why a label the user double-clicked cannot be written back to the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MermaidLabelRefusal {
    /// Nothing in the fence body could be identified as this label, so the
    /// source is left alone rather than guessed at.
    NotLocated,
    /// The label is written once but drawn on several connections, because its
    /// statement expands an `&` group (`A & B --> C`). Rewriting the token
    /// would silently change the other connections as well.
    SharedByGroup,
    /// The element is drawn with its id because the source gives it no label of
    /// its own, and there is no statement a label could be written into with a
    /// single edit. Renaming the id instead is the structured editor's job,
    /// since every other mention of it has to follow.
    NoLabelToken,
    /// The token is there but holds nothing (`A[]`, `A -->|| B`, `s1 :`). Its
    /// range is empty, which would make the staleness check the caller runs
    /// before writing — "is the token still the one this range was taken from?"
    /// — compare two empty strings and pass however far the buffer has moved
    /// on, dropping the new label at whatever now sits at that offset.
    EmptyLabel,
    /// Other statements carry the same key — a pie slice's label, a gantt
    /// section's name, a gantt task's id — and merman draws or drops them
    /// together, so rewriting one would change what the others draw.
    Duplicated,
}

/// The token a label edit replaces, or the reason there is none.
pub(crate) type MermaidLabelLookup = Result<MermaidLabelSpan, MermaidLabelRefusal>;

/// Where a label lives in the fence body, and what is written there now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MermaidLabelSpan {
    /// Byte range within the fence body, covering the label token including
    /// its surrounding quotes when it has them, but not its delimiters.
    pub(crate) range: Range<usize>,
    /// The label with its quotes and escapes removed.
    pub(crate) label: String,
    pub(crate) quoting: MermaidLabelQuoting,
}

/// The label the renderer draws for the box `node_id` names, and where it is
/// written. What that means depends on the diagram kind: see the module docs.
pub(crate) fn find_node_label(fence_body: &str, node_id: &str) -> MermaidLabelLookup {
    match detect_kind(fence_body) {
        Some(MermaidDiagramKind::Er) => find_er_entity_label(fence_body, node_id),
        Some(MermaidDiagramKind::State) => find_state_description(fence_body, node_id),
        Some(MermaidDiagramKind::Class) => class::find_class_label(fence_body, node_id),
        Some(MermaidDiagramKind::Sequence) => sequence::find_participant_label(fence_body, node_id),
        // Every other kind either is a flowchart or never reaches this far,
        // since the geometry that raised the edit is only collected for the
        // kinds this module reads.
        _ => find_flowchart_node_label(fence_body, node_id),
    }
}

/// The label of the `occurrence`-th connection from `from` to `to`, counting in
/// source order.
pub(crate) fn find_edge_label(
    fence_body: &str,
    from: &str,
    to: &str,
    occurrence: usize,
) -> MermaidLabelLookup {
    match detect_kind(fence_body) {
        Some(MermaidDiagramKind::Er) => {
            find_er_relationship_label(fence_body, from, to, occurrence)
        }
        // A state diagram numbers its connections rather than naming their
        // ends, so a pair of ends says nothing about which one is meant, and
        // reading its statements as a flowchart's would find a token that
        // belongs to something else.
        Some(MermaidDiagramKind::State) => Err(MermaidLabelRefusal::NotLocated),
        Some(MermaidDiagramKind::Class) => {
            class::find_relation_label(fence_body, from, to, occurrence)
        }
        // Messages are numbered too, and are looked up as transitions.
        Some(MermaidDiagramKind::Sequence) => Err(MermaidLabelRefusal::NotLocated),
        _ => find_flowchart_edge_label(fence_body, from, to, occurrence),
    }
}

/// The label of the `index`-th connection of a diagram whose SVG numbers them
/// rather than naming their ends, counting every statement that writes one in
/// document order.
pub(crate) fn find_transition_label(fence_body: &str, index: usize) -> MermaidLabelLookup {
    match detect_kind(fence_body) {
        Some(MermaidDiagramKind::State) => find_state_transition_label(fence_body, index),
        Some(MermaidDiagramKind::Sequence) => sequence::find_item_label(fence_body, index),
        _ => Err(MermaidLabelRefusal::NotLocated),
    }
}

/// The label the renderer draws for `node_id`, and where it is written.
///
/// A node may be declared more than once, and mermaid keeps the last label it
/// was given (`A[one] --> B` followed by `A[two] --> C` draws "two"), so the
/// last declaration that carries one is the token the user is looking at.
/// A node that is only ever referenced (`A --> B`, where `B` never gets a
/// shape) is drawn with its id, and has no label token: the span for it covers
/// the first bare reference itself, which is rewritten as `B["…"]`. Covering the
/// reference rather than the empty place after it is what lets the caller tell
/// that its edit is still going where it was aimed.
fn find_flowchart_node_label(fence_body: &str, node_id: &str) -> MermaidLabelLookup {
    let mut labelled = None;
    let mut insert_at = None;
    for item in parse_items(fence_body) {
        let Item::Node {
            id,
            span,
            insert_at: candidate,
        } = item
        else {
            continue;
        };
        if id != node_id {
            continue;
        }
        if span.is_some() {
            labelled = span;
        }
        if insert_at.is_none() {
            insert_at = candidate;
        }
    }
    let span = labelled
        .or_else(|| {
            insert_at.map(|range| MermaidLabelSpan {
                // Mermaid draws the id itself when a node has no label, so that
                // is what the field opens on.
                range,
                label: node_id.to_string(),
                quoting: MermaidLabelQuoting::Insert,
            })
        })
        .ok_or(MermaidLabelRefusal::NotLocated)?;
    checked(fence_body, span)
}

/// The label of the `occurrence`-th edge from `from` to `to`, counting in
/// source order.
fn find_flowchart_edge_label(
    fence_body: &str,
    from: &str,
    to: &str,
    occurrence: usize,
) -> MermaidLabelLookup {
    let mut seen = 0;
    for item in parse_items(fence_body) {
        let Item::Edge {
            from: edge_from,
            to: edge_to,
            span,
            shared,
        } = item
        else {
            continue;
        };
        if edge_from != from || edge_to != to {
            continue;
        }
        if seen == occurrence {
            let span = span.ok_or(MermaidLabelRefusal::NotLocated)?;
            if shared {
                return Err(MermaidLabelRefusal::SharedByGroup);
            }
            return checked(fence_body, span);
        }
        seen += 1;
    }
    Err(MermaidLabelRefusal::NotLocated)
}

/// Refuses a span that does not address whole characters of `fence_body`.
///
/// Every range here is built from byte offsets of the text that was scanned, so
/// this should never fire; it is the last line of defence between an off-by-one
/// in a scanner and a panic (or a corrupted multibyte character) in the buffer
/// edit the span drives.
fn checked(fence_body: &str, span: MermaidLabelSpan) -> MermaidLabelLookup {
    if span.range.start > span.range.end
        || span.range.end > fence_body.len()
        || !fence_body.is_char_boundary(span.range.start)
        || !fence_body.is_char_boundary(span.range.end)
    {
        return Err(MermaidLabelRefusal::NotLocated);
    }
    if span.range.is_empty() {
        return Err(MermaidLabelRefusal::EmptyLabel);
    }
    Ok(span)
}

/// Writes `label` in the form the slot it replaces expects, where `replaced`
/// is the text the span covers now.
///
/// Newlines cannot appear in a mermaid label token at all, so they collapse to
/// spaces rather than breaking the statement in two.
///
/// Mermaid has no escape for a character inside a quoted label; it decodes the
/// `#…;` entity shorthand instead. Quoting alone is not enough for `|` or `\`:
/// the edge label lexer stops at the first `|` whether or not it is inside
/// quotes, and a trailing `\` escapes the quote that would close the label.
/// `mermaid_render::flowchart_model` writes them the same way.
pub(crate) fn encode_label(label: &str, quoting: MermaidLabelQuoting, replaced: &str) -> String {
    let flattened: String = label
        .chars()
        .map(|character| {
            if character == '\n' || character == '\r' {
                ' '
            } else {
                character
            }
        })
        .collect();
    match quoting {
        MermaidLabelQuoting::Delimited
        | MermaidLabelQuoting::Metadata
        | MermaidLabelQuoting::Insert => {
            let escaped = escape_once(&flattened, |character| match character {
                '"' => Some("#quot;"),
                '|' => Some("#124;"),
                '\\' => Some("#92;"),
                _ => None,
            });
            let needs_quotes = flattened.is_empty()
                || flattened.trim() != flattened
                || flattened
                    .chars()
                    .any(|character| "\"'[](){}|<>;`/\\".contains(character));
            match quoting {
                MermaidLabelQuoting::Insert => format!("{replaced}[\"{escaped}\"]"),
                MermaidLabelQuoting::Delimited if !needs_quotes => escaped,
                _ => format!("\"{escaped}\""),
            }
        }
        // A bare quotation mark would end the string and break the diagram;
        // merman decodes the entity and draws the mark.
        MermaidLabelQuoting::ErLabel => format!(
            "\"{}\"",
            escape_once(&flattened, |character| (character == '"')
                .then_some("#quot;"))
        ),
        MermaidLabelQuoting::ErAlias | MermaidLabelQuoting::ErAliasInsert => {
            let escaped = escape_once(&flattened, |character| match character {
                '"' => Some("#quot;"),
                ']' => Some("#93;"),
                _ => None,
            });
            match quoting {
                MermaidLabelQuoting::ErAliasInsert => format!("{replaced}[\"{escaped}\"]"),
                _ => format!("\"{escaped}\""),
            }
        }
        // `:` and `;` end a description, as `mermaid_render::state_model`
        // writes them too.
        MermaidLabelQuoting::StateBare => escape_once(&flattened, |character| match character {
            ':' => Some("#58;"),
            ';' => Some("#59;"),
            _ => None,
        }),
        MermaidLabelQuoting::StateQuoted | MermaidLabelQuoting::ClassLabel => format!(
            "\"{}\"",
            escape_once(&flattened, |character| (character == '"')
                .then_some("#quot;"))
        ),
        MermaidLabelQuoting::PieLabel
        | MermaidLabelQuoting::PieTitle
        | MermaidLabelQuoting::GanttTask
        | MermaidLabelQuoting::GanttSection
        | MermaidLabelQuoting::GanttTitle
        | MermaidLabelQuoting::ErAttributeName => chart_labels::encode(&flattened, quoting),
        // The label is read to the end of the line as it stands; only the
        // surrounding whitespace, which merman trims, is not kept.
        MermaidLabelQuoting::ClassRelationLabel => flattened.trim().to_string(),
        MermaidLabelQuoting::SequenceText => encode_sequence_text(flattened.trim()),
        MermaidLabelQuoting::StateInsert => {
            let id = replaced.trim_start();
            let indent = replaced
                .get(..replaced.len() - id.len())
                .unwrap_or_default();
            let description = escape_once(&flattened, |character| match character {
                ':' => Some("#58;"),
                ';' => Some("#59;"),
                _ => None,
            });
            // The box already shows the id, so confirming the field as it
            // opened, or emptied, asks for nothing to change, and a line of
            // its own would be written for no visible difference.
            if description.trim().is_empty() || flattened == id {
                replaced.to_string()
            } else {
                format!("{indent}{id} : {description}\n{replaced}")
            }
        }
    }
}

/// Why `new_label` cannot be written into a slot of `quoting`'s kind, for the
/// slots of class and sequence diagrams.
pub(crate) fn write_refusal(quoting: MermaidLabelQuoting, new_label: &str) -> Option<&'static str> {
    match quoting {
        // merman would draw it, but the structured editor refuses to open a
        // relation label holding one, so writing it would lock the diagram out
        // of that editor.
        MermaidLabelQuoting::ClassRelationLabel if new_label.contains("%%") => {
            Some("クラス図の関係ラベルには %% を書けません。別の記号に置き換えてください。")
        }
        MermaidLabelQuoting::SequenceText
            if sequence::wrap_directive_length(new_label.trim()).is_some() =>
        {
            Some(
                "シーケンス図では先頭の wrap: / nowrap: が折り返しの指定として読まれるため、この文字では始められません。",
            )
        }
        MermaidLabelQuoting::SequenceText if new_label.trim().is_empty() => {
            Some("シーケンス図の文字は空にできません。「Mermaid 図を編集」から編集してください。")
        }
        _ => None,
    }
}

/// Writes the text of a sequence diagram's statement so that merman reads all
/// of it: a `;` would end the statement, and a `#` or a `%%` would comment out
/// the rest of the line.
fn encode_sequence_text(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            ';' => encoded.push_str("#59;"),
            '#' => encoded.push_str("#35;"),
            // Only the first of a pair has to go, so a lone `%` stays readable.
            '%' if characters.peek() == Some(&'%') => encoded.push_str("#37;"),
            _ => encoded.push(character),
        }
    }
    encoded
}

/// Replaces characters with the entity that stands for them, in one pass.
///
/// A pass per character class would re-escape what the previous one wrote: the
/// `;` of a freshly written `#58;` would become `#59;` in turn.
fn escape_once(text: &str, entity: impl Fn(char) -> Option<&'static str>) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match entity(character) {
            Some(entity) => escaped.push_str(entity),
            None => escaped.push(character),
        }
    }
    escaped
}

/// The inverse of [`encode_label`]: the label as the user sees it.
///
/// merman decodes the `#…;` shorthand wherever a label is read, so one decoder
/// serves every kind; writing an entity is what differs, and that is
/// [`encode_label`]'s business.
fn decode_label(raw: &str) -> String {
    let trimmed = raw.trim();
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(trimmed);
    if !unquoted.contains('#') {
        return unquoted.to_string();
    }
    const ENTITIES: [(&str, char); 11] = [
        ("#quot;", '"'),
        ("#34;", '"'),
        ("#124;", '|'),
        ("#92;", '\\'),
        ("#93;", ']'),
        ("#58;", ':'),
        ("#colon;", ':'),
        ("#59;", ';'),
        ("#semi;", ';'),
        ("#35;", '#'),
        ("#37;", '%'),
    ];
    let mut decoded = String::with_capacity(unquoted.len());
    let mut rest = unquoted;
    'scan: while let Some(position) = rest.find('#') {
        decoded.push_str(rest.get(..position).unwrap_or_default());
        let tail = rest.get(position..).unwrap_or_default();
        for (entity, character) in ENTITIES {
            if let Some(after) = tail.strip_prefix(entity) {
                decoded.push(character);
                rest = after;
                continue 'scan;
            }
        }
        decoded.push('#');
        rest = tail.get(1..).unwrap_or_default();
    }
    decoded.push_str(rest);
    decoded
}

enum Item {
    Node {
        id: String,
        span: Option<MermaidLabelSpan>,
        /// The reference itself, for one written as a bare id and followed by
        /// nothing a shape would collide with, which may be rewritten as the id
        /// plus a `["…"]` to give the node a label.
        insert_at: Option<Range<usize>>,
    },
    Edge {
        from: String,
        to: String,
        span: Option<MermaidLabelSpan>,
        /// Whether the statement this edge came from expands an `&` group, in
        /// which case its label token is drawn on more than one connection.
        shared: bool,
    },
}

/// Statement keywords that are not node or edge declarations. `subgraph` is
/// among them because its label belongs to a cluster, which the preview does
/// not hit-test, and matching it would shadow a node with the same id.
const SKIPPED_KEYWORDS: &[&str] = &[
    "flowchart",
    "graph",
    "subgraph",
    "end",
    "direction",
    "style",
    "classDef",
    "class",
    "click",
    "linkStyle",
    "accTitle",
    "accDescr",
];

fn parse_items(fence_body: &str) -> Vec<Item> {
    let mut items = Vec::new();
    let mut line_start = 0;
    for line in fence_body.split_inclusive('\n') {
        let content = strip_comment(line);
        let leading = content.len() - content.trim_start().len();
        let trimmed = content.trim_start();
        let is_skipped = SKIPPED_KEYWORDS.iter().any(|keyword| {
            trimmed.strip_prefix(keyword).is_some_and(|rest| {
                rest.is_empty() || !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_')
            })
        });
        if !is_skipped {
            parse_statement(line_start + leading, trimmed, &mut items);
        }
        line_start += line.len();
    }
    items
}

/// Where a `%%` comment starts, or `None` for a line that holds none.
///
/// merman reads `%%` as a comment only where no label token is open, so a pair
/// written inside a shape (`A[50%% 引き]`), between an edge's `|…|` or inside a
/// quoted string belongs to the label and stays. Cutting there would leave this
/// module reading a token the renderer never drew — and, where the same element
/// is declared twice, quietly rewriting the declaration that is not on screen.
fn comment_start(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut index = 0;
    let mut in_quotes = false;
    let mut in_pipes = false;
    let mut depth = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => in_quotes = !in_quotes,
            _ if in_quotes => {}
            b'|' => in_pipes = !in_pipes,
            b'[' | b'(' | b'{' => depth += 1,
            b']' | b')' | b'}' => depth = depth.saturating_sub(1),
            b'%' if !in_pipes && depth == 0 && bytes.get(index + 1) == Some(&b'%') => {
                return Some(index);
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// Truncates `line` at the `%%` comment it opens, if any.
fn strip_comment(line: &str) -> &str {
    match comment_start(line) {
        Some(index) => &line[..index],
        None => line.trim_end_matches(['\n', '\r']),
    }
}

/// [`strip_comment`] for a state diagram, whose descriptions and transition
/// labels run to the end of their line with no delimiter to close them, so
/// merman draws a `%%` written after the `:` as part of the label.
fn strip_state_comment(line: &str) -> &str {
    let Some(index) = comment_start(line) else {
        return line.trim_end_matches(['\n', '\r']);
    };
    match description_colon(line) {
        Some(colon) if colon < index => line.trim_end_matches(['\n', '\r']),
        _ => &line[..index],
    }
}

fn parse_statement(offset: usize, statement: &str, items: &mut Vec<Item>) {
    let mut position = 0;
    let Some(mut left) = parse_node_group(offset, statement, &mut position, items) else {
        return;
    };
    loop {
        skip_whitespace(statement, &mut position);
        let Some(connector) = match_connector(statement, position) else {
            return;
        };
        position = connector.end;
        skip_whitespace(statement, &mut position);
        let Some(right) = parse_node_group(offset, statement, &mut position, items) else {
            return;
        };
        let shared = left.len() * right.len() > 1;
        for from in &left {
            for to in &right {
                items.push(Item::Edge {
                    from: from.clone(),
                    to: to.clone(),
                    span: connector.label.clone().map(|mut span| {
                        span.range.start += offset;
                        span.range.end += offset;
                        span
                    }),
                    shared,
                });
            }
        }
        left = right;
    }
}

/// One side of a link: a node reference, or several joined by `&`.
fn parse_node_group(
    offset: usize,
    statement: &str,
    position: &mut usize,
    items: &mut Vec<Item>,
) -> Option<Vec<String>> {
    let mut group = Vec::new();
    loop {
        skip_whitespace(statement, position);
        let id_start = *position;
        let id = parse_identifier(statement, position)?;
        let id_end = *position;
        let span = match_shape(statement, *position).map(|shape| {
            *position = shape.end;
            MermaidLabelSpan {
                range: (shape.label.range.start + offset)..(shape.label.range.end + offset),
                label: shape.label.label,
                quoting: shape.label.quoting,
            }
        });
        let insert_at = match span {
            Some(_) => None,
            None => accepts_inserted_shape(statement, *position)
                .then(|| (offset + id_start)..(offset + id_end)),
        };
        items.push(Item::Node {
            id: id.clone(),
            span,
            insert_at,
        });
        group.push(id);

        let mut lookahead = *position;
        skip_whitespace(statement, &mut lookahead);
        if statement[lookahead..].starts_with('&') {
            *position = lookahead + 1;
        } else {
            return Some(group);
        }
    }
}

/// Whether a `["…"]` may be appended to the node reference that ends at
/// `position`.
///
/// A reference whose shape the matcher could not read is not necessarily a bare
/// one: `A@{ shape: rect }` is a metadata block with no `label:` key, and
/// `A[/one` is an unterminated shape. Appending a label to either writes a
/// statement that means something else, so only a reference followed by nothing
/// shape-like accepts one.
fn accepts_inserted_shape(statement: &str, position: usize) -> bool {
    let Some(rest) = statement.get(position..) else {
        return false;
    };
    !rest.starts_with('@') && !SHAPES.iter().any(|(opener, _)| rest.starts_with(*opener))
}

fn skip_whitespace(statement: &str, position: &mut usize) {
    while let Some(character) = statement[*position..].chars().next() {
        if character.is_whitespace() {
            *position += character.len_utf8();
        } else {
            break;
        }
    }
}

fn is_identifier_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_' || character == '-' || character == '.'
}

/// Reads a node id, stopping before anything that starts a link or a shape.
fn parse_identifier(statement: &str, position: &mut usize) -> Option<String> {
    let start = *position;
    let rest = &statement[start..];
    let mut end = 0;
    let mut characters = rest.char_indices().peekable();
    while let Some((index, character)) = characters.next() {
        if !is_identifier_char(character) {
            break;
        }
        let next = characters.peek().map(|(_, next)| *next);
        // `-` and `.` are legal inside an id but also begin `-->`, `-.->` and
        // friends; stop before the link rather than swallowing it.
        let starts_link = matches!(
            (character, next),
            ('-', Some('-')) | ('-', Some('.')) | ('-', Some('>')) | ('.', Some('-'))
        );
        if starts_link {
            break;
        }
        end = index + character.len_utf8();
    }
    if end == 0 {
        return None;
    }
    *position = start + end;
    Some(rest[..end].to_string())
}

struct Shape {
    end: usize,
    label: MermaidLabelSpan,
}

/// The delimiter pairs a flowchart node's label can be written in, longest
/// first so that `[[` is not read as `[`. A shape can have more than one
/// closer: the parallelogram and trapezoid variants share an opener.
const SHAPES: &[(&str, &[&str])] = &[
    ("(((", &[")))"]),
    ("([", &["])"]),
    ("[[", &["]]"]),
    ("[(", &[")]"]),
    ("[/", &["/]", "\\]"]),
    ("[\\", &["\\]", "/]"]),
    ("((", &["))"]),
    ("{{", &["}}"]),
    ("[", &["]"]),
    ("(", &[")"]),
    ("{", &["}"]),
    (">", &["]"]),
];

fn match_shape(statement: &str, position: usize) -> Option<Shape> {
    let rest = statement.get(position..)?;
    if let Some(body) = rest.strip_prefix("@{") {
        return match_metadata_shape(statement, position + "@{".len(), body);
    }
    let (opener, closers) = SHAPES
        .iter()
        .find(|(opener, _)| rest.starts_with(*opener))?;
    let body_start = position + opener.len();
    let (range, body_end) = read_delimited_label(statement, body_start, closers)?;
    let closer_length = closers
        .iter()
        .find(|closer| statement[body_end..].starts_with(*closer))?
        .len();
    let label = decode_label(&statement[range.start..range.end]);
    Some(Shape {
        end: body_end + closer_length,
        label: MermaidLabelSpan {
            range,
            label,
            quoting: MermaidLabelQuoting::Delimited,
        },
    })
}

/// Reads the text between a shape's delimiters, returning its range and the
/// offset the closer starts at.
///
/// A quoted label is read as a string first, so a `]` inside quotes does not
/// end the shape.
fn read_delimited_label(
    statement: &str,
    body_start: usize,
    closers: &[&str],
) -> Option<(Range<usize>, usize)> {
    let mut cursor = body_start;
    skip_whitespace(statement, &mut cursor);
    if statement[cursor..].starts_with('"') {
        let closing_quote = statement[cursor + 1..].find('"')? + cursor + 1;
        let label = cursor..closing_quote + 1;
        let mut after = label.end;
        skip_whitespace(statement, &mut after);
        if !closers
            .iter()
            .any(|closer| statement[after..].starts_with(closer))
        {
            return None;
        }
        return Some((label, after));
    }
    let body_end = closers
        .iter()
        .filter_map(|closer| statement[body_start..].find(closer))
        .min()?
        + body_start;
    Some((body_start..body_end, body_end))
}

/// `A@{ shape: rect, label: "…" }`.
fn match_metadata_shape(statement: &str, body_start: usize, body: &str) -> Option<Shape> {
    let mut cursor = 0;
    let mut in_quotes = false;
    let mut label: Option<Range<usize>> = None;
    // `cursor` walks whole characters: a metadata block may hold text in any
    // script, and slicing it mid-character would panic.
    while let Some(character) = body[cursor..].chars().next() {
        match character {
            '"' => {
                in_quotes = !in_quotes;
                cursor += 1;
            }
            '}' if !in_quotes => {
                return label.map(|label| Shape {
                    end: body_start + cursor + 1,
                    label: MermaidLabelSpan {
                        range: label.clone(),
                        label: decode_label(&statement[label.start..label.end]),
                        quoting: MermaidLabelQuoting::Metadata,
                    },
                });
            }
            _ if !in_quotes
                && label.is_none()
                && body[cursor..].starts_with("label")
                && !body[..cursor].ends_with(|previous: char| previous.is_alphanumeric()) =>
            {
                let after_key = cursor + "label".len();
                let mut value_start = after_key;
                skip_whitespace(body, &mut value_start);
                if !body[value_start..].starts_with(':') {
                    cursor = after_key;
                    continue;
                }
                value_start += 1;
                skip_whitespace(body, &mut value_start);
                let value_end = if body[value_start..].starts_with('"') {
                    body[value_start + 1..].find('"')? + value_start + 2
                } else {
                    body[value_start..]
                        .find([',', '}'])
                        .map(|index| index + value_start)?
                };
                let trimmed_end =
                    value_end - (body[..value_end].len() - body[..value_end].trim_end().len());
                label = Some((body_start + value_start)..(body_start + trimmed_end));
                cursor = value_end;
            }
            _ => cursor += character.len_utf8(),
        }
    }
    None
}

#[derive(Clone)]
struct Connector {
    end: usize,
    label: Option<MermaidLabelSpan>,
}

/// Matches a flowchart link at `position`, with its inline label if it has one.
///
/// Both of mermaid's forms are handled: a complete arrow optionally followed by
/// `|text|`, and an open link whose text sits between the two halves
/// (`-- text -->`, `== text ==>`, `-. text .->`).
fn match_connector(statement: &str, position: usize) -> Option<Connector> {
    let rest = statement.get(position..)?;
    let mut cursor = 0;

    // `<--`, `o--` and `x--` start a link that also has a head at its tail.
    if let Some(first) = rest.chars().next()
        && matches!(first, '<' | 'o' | 'x')
    {
        let after = &rest[first.len_utf8()..];
        if after.starts_with("--") || after.starts_with("==") || after.starts_with("-.") {
            cursor += first.len_utf8();
        }
    }

    let body = &rest[cursor..];
    let (open_end, label) = if body.starts_with("~~~") {
        (
            cursor + body.len() - body.trim_start_matches('~').len(),
            None,
        )
    } else if body.starts_with("-.") {
        match_dotted(statement, position, rest, cursor)?
    } else if body.starts_with("--") {
        match_plain(statement, position, rest, cursor, '-')?
    } else if body.starts_with("==") {
        match_plain(statement, position, rest, cursor, '=')?
    } else {
        return None;
    };

    if label.is_some() {
        return Some(Connector {
            end: position + open_end,
            label,
        });
    }

    // A complete arrow may be followed by `|text|`.
    let mut after = position + open_end;
    skip_whitespace(statement, &mut after);
    if statement[after..].starts_with('|') {
        let body_start = after + 1;
        let (range, body_end) = read_delimited_label(statement, body_start, &["|"])?;
        return Some(Connector {
            end: body_end + 1,
            label: Some(MermaidLabelSpan {
                range: range.clone(),
                label: decode_label(&statement[range.start..range.end]),
                quoting: MermaidLabelQuoting::Delimited,
            }),
        });
    }
    Some(Connector {
        end: position + open_end,
        label: None,
    })
}

/// `--`/`==` links: either a complete arrow, or an open link wrapped around
/// its text.
fn match_plain(
    statement: &str,
    position: usize,
    rest: &str,
    cursor: usize,
    dash: char,
) -> Option<(usize, Option<MermaidLabelSpan>)> {
    let body = &rest[cursor..];
    let run = body.len() - body.trim_start_matches(dash).len();
    let after_run = body[run..].chars().next();
    // mermaid's own rule: a complete arrow is `--`/`==` followed by another
    // dash or an arrow head. Exactly two, followed by anything else, opens a
    // link whose text runs up to the closing half.
    if matches!(after_run, Some('>' | 'x' | 'o')) || run > 2 {
        let head = usize::from(matches!(after_run, Some('>' | 'x' | 'o')));
        return Some((cursor + run + head, None));
    }
    let closer: String = [dash, dash].iter().collect();
    let text_start = cursor + run;
    let text_end = body[run..].find(&closer)? + cursor + run;
    let raw = &statement[position + text_start..position + text_end];
    let label = raw.trim();
    let label_start = position + text_start + (raw.len() - raw.trim_start().len());
    let closing = &rest[text_end..];
    let closing_run = closing.len() - closing.trim_start_matches(dash).len();
    let head = usize::from(matches!(
        closing[closing_run..].chars().next(),
        Some('>' | 'x' | 'o')
    ));
    Some((
        text_end + closing_run + head,
        Some(MermaidLabelSpan {
            range: label_start..label_start + label.len(),
            label: decode_label(label),
            quoting: MermaidLabelQuoting::Delimited,
        }),
    ))
}

/// `-.->` and `-. text .->`.
fn match_dotted(
    statement: &str,
    position: usize,
    rest: &str,
    cursor: usize,
) -> Option<(usize, Option<MermaidLabelSpan>)> {
    let body = &rest[cursor..];
    let after_open = body["-.".len()..].chars().next();
    if matches!(after_open, Some('-' | '.')) {
        let tail = &body["-.".len()..];
        let run = tail.len() - tail.trim_start_matches(['-', '.']).len();
        let head = usize::from(matches!(tail[run..].chars().next(), Some('>' | 'x' | 'o')));
        return Some((cursor + "-.".len() + run + head, None));
    }
    let text_start = cursor + "-.".len();
    let text_end = body["-.".len()..].find(".-")? + text_start;
    let raw = &statement[position + text_start..position + text_end];
    let label = raw.trim();
    let label_start = position + text_start + (raw.len() - raw.trim_start().len());
    let closing = &rest[text_end..];
    let run = closing.len() - closing.trim_start_matches(['-', '.']).len();
    let head = usize::from(matches!(
        closing[run..].chars().next(),
        Some('>' | 'x' | 'o')
    ));
    Some((
        text_end + run + head,
        Some(MermaidLabelSpan {
            range: label_start..label_start + label.len(),
            label: decode_label(label),
            quoting: MermaidLabelQuoting::Delimited,
        }),
    ))
}

/// Whether `statement` opens with `keyword` as a whole word.
fn starts_with_word(statement: &str, keyword: &str) -> bool {
    statement.strip_prefix(keyword).is_some_and(|rest| {
        rest.is_empty()
            || !rest.starts_with(|character: char| character.is_alphanumeric() || character == '_')
    })
}

/// Statements of an ER diagram that declare nothing this module can edit.
const ER_KEYWORDS: &[&str] = &[
    "erDiagram",
    "direction",
    "style",
    "classDef",
    "class",
    "title",
    "accTitle",
    "accDescr",
];

/// One statement of an ER diagram, as far as this module reads it.
enum ErStatement {
    Relationship {
        from: String,
        to: String,
        label: Option<MermaidLabelSpan>,
    },
    /// `name`, `name[alias]`, either standing alone or heading an attribute
    /// block.
    Declaration {
        name: String,
        alias: Option<MermaidLabelSpan>,
        /// The name itself, which a declaration with no alias gets one by being
        /// rewritten as the name plus a `["…"]`.
        insert_at: Range<usize>,
    },
    Other,
}

/// The statements of an ER diagram, with the byte offset each one starts at.
///
/// An entity's attributes live inside a block, which only a `{` at the end of a
/// line opens and only a `}` at the start of one closes. The `{` of a `||--o{`
/// cardinality is neither, and reading it as one would hide every statement
/// below it.
fn er_statements(fence_body: &str) -> Vec<(usize, &str)> {
    let mut statements = Vec::new();
    let mut line_start = 0;
    let mut in_block = false;
    for line in fence_body.split_inclusive('\n') {
        let content = strip_comment(line);
        let trimmed = content.trim();
        let leading = content.len() - content.trim_start().len();
        if in_block {
            if trimmed.starts_with('}') {
                in_block = false;
            }
        } else {
            if !trimmed.is_empty() {
                statements.push((line_start + leading, trimmed));
            }
            if trimmed.ends_with('{') {
                in_block = true;
            }
        }
        line_start += line.len();
    }
    statements
}

fn parse_er_statement(offset: usize, statement: &str) -> ErStatement {
    if ER_KEYWORDS
        .iter()
        .any(|keyword| starts_with_word(statement, keyword))
    {
        return ErStatement::Other;
    }
    let mut position = 0;
    skip_whitespace(statement, &mut position);
    let name_start = position;
    let Some(from) = read_er_name(statement, &mut position) else {
        return ErStatement::Other;
    };
    let after_name = position;

    skip_whitespace(statement, &mut position);
    let cardinality_start = position;
    while let Some(character) = statement[position..].chars().next() {
        if "|o{}.-".contains(character) {
            position += character.len_utf8();
        } else {
            break;
        }
    }
    if position - cardinality_start >= 3 {
        skip_whitespace(statement, &mut position);
        let Some(to) = read_er_name(statement, &mut position) else {
            return ErStatement::Other;
        };
        skip_whitespace(statement, &mut position);
        if !statement[position..].starts_with(':') {
            return ErStatement::Other;
        }
        position += 1;
        skip_whitespace(statement, &mut position);
        let raw = statement[position..].trim_end();
        let label = (!raw.is_empty()).then(|| MermaidLabelSpan {
            range: (offset + position)..(offset + position + raw.len()),
            label: decode_label(raw),
            quoting: MermaidLabelQuoting::ErLabel,
        });
        return ErStatement::Relationship { from, to, label };
    }

    let mut position = after_name;
    let alias = match_er_alias(offset, statement, &mut position);
    skip_whitespace(statement, &mut position);
    // Anything else on the line is a statement this reader does not know, and
    // an alias written into one would land in the middle of it.
    let rest = &statement[position..];
    if !(rest.is_empty() || rest == "{") {
        return ErStatement::Other;
    }
    ErStatement::Declaration {
        name: from,
        alias,
        insert_at: (offset + name_start)..(offset + after_name),
    }
}

/// Reads an entity name, which is either quoted or a run of characters that
/// starts neither an alias, an attribute block nor a label.
fn read_er_name(statement: &str, position: &mut usize) -> Option<String> {
    skip_whitespace(statement, position);
    let rest = statement.get(*position..)?;
    if let Some(body) = rest.strip_prefix('"') {
        let end = body.find('"')?;
        *position += 1 + end + 1;
        return Some(body[..end].to_string());
    }
    let mut end = 0;
    for (index, character) in rest.char_indices() {
        if character.is_whitespace() || "[]{}:".contains(character) {
            break;
        }
        end = index + character.len_utf8();
    }
    if end == 0 {
        return None;
    }
    *position += end;
    Some(rest[..end].to_string())
}

/// `name[alias]`, whose alias may be quoted.
fn match_er_alias(
    offset: usize,
    statement: &str,
    position: &mut usize,
) -> Option<MermaidLabelSpan> {
    if !statement.get(*position..)?.starts_with('[') {
        return None;
    }
    let body_start = *position + 1;
    let (range, body_end) = read_delimited_label(statement, body_start, &["]"])?;
    if !statement[body_end..].starts_with(']') {
        return None;
    }
    *position = body_end + 1;
    Some(MermaidLabelSpan {
        range: (offset + range.start)..(offset + range.end),
        label: decode_label(&statement[range.start..range.end]),
        quoting: MermaidLabelQuoting::ErAlias,
    })
}

/// The alias an ER entity is drawn with, or where one may be written.
///
/// An entity with no alias is drawn with its name, which is also how every
/// relationship refers to it, so the name itself is never rewritten here. An
/// alias may only be written where the entity is declared: merman rejects one
/// inside a relationship statement.
fn find_er_entity_label(fence_body: &str, entity: &str) -> MermaidLabelLookup {
    let mut alias = None;
    let mut insert_at = None;
    let mut mentioned = false;
    for (offset, statement) in er_statements(fence_body) {
        match parse_er_statement(offset, statement) {
            ErStatement::Relationship { from, to, .. } => {
                mentioned |= from == entity || to == entity;
            }
            ErStatement::Declaration {
                name,
                alias: declared,
                insert_at: candidate,
            } => {
                if name != entity {
                    continue;
                }
                mentioned = true;
                match declared {
                    Some(span) => alias = Some(span),
                    None => {
                        if insert_at.is_none() {
                            insert_at = Some(candidate);
                        }
                    }
                }
            }
            ErStatement::Other => {}
        }
    }
    if let Some(span) = alias {
        return checked(fence_body, span);
    }
    if let Some(range) = insert_at {
        return checked(
            fence_body,
            MermaidLabelSpan {
                range,
                label: entity.to_string(),
                quoting: MermaidLabelQuoting::ErAliasInsert,
            },
        );
    }
    Err(if mentioned {
        MermaidLabelRefusal::NoLabelToken
    } else {
        MermaidLabelRefusal::NotLocated
    })
}

/// The label of the `occurrence`-th relationship from `from` to `to`.
fn find_er_relationship_label(
    fence_body: &str,
    from: &str,
    to: &str,
    occurrence: usize,
) -> MermaidLabelLookup {
    let mut seen = 0;
    for (offset, statement) in er_statements(fence_body) {
        let ErStatement::Relationship {
            from: statement_from,
            to: statement_to,
            label,
        } = parse_er_statement(offset, statement)
        else {
            continue;
        };
        if statement_from != from || statement_to != to {
            continue;
        }
        if seen == occurrence {
            return checked(fence_body, label.ok_or(MermaidLabelRefusal::NotLocated)?);
        }
        seen += 1;
    }
    Err(MermaidLabelRefusal::NotLocated)
}

/// What one pass over a state diagram's statements found.
struct StateScan {
    /// The label token of each transition, in the order the statements wrote
    /// them, which is the order merman numbers them in.
    transitions: Vec<Option<MermaidLabelSpan>>,
    /// The description written for a state, which is what the box is drawn
    /// with. A state may be given more than one, and mermaid draws them all.
    descriptions: Vec<(String, MermaidLabelSpan)>,
    /// Every state id the source names.
    mentioned: Vec<String>,
    /// For each state, the first statement that opens with its id: the range
    /// from the start of the line to the end of the id, which is where a
    /// description can be written in front of it.
    leading: Vec<(String, Range<usize>)>,
    /// Set by a statement whose transitions this reader cannot number the way
    /// the renderer does: a chain (`a --> b --> c`) writes two of them, and a
    /// composite state written on one line hides however many are inside it.
    unreadable: bool,
}

/// Reads a state diagram far enough to number its transitions and to find the
/// description of each state.
fn scan_state(fence_body: &str) -> StateScan {
    let mut scan = StateScan {
        transitions: Vec::new(),
        descriptions: Vec::new(),
        mentioned: Vec::new(),
        leading: Vec::new(),
        unreadable: false,
    };
    let mut line_start = 0;
    let mut in_note = false;
    let mut in_description_block = false;
    for line in fence_body.split_inclusive('\n') {
        let content = strip_state_comment(line);
        let trimmed = content.trim();
        let line_begin = line_start;
        let offset = line_start + (content.len() - content.trim_start().len());
        line_start += line.len();
        // An `accDescr { … }` block is prose, and a line of it that happens
        // to open with a state's id is no place to write a description in
        // front of.
        if in_description_block {
            if trimmed.contains('}') {
                in_description_block = false;
            }
            continue;
        }
        if starts_with_word(trimmed, "accDescr") && trimmed.contains('{') && !trimmed.contains('}')
        {
            in_description_block = true;
            continue;
        }
        if in_note {
            // A note's text is drawn in a box of its own, which is not a state.
            // merman reads an arrow written inside one as a transition of its
            // own, so a note that holds one is left to the structured editor
            // rather than numbered by guesswork.
            if trimmed.contains("-->") {
                scan.unreadable = true;
            }
            if starts_with_word(trimmed, "end") {
                in_note = false;
            }
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        if starts_with_word(trimmed, "note") {
            // A note is drawn in a box attached to its state by a connection of
            // its own, and merman numbers that connection among the
            // transitions, so a note takes a place in the count even though its
            // own edge id is spelled differently and never mapped.
            scan.transitions.push(None);
            in_note = description_colon(trimmed).is_none();
            continue;
        }
        let first_word_end = trimmed
            .find(|character: char| {
                character.is_whitespace() || matches!(character, ':' | '-' | ';' | '{')
            })
            .unwrap_or(trimmed.len());
        let first_word = &trimmed[..first_word_end];
        if !first_word.is_empty()
            && first_word != "[*]"
            && !scan.leading.iter().any(|(id, _)| id == first_word)
        {
            scan.leading
                .push((first_word.to_string(), line_begin..offset + first_word_end));
        }
        let colon = description_colon(trimmed);
        let head = &trimmed[..colon.unwrap_or(trimmed.len())];
        if head.contains("-->") {
            if head.matches("-->").count() > 1 || (trimmed.contains('{') && trimmed.contains('}')) {
                scan.unreadable = true;
            }
            for end in head.split("-->") {
                scan.mentioned.push(state_id_of(end));
            }
            scan.transitions
                .push(colon.map(|colon| state_bare_span(offset, trimmed, colon)));
            continue;
        }
        if let Some(colon) = colon {
            let id = state_id_of(head);
            if !id.is_empty() && !id.contains(char::is_whitespace) {
                scan.mentioned.push(id.clone());
                scan.descriptions
                    .push((id, state_bare_span(offset, trimmed, colon)));
            }
            continue;
        }
        if starts_with_word(trimmed, "state") {
            read_state_declaration(offset, trimmed, &mut scan);
        }
    }
    scan
}

/// `state "text" as s1`, `state s1 { … }` and `state s1 <<fork>>`.
fn read_state_declaration(offset: usize, statement: &str, scan: &mut StateScan) {
    let rest = statement["state".len()..].trim_start();
    let rest_offset = offset + statement.len() - rest.len();
    if let Some(body) = rest.strip_prefix('"') {
        let Some(end) = body.find('"') else {
            return;
        };
        let Some(id) = body[end + 1..].trim_start().strip_prefix("as") else {
            return;
        };
        let id = state_id_of(id);
        if id.is_empty() || id.contains(char::is_whitespace) {
            return;
        }
        scan.mentioned.push(id.clone());
        scan.descriptions.push((
            id,
            MermaidLabelSpan {
                range: rest_offset..(rest_offset + end + 2),
                label: decode_label(&rest[..end + 2]),
                quoting: MermaidLabelQuoting::StateQuoted,
            },
        ));
        return;
    }
    let id = state_id_of(rest.split_whitespace().next().unwrap_or_default());
    if !id.is_empty() {
        scan.mentioned.push(id);
    }
}

/// The span of a description or a transition label, which runs from just after
/// the `:` to the end of the statement.
fn state_bare_span(offset: usize, statement: &str, colon: usize) -> MermaidLabelSpan {
    let mut start = colon + 1;
    skip_whitespace(statement, &mut start);
    let raw = statement[start..].trim_end();
    MermaidLabelSpan {
        range: (offset + start)..(offset + start + raw.len()),
        label: decode_label(raw),
        quoting: MermaidLabelQuoting::StateBare,
    }
}

/// The `:` that starts a description, which is the first one that is not part
/// of the `:::` that names a style class.
fn description_colon(statement: &str) -> Option<usize> {
    let bytes = statement.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b':' {
            if bytes.get(index + 1) == Some(&b':') && bytes.get(index + 2) == Some(&b':') {
                index += 3;
                continue;
            }
            return Some(index);
        }
        index += 1;
    }
    None
}

/// A state id as it is written at either end of a transition or in front of a
/// description, without the style class a `:::` may append.
fn state_id_of(text: &str) -> String {
    text.split(":::")
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// The description a state's box is drawn with.
///
/// A state with none is drawn with its id, which every transition refers to as
/// well, so it is never rewritten here.
fn find_state_description(fence_body: &str, state_id: &str) -> MermaidLabelLookup {
    let scan = scan_state(fence_body);
    let mut found = scan
        .descriptions
        .iter()
        .filter(|(id, _)| id == state_id)
        .map(|(_, span)| span);
    let Some(span) = found.next() else {
        if !scan.mentioned.iter().any(|id| id == state_id) {
            return Err(MermaidLabelRefusal::NotLocated);
        }
        // A state written with no description is drawn with its id. Giving it
        // one takes a line of its own, written in front of the first
        // statement that opens with the state, so that it lands in the same
        // composite state as that statement.
        let Some((_, range)) = scan.leading.iter().find(|(id, _)| id == state_id) else {
            return Err(MermaidLabelRefusal::NoLabelToken);
        };
        return checked(
            fence_body,
            MermaidLabelSpan {
                range: range.clone(),
                label: state_id.to_string(),
                quoting: MermaidLabelQuoting::StateInsert,
            },
        );
    };
    // Mermaid draws every description a state is given, so with more than one
    // there is no single token that is the label in front of the user.
    if found.next().is_some() {
        return Err(MermaidLabelRefusal::NotLocated);
    }
    checked(fence_body, span.clone())
}

/// The label of the `index`-th transition, counting every transition statement
/// in document order, including the ones inside a composite state.
fn find_state_transition_label(fence_body: &str, index: usize) -> MermaidLabelLookup {
    let scan = scan_state(fence_body);
    if scan.unreadable {
        return Err(MermaidLabelRefusal::NotLocated);
    }
    let span = scan
        .transitions
        .get(index)
        .cloned()
        .flatten()
        .ok_or(MermaidLabelRefusal::NotLocated)?;
    checked(fence_body, span)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Applies the edit the preview would make, so a test reads as the source
    /// before and after.
    fn replace_node(fence_body: &str, node_id: &str, new_label: &str) -> Option<String> {
        Some(apply(
            fence_body,
            &find_node_label(fence_body, node_id).ok()?,
            new_label,
        ))
    }

    fn replace_edge(
        fence_body: &str,
        from: &str,
        to: &str,
        occurrence: usize,
        new_label: &str,
    ) -> Option<String> {
        let span = find_edge_label(fence_body, from, to, occurrence).ok()?;
        Some(apply(fence_body, &span, new_label))
    }

    /// The label a lookup reports, for the cases where only the text matters.
    fn label_of(lookup: MermaidLabelLookup) -> Option<String> {
        lookup.ok().map(|span| span.label)
    }

    #[test]
    fn every_node_shape_round_trips() {
        let cases = [
            ("flowchart TD\n  A[old]", "flowchart TD\n  A[new]"),
            ("flowchart TD\n  A(old)", "flowchart TD\n  A(new)"),
            ("flowchart TD\n  A([old])", "flowchart TD\n  A([new])"),
            ("flowchart TD\n  A[[old]]", "flowchart TD\n  A[[new]]"),
            ("flowchart TD\n  A[(old)]", "flowchart TD\n  A[(new)]"),
            ("flowchart TD\n  A((old))", "flowchart TD\n  A((new))"),
            ("flowchart TD\n  A(((old)))", "flowchart TD\n  A(((new)))"),
            ("flowchart TD\n  A>old]", "flowchart TD\n  A>new]"),
            ("flowchart TD\n  A{old}", "flowchart TD\n  A{new}"),
            ("flowchart TD\n  A{{old}}", "flowchart TD\n  A{{new}}"),
            ("flowchart TD\n  A[/old/]", "flowchart TD\n  A[/new/]"),
            ("flowchart TD\n  A[\\old\\]", "flowchart TD\n  A[\\new\\]"),
            ("flowchart TD\n  A[/old\\]", "flowchart TD\n  A[/new\\]"),
            ("flowchart TD\n  A[\\old/]", "flowchart TD\n  A[\\new/]"),
            (
                "flowchart TD\n  A@{ shape: rect, label: \"old\" }",
                "flowchart TD\n  A@{ shape: rect, label: \"new\" }",
            ),
            (
                "flowchart TD\n  A@{ label: old }",
                "flowchart TD\n  A@{ label: \"new\" }",
            ),
            ("flowchart TD\n  A[\"old\"]", "flowchart TD\n  A[new]"),
        ];
        for (before, after) in cases {
            assert_eq!(
                replace_node(before, "A", "new").as_deref(),
                Some(after),
                "rewriting {before:?}"
            );
            assert_eq!(
                label_of(find_node_label(before, "A")),
                Some("old".to_string()),
                "reading {before:?}"
            );
        }
    }

    #[test]
    fn every_edge_form_round_trips() {
        let cases = [
            (
                "flowchart TD\n  A -->|old| B",
                "flowchart TD\n  A -->|new| B",
            ),
            ("flowchart TD\n  A---|old|B", "flowchart TD\n  A---|new|B"),
            (
                "flowchart TD\n  A -- old --> B",
                "flowchart TD\n  A -- new --> B",
            ),
            (
                "flowchart TD\n  A -- old --- B",
                "flowchart TD\n  A -- new --- B",
            ),
            (
                "flowchart TD\n  A -. old .-> B",
                "flowchart TD\n  A -. new .-> B",
            ),
            (
                "flowchart TD\n  A == old ==> B",
                "flowchart TD\n  A == new ==> B",
            ),
            (
                "flowchart TD\n  A -.->|old| B",
                "flowchart TD\n  A -.->|new| B",
            ),
            (
                "flowchart TD\n  A ==>|old| B",
                "flowchart TD\n  A ==>|new| B",
            ),
            (
                "flowchart TD\n  A --x|old| B",
                "flowchart TD\n  A --x|new| B",
            ),
            (
                "flowchart TD\n  A -->|\"old\"| B",
                "flowchart TD\n  A -->|new| B",
            ),
        ];
        for (before, after) in cases {
            assert_eq!(
                replace_edge(before, "A", "B", 0, "new").as_deref(),
                Some(after),
                "rewriting {before:?}"
            );
            assert_eq!(
                label_of(find_edge_label(before, "A", "B", 0)),
                Some("old".to_string()),
                "reading {before:?}"
            );
        }
    }

    /// The label belongs to a declaration, not to every mention of the id, and
    /// a node declared twice is drawn with the last label it was given, so that
    /// is the token the user is looking at. Rewriting any other occurrence
    /// would leave the diagram unchanged while editing a different line.
    #[test]
    fn the_last_declaration_is_rewritten() {
        let source = "flowchart TD\n  A[old] --> B[other]\n  B --> A\n  A[second]";
        assert_eq!(
            label_of(find_node_label(source, "A")),
            Some("second".to_string())
        );
        assert_eq!(
            replace_node(source, "A", "new").as_deref(),
            Some("flowchart TD\n  A[old] --> B[other]\n  B --> A\n  A[new]")
        );
    }

    /// A metadata block may hold text in any script, and the scan that looks
    /// for its `label` key walks it one character at a time. Walking it by
    /// bytes panics as soon as it lands inside a multibyte character, and the
    /// first diagram below does render, so nothing but a double-click would
    /// stand between the user and a crash.
    #[test]
    fn metadata_blocks_may_contain_any_script() {
        assert_eq!(
            label_of(find_node_label(
                "flowchart TD\n  A@{ shape: rect, tooltip: \u{65e5}\u{672c}\u{8a9e}, label: \"x\" }",
                "A"
            )),
            Some("x".to_string())
        );
        assert_eq!(
            find_node_label(
                "flowchart TD\n  A@{ shape: rect, tooltip: \u{65e5}\u{672c}\u{8a9e} }",
                "A"
            ),
            Err(MermaidLabelRefusal::NotLocated)
        );
        assert_eq!(
            find_node_label("flowchart TD\n  A@{ \u{8aac}\u{660e} }", "A"),
            Err(MermaidLabelRefusal::NotLocated)
        );
    }

    /// A node id that does not occur at all cannot be guessed at, since a wrong
    /// guess corrupts the user's source, and neither can an edge that carries
    /// no label token.
    #[test]
    fn unknown_nodes_and_unlabelled_edges_are_not_found() {
        assert_eq!(
            find_node_label("flowchart TD\n  A[one] --> B", "C"),
            Err(MermaidLabelRefusal::NotLocated)
        );
        assert_eq!(
            find_edge_label("flowchart TD\n  A --> B", "A", "B", 0),
            Err(MermaidLabelRefusal::NotLocated)
        );
        assert_eq!(
            find_edge_label("flowchart TD\n  A -->|one| B", "A", "B", 1),
            Err(MermaidLabelRefusal::NotLocated)
        );
    }

    /// A node that is only ever referenced (`A --> B`) is drawn with its id and
    /// has no token to rewrite, so a label is written by giving it a shape. The
    /// insertion goes after the first bare reference, and only where a shape can
    /// legally follow: appending one to `A@{ … }` or to an unterminated shape
    /// would write a different statement.
    #[test]
    fn unlabelled_nodes_gain_a_shape() {
        let source = "flowchart TD\n  A[one] --> B\n  B --> C";
        let span = find_node_label(source, "B").expect("B should be locatable");
        assert_eq!(span.quoting, MermaidLabelQuoting::Insert);
        assert_eq!(&source[span.range.clone()], "B");
        assert_eq!(span.range, 26..27, "the span covers the first bare `B`");
        assert_eq!(
            span.label, "B",
            "mermaid draws the id when there is no label"
        );
        let edited = apply(source, &span, "\u{4e8c}\u{3064}\u{76ee}");
        assert_eq!(
            edited,
            "flowchart TD\n  A[one] --> B[\"\u{4e8c}\u{3064}\u{76ee}\"]\n  B --> C"
        );
        assert_eq!(
            geometry_of(&edited)
                .nodes
                .iter()
                .find(|node| node.node_id == "B")
                .map(|node| node.label.clone()),
            Some("\u{4e8c}\u{3064}\u{76ee}".to_string()),
            "the edited source has to render with the new label"
        );
        assert_eq!(
            replace_node("flowchart TD\n  A&B --> C", "B", "x").as_deref(),
            Some("flowchart TD\n  A&B[\"x\"] --> C")
        );
        assert_eq!(
            replace_node("flowchart TD\n  A:::big --> B", "A", "x").as_deref(),
            Some("flowchart TD\n  A[\"x\"]:::big --> B")
        );
        assert_eq!(
            find_node_label("flowchart TD\n  A@{ shape: rect } --> B", "A"),
            Err(MermaidLabelRefusal::NotLocated),
            "a metadata block with no label key is not a bare reference"
        );
        assert_eq!(
            find_node_label("flowchart TD\n  A[/unterminated --> B", "A"),
            Err(MermaidLabelRefusal::NotLocated),
            "an unterminated shape is not a bare reference"
        );
    }

    #[test]
    fn parallel_edges_are_told_apart_by_position() {
        let source = "flowchart TD\n  A -->|first| B\n  A -->|second| B";
        assert_eq!(
            replace_edge(source, "A", "B", 1, "new").as_deref(),
            Some("flowchart TD\n  A -->|first| B\n  A -->|new| B")
        );
        assert_eq!(
            label_of(find_edge_label(source, "A", "B", 0)),
            Some("first".to_string())
        );
    }

    /// The edges of a chain are numbered in the order they were written, which
    /// is the order the renderer lays them out in. The edges an `&` group
    /// expands to all share one label token, so rewriting either of them would
    /// silently relabel the other connections as well: they are refused with
    /// the reason instead.
    #[test]
    fn chains_and_ampersand_groups_produce_edges_in_source_order() {
        let source = "flowchart TD\n  A -->|one| B -->|two| C\n  B & C -->|three| D";
        assert_eq!(
            label_of(find_edge_label(source, "A", "B", 0)),
            Some("one".to_string())
        );
        assert_eq!(
            label_of(find_edge_label(source, "B", "C", 0)),
            Some("two".to_string())
        );
        assert_eq!(
            find_edge_label(source, "B", "D", 0),
            Err(MermaidLabelRefusal::SharedByGroup)
        );
        assert_eq!(
            find_edge_label(source, "C", "D", 0),
            Err(MermaidLabelRefusal::SharedByGroup)
        );
        // A group on either side alone is enough to share the token.
        assert_eq!(
            find_edge_label("flowchart TD\n  A -->|x| B & C", "A", "B", 0),
            Err(MermaidLabelRefusal::SharedByGroup)
        );
        assert_eq!(
            label_of(find_edge_label("flowchart TD\n  A -->|x| B", "A", "B", 0)),
            Some("x".to_string())
        );
    }

    #[test]
    fn labels_needing_quotes_are_quoted() {
        assert_eq!(
            replace_node("flowchart TD\n  A[old]", "A", "a [b] c").as_deref(),
            Some("flowchart TD\n  A[\"a [b] c\"]")
        );
        assert_eq!(
            replace_node("flowchart TD\n  A[old]", "A", "say \"hi\"").as_deref(),
            Some("flowchart TD\n  A[\"say #quot;hi#quot;\"]")
        );
        // Quoting alone does not protect `|` or `\`: the edge label lexer stops
        // at the first `|` whether or not it is quoted, and a trailing `\`
        // escapes the quote that would close the label.
        assert_eq!(
            replace_edge("flowchart TD\n  A -->|old| B", "A", "B", 0, "yes|no").as_deref(),
            Some("flowchart TD\n  A -->|\"yes#124;no\"| B")
        );
        assert_eq!(
            replace_node("flowchart TD\n  A[old]", "A", "C:\\path\\").as_deref(),
            Some("flowchart TD\n  A[\"C:#92;path#92;\"]")
        );
        assert_eq!(
            label_of(find_node_label("flowchart TD\n  A[\"a#124;b\"]", "A")),
            Some("a|b".to_string()),
            "reading a label undoes the escapes that writing it applied"
        );
        // A plain label stays unquoted so the source keeps the shape the user
        // wrote it in.
        assert_eq!(
            replace_node("flowchart TD\n  A[old]", "A", "\u{65b0}\u{3057}\u{3044}").as_deref(),
            Some("flowchart TD\n  A[\u{65b0}\u{3057}\u{3044}]")
        );
        // A single-line input cannot contain a newline, but a paste can.
        assert_eq!(
            encode_label("a\nb", MermaidLabelQuoting::Delimited, "old"),
            "a b"
        );
    }

    #[test]
    fn quoted_labels_decode_their_escapes() {
        let span = find_node_label("flowchart TD\n  A[\"say #quot;hi#quot;\"]", "A")
            .expect("label should be found");
        assert_eq!(span.label, "say \"hi\"");
    }

    #[test]
    fn japanese_labels_are_rewritten_by_byte_range() {
        let source = "flowchart TD\n  A[\u{958b}\u{59cb}] --> B{\u{5224}\u{5b9a}}\n  B -->|\u{306f}\u{3044}| C[\u{51e6}\u{7406}]";
        assert_eq!(
            label_of(find_node_label(source, "B")),
            Some("\u{5224}\u{5b9a}".to_string())
        );
        assert_eq!(
            replace_node(source, "A", "\u{958b}\u{59cb}\u{3059}\u{308b}").as_deref(),
            Some(
                "flowchart TD\n  A[\u{958b}\u{59cb}\u{3059}\u{308b}] --> B{\u{5224}\u{5b9a}}\n  B -->|\u{306f}\u{3044}| C[\u{51e6}\u{7406}]"
            )
        );
        assert_eq!(
            replace_edge(source, "B", "C", 0, "\u{3044}\u{3044}\u{3048}").as_deref(),
            Some(
                "flowchart TD\n  A[\u{958b}\u{59cb}] --> B{\u{5224}\u{5b9a}}\n  B -->|\u{3044}\u{3044}\u{3048}| C[\u{51e6}\u{7406}]"
            )
        );
    }

    /// Comments, subgraph headers and directive lines must not be mistaken for
    /// declarations, and a bracket inside a quoted label must not end a shape.
    #[test]
    fn comments_subgraphs_and_quoted_brackets_are_not_declarations() {
        let source = concat!(
            "flowchart TD\n",
            "  %% A[commented out]\n",
            "  subgraph A[cluster]\n",
            "    B[\"holds A[not a node]\"]\n",
            "  end\n",
            "  A[real] --> B\n"
        );
        assert_eq!(
            label_of(find_node_label(source, "A")),
            Some("real".to_string())
        );
        assert_eq!(
            label_of(find_node_label(source, "B")),
            Some("holds A[not a node]".to_string())
        );
        assert_eq!(
            replace_node(source, "B", "plain").as_deref(),
            Some(concat!(
                "flowchart TD\n",
                "  %% A[commented out]\n",
                "  subgraph A[cluster]\n",
                "    B[plain]\n",
                "  end\n",
                "  A[real] --> B\n"
            ))
        );
    }

    /// merman reads `%%` as a comment only where no label token is open, so a
    /// label that spells one is still that element's label. Cutting the line
    /// there leaves the shape unreadable, and the lookup then falls back to an
    /// older declaration or to the next edge — silently rewriting a token the
    /// user is not looking at.
    #[test]
    fn a_percent_pair_inside_a_label_is_not_a_comment() {
        let twice = "flowchart TD\n  A[first]\n  A[second with %% inside]\n";
        assert_eq!(
            label_of(find_node_label(twice, "A")),
            Some("second with %% inside".to_string())
        );
        assert_eq!(
            replace_node(twice, "A", "x").as_deref(),
            Some("flowchart TD\n  A[first]\n  A[x]\n")
        );

        let edges = "flowchart TD\n  A -->|one %% two| B\n  A -->|x| B\n";
        assert_eq!(
            label_of(find_edge_label(edges, "A", "B", 0)),
            Some("one %% two".to_string())
        );
        assert_eq!(
            label_of(find_edge_label(edges, "A", "B", 1)),
            Some("x".to_string())
        );

        // A state diagram's descriptions and transition labels have no closing
        // delimiter, so everything after the `:` is drawn, `%%` included.
        let state = concat!(
            "stateDiagram-v2\n",
            "  direction LR %% not drawn\n",
            "  a --> b : go %% note\n",
            "  s1 : desc %% note\n",
            "  [*] --> s1\n",
        );
        assert_eq!(
            label_of(find_transition_label(state, 0)),
            Some("go %% note".to_string())
        );
        assert_eq!(
            label_of(find_node_label(state, "s1")),
            Some("desc %% note".to_string())
        );

        // A `%%` written where no label token is open is still a comment.
        let trailing = "flowchart TD\n  A[x] --> B[y] %% A[z]\n";
        assert_eq!(
            label_of(find_node_label(trailing, "A")),
            Some("x".to_string())
        );
        assert_eq!(
            find_node_label(trailing, "z"),
            Err(MermaidLabelRefusal::NotLocated)
        );
        assert_eq!(
            label_of(find_node_label("flowchart TD\n  %% A[z]\n  A[x]\n", "A")),
            Some("x".to_string())
        );
    }

    /// A token that holds nothing has an empty range, and the caller's check
    /// that the range still holds the text the span was read from would then
    /// compare two empty strings and pass however far the buffer has moved on,
    /// dropping the new label at whatever now sits at that offset. Every source
    /// below renders, so the refusal is the only thing standing in the way.
    #[test]
    fn empty_label_tokens_are_refused() {
        assert_eq!(
            find_node_label("flowchart TD\n  A[] --> B[y]", "A"),
            Err(MermaidLabelRefusal::EmptyLabel)
        );
        assert_eq!(
            find_edge_label("flowchart TD\n  A[x] -->|| B[y]", "A", "B", 0),
            Err(MermaidLabelRefusal::EmptyLabel)
        );
        assert_eq!(
            find_edge_label("flowchart TD\n  A[x] -- --> B[y]", "A", "B", 0),
            Err(MermaidLabelRefusal::EmptyLabel)
        );
        assert_eq!(
            find_node_label("stateDiagram-v2\n  s1 :\n  s1 --> s2 : go", "s1"),
            Err(MermaidLabelRefusal::EmptyLabel)
        );
        assert_eq!(
            find_transition_label("stateDiagram-v2\n  a --> b :\n", 0),
            Err(MermaidLabelRefusal::EmptyLabel)
        );
    }

    /// Node ids that contain characters the link syntax also uses must not be
    /// cut short.
    #[test]
    fn node_ids_may_contain_dashes_and_dots() {
        let source = "flowchart TD\n  step-1[one] --> step.2[two]";
        assert_eq!(
            label_of(find_node_label(source, "step-1")),
            Some("one".to_string())
        );
        assert_eq!(
            find_edge_label(source, "step-1", "step.2", 0),
            Err(MermaidLabelRefusal::NotLocated),
            "an unlabelled edge has nothing to rewrite"
        );
        assert_eq!(
            replace_node(source, "step.2", "two!").as_deref(),
            Some("flowchart TD\n  step-1[one] --> step.2[two!]")
        );
    }

    /// The whole point of this module is that the token it finds is the one
    /// the user is looking at, so the labels it reads have to agree with the
    /// ones merman actually drew, and an edge's position among its parallels
    /// has to be counted the same way on both sides. They are counted
    /// independently — the renderer walks `<g class="edgePaths">`, this module
    /// walks the statements — so a chain, an `&` group or a mix of link styles
    /// could easily drift apart and rewrite the wrong edge.
    #[test]
    fn the_renderer_and_the_source_agree_on_every_label() {
        let sources = [
            "flowchart TD\n  A[one] --> B[two] --> C[three]",
            "flowchart TD\n  A -->|one| B -->|two| A -->|three| B",
            "flowchart TD\n  A[a] & B[b] -->|x| C[c] & D[d]",
            "flowchart LR\n  A -->|p| B\n  A -- q --> B\n  A -.r.-> B\n  A == s ==> B",
            "flowchart TD\n  A[one] --> B\n  A[two] --> C",
            "flowchart TD\n  A[\u{958b}\u{59cb}] -->|\u{306f}\u{3044}| B{\u{5224}\u{5b9a}}\n  subgraph S[\u{7fa4}]\n    B -- \u{3044}\u{3044}\u{3048} --> C((\u{7d42}\u{4e86}))\n  end",
            "flowchart TD\n  A[first]\n  A[second with %% inside]",
            "flowchart TD\n  A -->|one %% two| B\n  A -->|x| B",
            "flowchart TD\n  A[x] --> B[y] %% A[z]",
            "flowchart TD\n  A(50%% \u{5f15}) --> B{40%% \u{5f15}}",
        ];
        for source in sources {
            let (_, geometry) = mermaid_render::render_to_svg_with_geometry(
                source,
                &mermaid_render::MermaidTheme::default(),
            )
            .unwrap_or_else(|error| panic!("{source:?} should render: {error:#}"));
            assert!(
                !geometry.nodes.is_empty(),
                "{source:?} mapped no nodes, so this case tests nothing"
            );

            for node in &geometry.nodes {
                let Ok(span) = find_node_label(source, &node.node_id) else {
                    continue;
                };
                assert_eq!(
                    span.label, node.label,
                    "node {} of {source:?} is drawn as {:?} but the source token says {:?}",
                    node.node_id, node.label, span.label
                );
            }

            for edge in &geometry.edges {
                match find_edge_label(source, &edge.from, &edge.to, edge.occurrence) {
                    // A shared token is refused rather than read, but only
                    // where there is one: the refusal must never stand in for
                    // an edge whose label the parser simply lost.
                    Err(MermaidLabelRefusal::SharedByGroup) => assert!(
                        edge.label.is_some(),
                        "edge {}->{} #{} of {source:?} is drawn without a label, \
                         so there is no shared token to refuse",
                        edge.from,
                        edge.to,
                        edge.occurrence
                    ),
                    found => assert_eq!(
                        found.ok().map(|span| span.label).as_deref(),
                        edge.label.as_deref(),
                        "edge {}->{} #{} of {source:?}",
                        edge.from,
                        edge.to,
                        edge.occurrence
                    ),
                }
            }
        }
    }

    /// The geometry merman reports for `source`, which is what a double-click
    /// is resolved against.
    fn geometry_of(source: &str) -> mermaid_render::DiagramGeometry {
        let (_, geometry) = mermaid_render::render_to_svg_with_geometry(
            source,
            &mermaid_render::MermaidTheme::default(),
        )
        .unwrap_or_else(|error| panic!("{source:?} should render: {error:#}"));
        geometry
    }

    /// Applies a span the way the preview does, then renders the result, so a
    /// test can assert that the edit both lands and stays valid mermaid.
    fn apply(source: &str, span: &MermaidLabelSpan, new_label: &str) -> String {
        let replaced = source
            .get(span.range.clone())
            .expect("a span addresses its own source");
        let written = encode_label(new_label, span.quoting, replaced);
        let mut edited = source.to_string();
        edited.replace_range(span.range.clone(), &written);
        edited
    }

    /// An ER relationship's label follows the `:`, and is written quoted
    /// because merman's lexer reserves the bare words a cardinality may be
    /// spelled with (`one`, `many`).
    #[test]
    fn er_relationship_labels_are_rewritten() {
        let source = concat!(
            "erDiagram\n",
            "  \u{9867}\u{5ba2} ||--o{ \u{6ce8}\u{6587} : \"\u{6ce8}\u{6587}\u{3059}\u{308b}\"\n",
            "  \u{9867}\u{5ba2} ||--o{ \u{6ce8}\u{6587} : second\n",
        );
        assert_eq!(
            label_of(find_edge_label(
                source,
                "\u{9867}\u{5ba2}",
                "\u{6ce8}\u{6587}",
                0
            )),
            Some("\u{6ce8}\u{6587}\u{3059}\u{308b}".to_string())
        );
        let span = find_edge_label(source, "\u{9867}\u{5ba2}", "\u{6ce8}\u{6587}", 1)
            .expect("the second relationship should be found");
        assert_eq!(span.label, "second");
        let edited = apply(source, &span, "\u{4e8c}\u{756a}\u{76ee}: \u{6ce8}");
        assert_eq!(
            edited,
            concat!(
                "erDiagram\n",
                "  \u{9867}\u{5ba2} ||--o{ \u{6ce8}\u{6587} : \"\u{6ce8}\u{6587}\u{3059}\u{308b}\"\n",
                "  \u{9867}\u{5ba2} ||--o{ \u{6ce8}\u{6587} : \"\u{4e8c}\u{756a}\u{76ee}: \u{6ce8}\"\n",
            )
        );
        let labels: Vec<_> = geometry_of(&edited)
            .edges
            .iter()
            .map(|edge| edge.label.clone())
            .collect();
        assert_eq!(
            labels,
            vec![
                Some("\u{6ce8}\u{6587}\u{3059}\u{308b}".to_string()),
                Some("\u{4e8c}\u{756a}\u{76ee}: \u{6ce8}".to_string())
            ],
            "the edited source has to draw the new label"
        );

        assert_eq!(
            find_edge_label(source, "\u{9867}\u{5ba2}", "\u{6ce8}\u{6587}", 2),
            Err(MermaidLabelRefusal::NotLocated)
        );
    }

    /// A quotation mark in an ER relationship label is written as `#quot;`,
    /// and merman draws it as the character.
    #[test]
    fn a_quotation_mark_in_an_er_relationship_label_is_drawn() {
        let source = "erDiagram\n  a ||--o{ b : old\n";
        let span = find_edge_label(source, "a", "b", 0).expect("the relationship should be found");
        let edited = apply(source, &span, "\u{8a00}\u{3046} \"\u{306f}\u{3044}\"");
        assert_eq!(
            edited,
            "erDiagram\n  a ||--o{ b : \"\u{8a00}\u{3046} #quot;\u{306f}\u{3044}#quot;\"\n"
        );
        let svg = mermaid_render::render_to_svg(&edited, &mermaid_render::MermaidTheme::default())
            .unwrap_or_else(|error| panic!("{edited:?} should render: {error:#}"));
        // merman 0.7 put each word in a `<tspan>` of its own; 0.8 writes the
        // label as one text, with the quotes escaped or not.
        let label = "\u{8a00}\u{3046} \"\u{306f}\u{3044}\"";
        assert!(
            svg.contains(&format!(">{label}<"))
                || svg.contains(&format!(">{}<", label.replace('"', "&quot;"))),
            "the quotation marks have to be drawn"
        );
    }

    /// An ER entity is drawn with its name unless it is given an alias, and
    /// every relationship refers to it by that name, so the name is never
    /// rewritten. merman refuses an alias written into a relationship
    /// statement, so an entity that appears in nothing else has nowhere to put
    /// one.
    #[test]
    fn er_entity_aliases_are_rewritten_or_inserted() {
        let with_alias = concat!(
            "erDiagram\n",
            "  c ||--o{ o : places\n",
            "  c[\u{9867}\u{5ba2}] {\n",
            "    string \u{540d}\u{524d}\n",
            "  }\n",
        );
        let span = find_node_label(with_alias, "c").expect("the alias should be found");
        assert_eq!(span.label, "\u{9867}\u{5ba2}");
        assert_eq!(span.quoting, MermaidLabelQuoting::ErAlias);
        let edited = apply(with_alias, &span, "\u{304a}\u{5ba2}\u{69d8}");
        assert!(
            edited.contains("c[\"\u{304a}\u{5ba2}\u{69d8}\"] {"),
            "{edited:?}"
        );
        assert_eq!(
            geometry_of(&edited)
                .nodes
                .iter()
                .find(|node| node.node_id == "c")
                .map(|node| node.label.clone()),
            Some("\u{304a}\u{5ba2}\u{69d8}".to_string())
        );

        // An entity declared without one gains a whole `["…"]`, whether the
        // declaration heads an attribute block or stands alone.
        let block = "erDiagram\n  c ||--o{ o : places\n  c {\n    string n\n  }\n";
        let span = find_node_label(block, "c").expect("an insertion point should be found");
        assert_eq!(span.quoting, MermaidLabelQuoting::ErAliasInsert);
        assert_eq!(span.label, "c", "the name is what the box is drawn with");
        let edited = apply(block, &span, "\u{9867}\u{5ba2}");
        assert_eq!(
            edited,
            "erDiagram\n  c ||--o{ o : places\n  c[\"\u{9867}\u{5ba2}\"] {\n    string n\n  }\n"
        );
        assert_eq!(
            geometry_of(&edited)
                .nodes
                .iter()
                .find(|node| node.node_id == "c")
                .map(|node| node.label.clone()),
            Some("\u{9867}\u{5ba2}".to_string())
        );

        let standalone = "erDiagram\n  c\n  c ||--o{ o : places\n";
        let span = find_node_label(standalone, "c").expect("an insertion point should be found");
        assert_eq!(
            apply(standalone, &span, "\u{9867}\u{5ba2}"),
            "erDiagram\n  c[\"\u{9867}\u{5ba2}\"]\n  c ||--o{ o : places\n"
        );

        // `o` is only ever an end of a relationship, which merman refuses an
        // alias in, and `x` is not in the diagram at all.
        assert_eq!(
            find_node_label(block, "o"),
            Err(MermaidLabelRefusal::NoLabelToken)
        );
        assert_eq!(
            find_node_label(block, "x"),
            Err(MermaidLabelRefusal::NotLocated)
        );
    }

    /// The `{` of a `||--o{` cardinality is not the start of an attribute
    /// block. Reading it as one would hide every statement below it, so an
    /// entity declared further down would look undeclarable.
    #[test]
    fn a_cardinality_brace_does_not_open_an_attribute_block() {
        let source = "erDiagram\n  a ||--o{ b : x\n  b ||--|{ c : y\n  c {\n    string n\n  }\n";
        assert_eq!(
            label_of(find_edge_label(source, "b", "c", 0)),
            Some("y".to_string())
        );
        let span = find_node_label(source, "c").expect("c is declared with a block");
        assert_eq!(span.quoting, MermaidLabelQuoting::ErAliasInsert);
        // The attribute itself is not a declaration of an entity called
        // `string`.
        assert_eq!(
            find_node_label(source, "string"),
            Err(MermaidLabelRefusal::NotLocated)
        );
    }

    /// A state diagram's SVG numbers its transitions instead of naming their
    /// ends, counting the statements in document order — a composite state's
    /// body included, where it stands. The source has to be counted the same
    /// way or a double-click rewrites a different transition's label.
    #[test]
    fn state_transitions_are_numbered_like_the_renderer() {
        let source = concat!(
            "stateDiagram-v2\n",
            "  a --> b : one\n",
            "  %% a --> b : commented out\n",
            "  state C {\n",
            "    x --> y : inner\n",
            "  }\n",
            "  note right of a\n",
            "    where it all starts\n",
            "  end note\n",
            "  b --> C : two\n",
            "  note left of b : short\n",
            "  C --> d : three\n",
        );
        // Each note takes a place in the count as well, since merman draws one
        // as a connection between its box and its state.
        for (index, label) in [(0, "one"), (1, "inner"), (3, "two"), (5, "three")] {
            assert_eq!(
                label_of(find_transition_label(source, index)),
                Some(label.to_string()),
                "transition {index}"
            );
        }
        for index in [2, 4, 6] {
            assert_eq!(
                find_transition_label(source, index),
                Err(MermaidLabelRefusal::NotLocated),
                "transition {index}"
            );
        }

        // The renderer's own numbering is the one that matters.
        for edge in &geometry_of(source).edges {
            let index = edge.statement_index.expect("a numbered transition");
            assert_eq!(
                label_of(find_transition_label(source, index)).as_deref(),
                edge.label.as_deref(),
                "transition {index}"
            );
        }

        let span = find_transition_label(source, 1).expect("the inner transition");
        assert_eq!(
            apply(source, &span, "\u{4e2d}\u{8eab}: 1"),
            source.replace("x --> y : inner", "x --> y : \u{4e2d}\u{8eab}#58; 1")
        );
        assert_eq!(
            geometry_of(&apply(source, &span, "\u{4e2d}\u{8eab}: 1"))
                .edges
                .iter()
                .find(|edge| edge.statement_index == Some(1))
                .and_then(|edge| edge.label.clone()),
            Some("\u{4e2d}\u{8eab}: 1".to_string())
        );
    }

    /// A chain writes two transitions on one line and a one-line composite
    /// hides however many are inside it, so the numbering cannot be reproduced
    /// and nothing is offered rather than the wrong token.
    #[test]
    fn state_transitions_that_cannot_be_numbered_are_refused() {
        for source in [
            "stateDiagram-v2\n  a --> b --> c : one\n",
            "stateDiagram-v2\n  a --> b : one\n  state C { x --> y : inner }\n",
            "stateDiagram-v2\n  a --> b : one\n  note right of a\n    c --> d\n  end note\n",
        ] {
            assert_eq!(
                find_transition_label(source, 0),
                Err(MermaidLabelRefusal::NotLocated),
                "{source:?}"
            );
        }
    }

    /// A state's box is drawn with its description, in either of the two forms
    /// that write one. A state with none is drawn with its id, which every
    /// transition refers to, so it is left alone.
    #[test]
    fn state_descriptions_are_rewritten() {
        let bare =
            "stateDiagram-v2\n  s1 : \u{5f85}\u{6a5f}\u{4e2d}\n  [*] --> s1\n  s1 --> s2 : go\n";
        let span = find_node_label(bare, "s1").expect("the description should be found");
        assert_eq!(span.label, "\u{5f85}\u{6a5f}\u{4e2d}");
        assert_eq!(span.quoting, MermaidLabelQuoting::StateBare);
        let edited = apply(bare, &span, "\u{5f85}\u{3061}: 1");
        assert_eq!(
            edited,
            "stateDiagram-v2\n  s1 : \u{5f85}\u{3061}#58; 1\n  [*] --> s1\n  s1 --> s2 : go\n"
        );
        assert_eq!(
            geometry_of(&edited)
                .nodes
                .iter()
                .find(|node| node.node_id == "s1")
                .map(|node| node.label.clone()),
            Some("\u{5f85}\u{3061}: 1".to_string())
        );

        let quoted = "stateDiagram-v2\n  state \"\u{8aac}\u{660e}\" as s1\n  [*] --> s1\n";
        let span = find_node_label(quoted, "s1").expect("the description should be found");
        assert_eq!(span.label, "\u{8aac}\u{660e}");
        assert_eq!(span.quoting, MermaidLabelQuoting::StateQuoted);
        let edited = apply(quoted, &span, "\u{65b0}\u{3057}\u{3044}\u{8aac}\u{660e}");
        assert_eq!(
            edited,
            "stateDiagram-v2\n  state \"\u{65b0}\u{3057}\u{3044}\u{8aac}\u{660e}\" as s1\n  [*] --> s1\n"
        );
        assert_eq!(
            geometry_of(&edited)
                .nodes
                .iter()
                .find(|node| node.node_id == "s1")
                .map(|node| node.label.clone()),
            Some("\u{65b0}\u{3057}\u{3044}\u{8aac}\u{660e}".to_string())
        );

        assert_eq!(
            find_node_label(bare, "s2"),
            Err(MermaidLabelRefusal::NoLabelToken),
            "a state drawn with its id has no token to rewrite"
        );
        assert_eq!(
            find_node_label(bare, "s3"),
            Err(MermaidLabelRefusal::NotLocated)
        );
        // Mermaid draws both of these, so neither is the label.
        assert_eq!(
            find_node_label(
                "stateDiagram-v2\n  state \"one\" as s1\n  s1 : two\n  [*] --> s1\n",
                "s1"
            ),
            Err(MermaidLabelRefusal::NotLocated)
        );
    }

    /// A `style` or `classDef` statement holds a `:` of its own, and a class
    /// assignment spells one three times over. Neither writes a description.
    #[test]
    fn state_statements_with_colons_are_not_descriptions() {
        let source = concat!(
            "stateDiagram-v2\n",
            "  classDef warn fill:#fdd\n",
            "  style s1 fill:#eef\n",
            "  s1:::warn : \u{5f85}\u{6a5f}\n",
            "  [*] --> s1\n",
        );
        let span = find_node_label(source, "s1").expect("the description should be found");
        assert_eq!(span.label, "\u{5f85}\u{6a5f}");
        assert_eq!(
            find_node_label(source, "warn"),
            Err(MermaidLabelRefusal::NotLocated)
        );
        assert_eq!(
            apply(source, &span, "x"),
            source.replace("s1:::warn : \u{5f85}\u{6a5f}", "s1:::warn : x")
        );
    }

    /// A state drawn with its id gets a description line of its own, written
    /// in front of the first statement that opens with the state, at that
    /// statement's indentation and so inside the same composite state.
    #[test]
    fn a_state_without_a_description_is_given_one_on_a_new_line() {
        let source = concat!(
            "stateDiagram-v2\n",
            "  [*] --> s1\n",
            "  state P {\n",
            "    s1:::warn --> s2 : go\n",
            "  }\n",
            "  classDef warn fill:#fdd\n",
        );
        let span = find_node_label(source, "s1").expect("s1 should be offered a description");
        assert_eq!(span.quoting, MermaidLabelQuoting::StateInsert);
        assert_eq!(span.label, "s1", "the field opens on what the box shows");
        assert_eq!(&source[span.range.clone()], "    s1");

        let edited = apply(source, &span, "\u{5f85}\u{6a5f}: 1");
        assert_eq!(
            edited,
            concat!(
                "stateDiagram-v2\n",
                "  [*] --> s1\n",
                "  state P {\n",
                "    s1 : \u{5f85}\u{6a5f}#58; 1\n",
                "    s1:::warn --> s2 : go\n",
                "  }\n",
                "  classDef warn fill:#fdd\n",
            )
        );
        assert_eq!(
            geometry_of(&edited)
                .nodes
                .iter()
                .find(|node| node.node_id == "s1")
                .map(|node| node.label.clone()),
            Some("\u{5f85}\u{6a5f}: 1".to_string())
        );
        // Confirming the field as it opened, or emptied, writes nothing.
        assert_eq!(apply(source, &span, "s1"), source);
        assert_eq!(apply(source, &span, ""), source);

        // Once written, the description is the label, found in the usual way.
        assert_eq!(
            find_node_label(&edited, "s1").map(|span| span.quoting),
            Ok(MermaidLabelQuoting::StateBare)
        );

        // A state that no statement opens with has nowhere to put one.
        assert_eq!(
            find_node_label(source, "s2"),
            Err(MermaidLabelRefusal::NoLabelToken)
        );
        // A line of an `accDescr` block is prose, not a statement.
        let described =
            "stateDiagram-v2\n  accDescr {\n    s9 is mentioned here\n  }\n  [*] --> s9\n";
        assert_eq!(
            find_node_label(described, "s9"),
            Err(MermaidLabelRefusal::NoLabelToken)
        );
        // A note's text is not a statement either.
        let noted = "stateDiagram-v2\n  [*] --> s3\n  note right of s3\n    s3 again\n  end note\n";
        assert_eq!(
            find_node_label(noted, "s3"),
            Err(MermaidLabelRefusal::NoLabelToken)
        );
    }

    /// The whole point of this module is that the token it finds is the one the
    /// user is looking at, so for every kind the labels it reads have to agree
    /// with the ones merman drew.
    #[test]
    fn the_renderer_and_the_source_agree_for_every_kind() {
        let sources = [
            concat!(
                "erDiagram\n",
                "  \u{9867}\u{5ba2} ||--o{ \u{6ce8}\u{6587} : \"\u{6ce8}\u{6587}\u{3059}\u{308b}\"\n",
                "  \u{6ce8}\u{6587} }|..|{ \u{5546}\u{54c1} : contains\n",
                "  \u{9867}\u{5ba2}[\u{304a}\u{5ba2}\u{69d8}] {\n",
                "    string \u{540d}\u{524d}\n",
                "  }\n",
            ),
            concat!(
                "stateDiagram-v2\n",
                "  [*] --> \u{5f85}\u{6a5f}\n",
                "  \u{5f85}\u{6a5f} --> \u{5b9f}\u{884c} : \u{958b}\u{59cb}\n",
                "  state \u{8907}\u{5408} {\n",
                "    x --> y : \u{4e2d}\u{8eab}\n",
                "  }\n",
                "  \u{5b9f}\u{884c} --> [*]\n",
                "  \u{5b9f}\u{884c} : \u{52d5}\u{4f5c}\u{4e2d}\n",
            ),
            concat!(
                "stateDiagram-v2\n",
                "  direction LR %% \u{63cf}\u{753b}\u{3055}\u{308c}\u{306a}\u{3044}\n",
                "  a --> b : go %% note\n",
                "  s1 : desc %% note\n",
                "  [*] --> s1\n",
            ),
            concat!(
                "erDiagram\n",
                "  %% \u{5148}\u{982d}\u{306e}\u{30e1}\u{30e2}\n",
                "  a ||--o{ b : \"50%% \u{5f15}\"\n",
                "  a[\u{5225}\u{540d}]\n",
            ),
        ];
        for source in sources {
            let geometry = geometry_of(source);
            assert!(!geometry.nodes.is_empty(), "{source:?} mapped no nodes");
            for node in &geometry.nodes {
                let Ok(span) = find_node_label(source, &node.node_id) else {
                    continue;
                };
                assert_eq!(
                    span.label, node.label,
                    "node {} of {source:?} is drawn as {:?} but the source token says {:?}",
                    node.node_id, node.label, span.label
                );
            }
            for edge in &geometry.edges {
                let found = match edge.statement_index {
                    Some(index) => find_transition_label(source, index),
                    None => find_edge_label(source, &edge.from, &edge.to, edge.occurrence),
                };
                assert_eq!(
                    found.ok().map(|span| span.label).as_deref(),
                    edge.label.as_deref(),
                    "edge {:?} of {source:?}",
                    edge.edge_id
                );
            }
        }
    }

    /// An edit only ever touches the one token that was found.
    #[test]
    fn the_rest_of_the_source_is_untouched() {
        let source = "flowchart LR\n  A[one] -->|go| B[two]\n  B --> C[three]";
        let span = find_node_label(source, "B").expect("label should be found");
        assert_eq!(&source[span.range], "two");
        assert_eq!(
            replace_node(source, "B", "TWO").as_deref(),
            Some("flowchart LR\n  A[one] -->|go| B[TWO]\n  B --> C[three]")
        );
    }
}
