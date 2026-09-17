//! Locates the label of a flowchart node or edge inside a mermaid fence body,
//! so that editing a label in the rendered preview can be written back as a
//! single replacement of the token the user actually wrote.
//!
//! This is a deliberately small parser: it walks each statement of a flowchart
//! far enough to know which node ids are declared with which shape, and which
//! edges connect them in which order. Anything it cannot read confidently is
//! reported as "not found" so the caller leaves the source alone, which is
//! always safe.

use std::ops::Range;

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
}

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

/// The label the renderer draws for `node_id`, and where it is written.
///
/// A node may be declared more than once, and mermaid keeps the last label it
/// was given (`A[one] --> B` followed by `A[two] --> C` draws "two"), so the
/// last declaration that carries one is the token the user is looking at.
/// Declarations that are bare references (`A --> B`, where `B` never gets a
/// shape) carry no label token and are skipped; a node that has none at all
/// yields `None`.
pub(crate) fn find_node_label(fence_body: &str, node_id: &str) -> Option<MermaidLabelSpan> {
    parse_items(fence_body)
        .into_iter()
        .filter_map(|item| match item {
            Item::Node { id, span } if id == node_id => span,
            _ => None,
        })
        .next_back()
}

/// The label of the `occurrence`-th edge from `from` to `to`, counting in
/// source order.
pub(crate) fn find_edge_label(
    fence_body: &str,
    from: &str,
    to: &str,
    occurrence: usize,
) -> Option<MermaidLabelSpan> {
    let mut seen = 0;
    for item in parse_items(fence_body) {
        let Item::Edge {
            from: edge_from,
            to: edge_to,
            span,
        } = item
        else {
            continue;
        };
        if edge_from != from || edge_to != to {
            continue;
        }
        if seen == occurrence {
            return span;
        }
        seen += 1;
    }
    None
}

/// Writes `label` in the form the slot it replaces expects.
///
/// Newlines cannot appear in a mermaid label token at all, so they collapse to
/// spaces rather than breaking the statement in two.
pub(crate) fn encode_label(label: &str, quoting: MermaidLabelQuoting) -> String {
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
    let escaped = flattened.replace('"', "#quot;");
    let needs_quotes = escaped.is_empty()
        || escaped.trim() != escaped
        || escaped
            .chars()
            .any(|character| "[](){}|<>".contains(character))
        || escaped.contains("#quot;");
    match quoting {
        MermaidLabelQuoting::Metadata => format!("\"{escaped}\""),
        MermaidLabelQuoting::Delimited if needs_quotes => format!("\"{escaped}\""),
        MermaidLabelQuoting::Delimited => escaped,
    }
}

/// The inverse of [`encode_label`]: the label as the user sees it.
fn decode_label(raw: &str) -> String {
    let trimmed = raw.trim();
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(trimmed);
    unquoted.replace("#quot;", "\"")
}

enum Item {
    Node {
        id: String,
        span: Option<MermaidLabelSpan>,
    },
    Edge {
        from: String,
        to: String,
        span: Option<MermaidLabelSpan>,
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

/// Truncates `line` at a `%%` comment that starts outside a quoted string.
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut index = 0;
    let mut in_quotes = false;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => in_quotes = !in_quotes,
            b'%' if !in_quotes && bytes.get(index + 1) == Some(&b'%') => {
                return &line[..index];
            }
            _ => {}
        }
        index += 1;
    }
    line.trim_end_matches(['\n', '\r'])
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
        let id = parse_identifier(statement, position)?;
        let span = match_shape(statement, *position).map(|shape| {
            *position = shape.end;
            MermaidLabelSpan {
                range: (shape.label.range.start + offset)..(shape.label.range.end + offset),
                label: shape.label.label,
                quoting: shape.label.quoting,
            }
        });
        items.push(Item::Node {
            id: id.clone(),
            span,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Applies the edit the preview would make, so a test reads as the source
    /// before and after.
    fn replace_node(fence_body: &str, node_id: &str, new_label: &str) -> Option<String> {
        let span = find_node_label(fence_body, node_id)?;
        let mut edited = fence_body.to_string();
        edited.replace_range(span.range, &encode_label(new_label, span.quoting));
        Some(edited)
    }

    fn replace_edge(
        fence_body: &str,
        from: &str,
        to: &str,
        occurrence: usize,
        new_label: &str,
    ) -> Option<String> {
        let span = find_edge_label(fence_body, from, to, occurrence)?;
        let mut edited = fence_body.to_string();
        edited.replace_range(span.range, &encode_label(new_label, span.quoting));
        Some(edited)
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
                find_node_label(before, "A").map(|span| span.label),
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
                find_edge_label(before, "A", "B", 0).map(|span| span.label),
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
            find_node_label(source, "A").map(|span| span.label),
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
            find_node_label(
                "flowchart TD\n  A@{ shape: rect, tooltip: \u{65e5}\u{672c}\u{8a9e}, label: \"x\" }",
                "A"
            )
            .map(|span| span.label),
            Some("x".to_string())
        );
        assert_eq!(
            find_node_label(
                "flowchart TD\n  A@{ shape: rect, tooltip: \u{65e5}\u{672c}\u{8a9e} }",
                "A"
            ),
            None
        );
        assert_eq!(
            find_node_label("flowchart TD\n  A@{ \u{8aac}\u{660e} }", "A"),
            None
        );
    }

    /// A node that is only ever referenced has no token to rewrite, and a node
    /// id that does not occur at all is likewise not found. Neither may be
    /// guessed at, since a wrong guess corrupts the user's source.
    #[test]
    fn unlabelled_and_unknown_nodes_are_not_found() {
        assert_eq!(find_node_label("flowchart TD\n  A[one] --> B", "B"), None);
        assert_eq!(find_node_label("flowchart TD\n  A[one] --> B", "C"), None);
        assert_eq!(
            find_edge_label("flowchart TD\n  A --> B", "A", "B", 0),
            None
        );
        assert_eq!(
            find_edge_label("flowchart TD\n  A -->|one| B", "A", "B", 1),
            None
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
            find_edge_label(source, "A", "B", 0).map(|span| span.label),
            Some("first".to_string())
        );
    }

    /// The edges of a chain are numbered in the order they were written, which
    /// is the order the renderer lays them out in.
    #[test]
    fn chains_and_ampersand_groups_produce_edges_in_source_order() {
        let source = "flowchart TD\n  A -->|one| B -->|two| C\n  B & C -->|three| D";
        assert_eq!(
            find_edge_label(source, "A", "B", 0).map(|span| span.label),
            Some("one".to_string())
        );
        assert_eq!(
            find_edge_label(source, "B", "C", 0).map(|span| span.label),
            Some("two".to_string())
        );
        assert_eq!(
            find_edge_label(source, "B", "D", 0).map(|span| span.label),
            Some("three".to_string())
        );
        assert_eq!(
            find_edge_label(source, "C", "D", 0).map(|span| span.label),
            Some("three".to_string())
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
        assert_eq!(
            replace_edge("flowchart TD\n  A -->|old| B", "A", "B", 0, "yes|no").as_deref(),
            Some("flowchart TD\n  A -->|\"yes|no\"| B")
        );
        // A plain label stays unquoted so the source keeps the shape the user
        // wrote it in.
        assert_eq!(
            replace_node("flowchart TD\n  A[old]", "A", "\u{65b0}\u{3057}\u{3044}").as_deref(),
            Some("flowchart TD\n  A[\u{65b0}\u{3057}\u{3044}]")
        );
        // A single-line input cannot contain a newline, but a paste can.
        assert_eq!(encode_label("a\nb", MermaidLabelQuoting::Delimited), "a b");
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
            find_node_label(source, "B").map(|span| span.label),
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
            find_node_label(source, "A").map(|span| span.label),
            Some("real".to_string())
        );
        assert_eq!(
            find_node_label(source, "B").map(|span| span.label),
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

    /// Node ids that contain characters the link syntax also uses must not be
    /// cut short.
    #[test]
    fn node_ids_may_contain_dashes_and_dots() {
        let source = "flowchart TD\n  step-1[one] --> step.2[two]";
        assert_eq!(
            find_node_label(source, "step-1").map(|span| span.label),
            Some("one".to_string())
        );
        assert_eq!(
            find_edge_label(source, "step-1", "step.2", 0),
            None,
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
                let Some(span) = find_node_label(source, &node.node_id) else {
                    continue;
                };
                assert_eq!(
                    span.label, node.label,
                    "node {} of {source:?} is drawn as {:?} but the source token says {:?}",
                    node.node_id, node.label, span.label
                );
            }

            for edge in &geometry.edges {
                let found = find_edge_label(source, &edge.from, &edge.to, edge.occurrence)
                    .map(|span| span.label);
                assert_eq!(
                    found.as_deref(),
                    edge.label.as_deref(),
                    "edge {}->{} #{} of {source:?}",
                    edge.from,
                    edge.to,
                    edge.occurrence
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
