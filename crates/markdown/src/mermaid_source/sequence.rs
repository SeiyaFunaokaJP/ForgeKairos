//! Sequence diagrams: a message's or a note's text, which follows the `:` of
//! its statement, and a participant's display name, which follows the `as` of
//! its declaration.
//!
//! merman's lexer ends every one of those texts at a `;` (which separates
//! statements just like a line break does), at a `#` that does not start a
//! `#…;` entity, and at a `%%`; the last two open a comment that runs to the
//! end of the line. The scan here ends them at the same places, so that the
//! token it reports is the text that is actually drawn.
//!
//! The SVG numbers messages and notes together in the order they were written,
//! which is the count kept here as well. A statement this scan cannot read
//! might write one of them, so it makes every numbered lookup a refusal rather
//! than a guess that would land on a neighbouring message.

use super::{MermaidLabelLookup, MermaidLabelQuoting, MermaidLabelRefusal, MermaidLabelSpan};
use super::{checked, decode_label};

/// What one pass over a sequence diagram's statements found.
struct SequenceScan {
    /// The text of each message and note, in the order the statements wrote
    /// them. `None` for one whose text could not be read.
    items: Vec<Option<MermaidLabelSpan>>,
    /// The display name each `participant … as …` or `actor … as …` gives.
    aliases: Vec<(String, MermaidLabelSpan)>,
    /// Every participant id the source names.
    mentioned: Vec<String>,
    /// Set by a statement this scan does not recognize, which may write a
    /// message or a note of its own and shift the count.
    unreadable: bool,
}

/// Where a run of text that reads to "the end of the line" stops, and why.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Terminator {
    /// A `;`, after which another statement may follow on the same line.
    Semicolon,
    /// A `#` or a `%%`, which comment out the rest of the line.
    Comment,
    EndOfLine,
}

/// Block and meta statements whose argument runs to the end of the statement
/// and names nothing this module edits.
const REST_OF_LINE_KEYWORDS: &[&str] = &[
    "title",
    "accTitle",
    "loop",
    "rect",
    "opt",
    "alt",
    "else",
    "par_over",
    "par",
    "and",
    "critical",
    "option",
    "break",
    "box",
    "autonumber",
    "activate",
    "deactivate",
    "destroy",
    "links",
    "link",
    "properties",
    "details",
];

/// The arrows merman's lexer reads between two participants, longest first so
/// that a prefix never shadows the arrow it starts.
const ARROWS: &[&str] = &[
    "<<-->>", "<<->>", "-->>", "->>", "-->", "->", "--x", "-x", "--)", "-)",
];

/// Whether `text` opens with `keyword` as a whole word, ignoring ASCII case the
/// way merman's sequence lexer does.
fn starts_with_word_ignoring_case(text: &str, keyword: &str) -> bool {
    let Some(head) = text.get(..keyword.len()) else {
        return false;
    };
    if !head.eq_ignore_ascii_case(keyword) {
        return false;
    }
    !text[keyword.len()..]
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn skip_blanks(line: &str, position: &mut usize) {
    while let Some(byte) = line.as_bytes().get(*position) {
        if *byte == b' ' || *byte == b'\t' || *byte == b'\r' {
            *position += 1;
        } else {
            break;
        }
    }
}

/// The length of the `#…;` entity `text` opens with, if it opens with one.
///
/// merman rewrites those into placeholders before it lexes anything, so the
/// `#` and the `;` of one neither open a comment nor end a statement.
fn entity_length(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'#') {
        return None;
    }
    let mut end = 1;
    while bytes
        .get(end)
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        end += 1;
    }
    (end > 1 && bytes.get(end) == Some(&b';')).then_some(end + 1)
}

/// Reads from `start` to where merman's lexer ends a text, returning that end
/// and what ended it.
fn read_rest(line: &str, start: usize) -> (usize, Terminator) {
    let bytes = line.as_bytes();
    let mut position = start;
    while position < bytes.len() {
        match bytes[position] {
            b'\n' => return (position, Terminator::EndOfLine),
            b';' => return (position, Terminator::Semicolon),
            b'#' => match entity_length(&line[position..]) {
                Some(length) => {
                    position += length;
                    continue;
                }
                None => return (position, Terminator::Comment),
            },
            b'%' if bytes.get(position + 1) == Some(&b'%') => {
                return (position, Terminator::Comment);
            }
            _ => {}
        }
        position += 1;
    }
    (bytes.len(), Terminator::EndOfLine)
}

/// Reads a participant id the way merman's lexer does, returning it and where
/// it ends.
fn read_actor(line: &str, start: usize) -> Option<(String, usize)> {
    let bytes = line.as_bytes();
    let mut end = start;
    while end < bytes.len() {
        let byte = bytes[end];
        if byte.is_ascii_whitespace()
            || matches!(byte, b';' | b',' | b':' | b'+' | b'<')
            || (byte == b'@' && bytes.get(end + 1) == Some(&b'{'))
            || (byte == b'-' && matches!(bytes.get(end + 1), Some(b'-' | b'>' | b'x' | b')')))
        {
            break;
        }
        end += 1;
    }
    if end == start {
        return None;
    }
    let id = line.get(start..end)?;
    // `A()->>B` marks a central connection on the sending side; the `()`
    // belongs to the arrow, not to the id.
    let id = id.strip_suffix("()").unwrap_or(id);
    (!id.is_empty()).then(|| (id.to_string(), end))
}

/// The span of a text from `start` to where merman ends it, with the
/// surrounding whitespace and a leading `wrap:` / `nowrap:` directive — which
/// merman reads as a formatting switch rather than as text — left out.
fn text_span(
    line_offset: usize,
    line: &str,
    start: usize,
) -> (MermaidLabelSpan, (usize, Terminator)) {
    let (end, terminator) = read_rest(line, start);
    let mut text_start = start;
    skip_blanks(line, &mut text_start);
    let text_start = text_start.min(end);
    let raw = line[text_start..end].trim_end();
    let mut label_start = text_start;
    if let Some(directive) = wrap_directive_length(raw) {
        label_start += directive;
        skip_blanks(line, &mut label_start);
        label_start = label_start.min(text_start + raw.len());
    }
    let label_end = text_start + raw.len();
    let raw = &line[label_start..label_end];
    (
        MermaidLabelSpan {
            range: (line_offset + label_start)..(line_offset + label_end),
            label: decode_label(raw),
            quoting: MermaidLabelQuoting::SequenceText,
        },
        (end, terminator),
    )
}

/// The length of the `wrap:` / `nowrap:` directive `text` opens with, if any.
pub(super) fn wrap_directive_length(text: &str) -> Option<usize> {
    [":wrap:", "wrap:", ":nowrap:", "nowrap:"]
        .iter()
        .find(|directive| {
            text.get(..directive.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(directive))
        })
        .map(|directive| directive.len())
}

fn scan_sequence(fence_body: &str) -> SequenceScan {
    let mut scan = SequenceScan {
        items: Vec::new(),
        aliases: Vec::new(),
        mentioned: Vec::new(),
        unreadable: false,
    };
    let mut line_offset = 0;
    let mut in_description_block = false;
    for raw_line in fence_body.split_inclusive('\n') {
        let offset = line_offset;
        line_offset += raw_line.len();
        let line = raw_line.trim_end_matches(['\n', '\r']);
        if in_description_block {
            if let Some(close) = line.find('}') {
                in_description_block = false;
                mark_unreadable_after_block(line, close + 1, &mut scan);
            }
            continue;
        }
        let mut position = 0;
        loop {
            skip_blanks(line, &mut position);
            let Some(rest) = line.get(position..) else {
                scan.unreadable = true;
                break;
            };
            if rest.is_empty() {
                break;
            }
            if rest.starts_with("%%") || (rest.starts_with('#') && entity_length(rest).is_none()) {
                break;
            }
            if rest.starts_with(';') {
                position += 1;
                continue;
            }
            let terminator =
                read_statement(offset, line, position, &mut scan, &mut in_description_block);
            match terminator {
                Some((end, Terminator::Semicolon)) => position = end + 1,
                Some((_, Terminator::Comment | Terminator::EndOfLine)) | None => break,
            }
        }
    }
    scan
}

/// Reads the statement starting at `position`, recording what it declares, and
/// returns where it ends and why. `None` means the rest of the line is gone.
fn read_statement(
    offset: usize,
    line: &str,
    position: usize,
    scan: &mut SequenceScan,
    in_description_block: &mut bool,
) -> Option<(usize, Terminator)> {
    let rest = &line[position..];
    if starts_with_word_ignoring_case(rest, "accDescr") {
        let mut after = position + "accDescr".len();
        skip_blanks(line, &mut after);
        if line[after..].starts_with('{') {
            // The block runs to the next `}`, however many lines away.
            match line[after..].find('}') {
                Some(close) => mark_unreadable_after_block(line, after + close + 1, scan),
                None => *in_description_block = true,
            }
            return None;
        }
        return Some(read_rest(line, after));
    }
    // merman's lexer reads the title before anything else, but an id followed
    // by an arrow before any other keyword: `Box->>A: …` is a message from a
    // participant named `Box`, not a `box` block.
    if starts_with_word_ignoring_case(rest, "title")
        && rest["title".len()..]
            .bytes()
            .next()
            .is_none_or(|byte| byte.is_ascii_whitespace() || byte == b':')
    {
        return Some(read_rest(line, position + "title".len()));
    }
    if opens_message(line, position) {
        return Some(read_message(offset, line, position, scan));
    }
    if starts_with_word_ignoring_case(rest, "sequenceDiagram") {
        let after = position + "sequenceDiagram".len();
        return Some(read_rest(line, after));
    }
    if starts_with_word_ignoring_case(rest, "end") {
        return Some(read_rest(line, position + "end".len()));
    }
    if let Some(keyword) = REST_OF_LINE_KEYWORDS
        .iter()
        .find(|keyword| starts_with_word_ignoring_case(rest, keyword))
    {
        return Some(read_rest(line, position + keyword.len()));
    }
    if starts_with_word_ignoring_case(rest, "create") {
        let mut after = position + "create".len();
        skip_blanks(line, &mut after);
        return read_participant(offset, line, after, scan);
    }
    if starts_with_word_ignoring_case(rest, "participant")
        || starts_with_word_ignoring_case(rest, "actor")
    {
        return read_participant(offset, line, position, scan);
    }
    if starts_with_word_ignoring_case(rest, "note") {
        return Some(read_note(offset, line, position + "note".len(), scan));
    }
    Some(read_message(offset, line, position, scan))
}

/// Whether the statement at `position` is an id followed by an arrow, which
/// merman reads as a message whatever word the id is.
fn opens_message(line: &str, position: usize) -> bool {
    let Some((_, mut cursor)) = read_actor(line, position) else {
        return false;
    };
    skip_blanks(line, &mut cursor);
    if line[cursor..].starts_with("()") {
        cursor += 2;
        skip_blanks(line, &mut cursor);
    }
    ARROWS.iter().any(|arrow| line[cursor..].starts_with(arrow))
}

/// merman goes on reading statements right after the `}` that closes a
/// description block, which this scan does not, so anything there makes the
/// count untrustworthy.
fn mark_unreadable_after_block(line: &str, after_close: usize, scan: &mut SequenceScan) {
    let mut position = after_close;
    skip_blanks(line, &mut position);
    let rest = line.get(position..).unwrap_or_default();
    if !(rest.is_empty()
        || rest.starts_with("%%")
        || (rest.starts_with('#') && entity_length(rest).is_none()))
    {
        scan.unreadable = true;
    }
}

/// `participant A`, `participant A as 表示名` and the same with `actor`.
fn read_participant(
    offset: usize,
    line: &str,
    position: usize,
    scan: &mut SequenceScan,
) -> Option<(usize, Terminator)> {
    let rest = &line[position..];
    let keyword = if starts_with_word_ignoring_case(rest, "participant") {
        "participant"
    } else if starts_with_word_ignoring_case(rest, "actor") {
        "actor"
    } else {
        scan.unreadable = true;
        return Some(read_rest(line, position));
    };
    let mut cursor = position + keyword.len();
    skip_blanks(line, &mut cursor);
    let Some((id, after_id)) = read_actor(line, cursor) else {
        scan.unreadable = true;
        return Some(read_rest(line, cursor));
    };
    scan.mentioned.push(id.clone());
    let mut cursor = after_id;
    if line[cursor..].starts_with("@{") {
        // A config object may name the participant's shape and alias in YAML,
        // which is the structured editor's business; the text after it is
        // still read so the statements that follow keep their place.
        let close = line[cursor..].find('}')?;
        cursor += close + 1;
        return Some(read_rest(line, cursor));
    }
    skip_blanks(line, &mut cursor);
    if starts_with_word_ignoring_case(&line[cursor..], "as") {
        let (span, end) = text_span(offset, line, cursor + "as".len());
        scan.aliases.push((id, span));
        return Some(end);
    }
    Some(read_rest(line, cursor))
}

/// `note left of A: …`, `note right of A: …`, `note over A: …` and
/// `note over A,B: …`.
fn read_note(
    offset: usize,
    line: &str,
    after_keyword: usize,
    scan: &mut SequenceScan,
) -> (usize, Terminator) {
    let (end, terminator) = read_rest(line, after_keyword);
    let head = &line[after_keyword..end];
    let Some(colon) = head.find(':') else {
        scan.items.push(None);
        return (end, terminator);
    };
    let placement = head[..colon].trim_start();
    let actors = ["left of", "right of", "over"]
        .iter()
        .find_map(|placement_word| {
            let head = placement.get(..placement_word.len())?;
            head.eq_ignore_ascii_case(placement_word)
                .then(|| &placement[placement_word.len()..])
        });
    match actors {
        Some(actors) => {
            for actor in actors.split(',') {
                let actor = actor.trim();
                if !actor.is_empty() {
                    scan.mentioned.push(actor.to_string());
                }
            }
        }
        None => scan.unreadable = true,
    }
    let (span, _) = text_span(offset, line, after_keyword + colon + 1);
    scan.items.push(Some(span));
    (end, terminator)
}

/// `A ->> B: …`, with the activation and central-connection markers merman
/// accepts around the arrow.
fn read_message(
    offset: usize,
    line: &str,
    position: usize,
    scan: &mut SequenceScan,
) -> (usize, Terminator) {
    let unreadable = |scan: &mut SequenceScan| {
        scan.unreadable = true;
        read_rest(line, position)
    };
    let Some((from, mut cursor)) = read_actor(line, position) else {
        return unreadable(scan);
    };
    skip_blanks(line, &mut cursor);
    if line[cursor..].starts_with("()") {
        cursor += 2;
        skip_blanks(line, &mut cursor);
    }
    let Some(arrow) = ARROWS
        .iter()
        .find(|arrow| line[cursor..].starts_with(**arrow))
    else {
        return unreadable(scan);
    };
    cursor += arrow.len();
    skip_blanks(line, &mut cursor);
    if line[cursor..].starts_with('+') || line[cursor..].starts_with('-') {
        cursor += 1;
        skip_blanks(line, &mut cursor);
    }
    if line[cursor..].starts_with("()") {
        cursor += 2;
        skip_blanks(line, &mut cursor);
    }
    let Some((to, mut cursor)) = read_actor(line, cursor) else {
        return unreadable(scan);
    };
    scan.mentioned.push(from);
    scan.mentioned.push(to);
    skip_blanks(line, &mut cursor);
    if !line[cursor..].starts_with(':') {
        // merman does not draw a message without text, so there is nothing to
        // number here, but nothing to trust about the count either.
        return unreadable(scan);
    }
    let (span, end) = text_span(offset, line, cursor + 1);
    scan.items.push(Some(span));
    end
}

/// The text of the `index`-th message or note, counting both in the order they
/// were written.
pub(super) fn find_item_label(fence_body: &str, index: usize) -> MermaidLabelLookup {
    let scan = scan_sequence(fence_body);
    if scan.unreadable {
        return Err(MermaidLabelRefusal::NotLocated);
    }
    let span = scan
        .items
        .get(index)
        .cloned()
        .flatten()
        .ok_or(MermaidLabelRefusal::NotLocated)?;
    checked(fence_body, span)
}

/// The display name a participant is drawn with.
///
/// A participant declared without `as` is drawn with its id, which every
/// message names it by as well, so the id is never rewritten here.
pub(super) fn find_participant_label(fence_body: &str, participant: &str) -> MermaidLabelLookup {
    let scan = scan_sequence(fence_body);
    let mut found = scan
        .aliases
        .iter()
        .filter(|(id, _)| id == participant)
        .map(|(_, span)| span);
    let Some(span) = found.next() else {
        return Err(if scan.mentioned.iter().any(|id| id == participant) {
            MermaidLabelRefusal::NoLabelToken
        } else {
            MermaidLabelRefusal::NotLocated
        });
    };
    // A participant declared twice with two names is drawn with one of them,
    // and which one is merman's business; rewriting the other would change
    // nothing on screen.
    if found.next().is_some() {
        return Err(MermaidLabelRefusal::NotLocated);
    }
    checked(fence_body, span.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mermaid_source::encode_label;

    fn apply(fence_body: &str, span: &MermaidLabelSpan, new_label: &str) -> String {
        let replaced = &fence_body[span.range.clone()];
        let mut edited = fence_body.to_string();
        edited.replace_range(
            span.range.clone(),
            &encode_label(new_label, span.quoting, replaced),
        );
        edited
    }

    fn item(fence_body: &str, index: usize) -> &str {
        let span = find_item_label(fence_body, index).expect("the item should be found");
        &fence_body[span.range]
    }

    #[test]
    fn messages_and_notes_are_numbered_together_in_source_order() {
        let fence_body = concat!(
            "sequenceDiagram\n",
            "    participant A as \u{9867}\u{5ba2}\n",
            "    A->>B: \u{6ce8}\u{6587}\n",
            "    Note right of B: \u{78ba}\u{8a8d}\u{4e2d}\n",
            "    loop \u{6bce}\u{65e5}\n",
            "        B-->>A: \u{5fdc}\u{7b54}\n",
            "    end\n",
            "    activate A\n",
            "    Note over A,B: \u{7d42}\u{4e86}\n",
        );
        assert_eq!(item(fence_body, 0), "\u{6ce8}\u{6587}");
        assert_eq!(item(fence_body, 1), "\u{78ba}\u{8a8d}\u{4e2d}");
        assert_eq!(item(fence_body, 2), "\u{5fdc}\u{7b54}");
        assert_eq!(item(fence_body, 3), "\u{7d42}\u{4e86}");
        assert_eq!(
            find_item_label(fence_body, 4),
            Err(MermaidLabelRefusal::NotLocated)
        );
    }

    /// A `;` ends a statement just like a line break, so two messages on one
    /// line are two numbers, and the first one's text stops at the `;`.
    #[test]
    fn a_semicolon_separates_statements() {
        let fence_body = "sequenceDiagram\n    A->>B: one; B->>A: two\n";
        assert_eq!(item(fence_body, 0), "one");
        assert_eq!(item(fence_body, 1), "two");

        let edited = apply(
            fence_body,
            &find_item_label(fence_body, 0).expect("found"),
            "a;b",
        );
        assert_eq!(edited, "sequenceDiagram\n    A->>B: a#59;b; B->>A: two\n");
        assert_eq!(item(&edited, 1), "two", "the neighbour keeps its number");
        assert_eq!(
            find_item_label(&edited, 0).map(|span| span.label),
            Ok("a;b".to_string())
        );
    }

    /// A `%%` or a bare `#` ends the text and comments out the rest of the
    /// line, so neither is part of the token. An entity is not a comment.
    #[test]
    fn a_comment_ends_the_text_and_an_entity_does_not() {
        let fence_body = "sequenceDiagram\n    A->>B: \u{672c}\u{6587} %% \u{30e1}\u{30e2}; B->>A: gone\n    A->>B: x #59; y # tail\n";
        assert_eq!(item(fence_body, 0), "\u{672c}\u{6587}");
        assert_eq!(item(fence_body, 1), "x #59; y");
        assert_eq!(
            find_item_label(fence_body, 1).map(|span| span.label),
            Ok("x ; y".to_string())
        );
        assert_eq!(
            find_item_label(fence_body, 2),
            Err(MermaidLabelRefusal::NotLocated),
            "the statement after the comment is not a message"
        );

        let edited = apply(
            fence_body,
            &find_item_label(fence_body, 0).expect("found"),
            "50%% #1",
        );
        assert!(
            edited.contains("A->>B: 50#37;% #35;1 %% "),
            "a written `%%` or `#` must not open a comment: {edited}"
        );
    }

    #[test]
    fn a_participant_alias_is_the_label_and_a_bare_id_is_refused() {
        let fence_body = concat!(
            "sequenceDiagram\n",
            "    participant A as \u{9867}\u{5ba2}\u{3055}\u{3093}\n",
            "    actor B\n",
            "    A->>B: hi\n",
        );
        let span = find_participant_label(fence_body, "A").expect("the alias should be found");
        assert_eq!(
            &fence_body[span.range.clone()],
            "\u{9867}\u{5ba2}\u{3055}\u{3093}"
        );
        assert_eq!(
            apply(fence_body, &span, "\u{304a}\u{5ba2}\u{69d8}"),
            fence_body.replace(
                "\u{9867}\u{5ba2}\u{3055}\u{3093}",
                "\u{304a}\u{5ba2}\u{69d8}"
            )
        );
        assert_eq!(
            find_participant_label(fence_body, "B"),
            Err(MermaidLabelRefusal::NoLabelToken)
        );
        assert_eq!(
            find_participant_label(fence_body, "C"),
            Err(MermaidLabelRefusal::NotLocated)
        );

        let twice = "sequenceDiagram\n    participant A as one\n    participant A as two\n";
        assert_eq!(
            find_participant_label(twice, "A"),
            Err(MermaidLabelRefusal::NotLocated)
        );
    }

    /// merman reads a leading `wrap:` as a switch, not as text, so the token
    /// starts after it.
    #[test]
    fn a_wrap_directive_is_not_part_of_the_text() {
        let fence_body = "sequenceDiagram\n    A->>B: wrap: long text\n";
        assert_eq!(item(fence_body, 0), "long text");
    }

    #[test]
    fn the_arrow_variants_are_all_messages() {
        let fence_body = concat!(
            "sequenceDiagram\n",
            "    A->>+B: a\n",
            "    B-->>-A: b\n",
            "    A-xB: c\n",
            "    A--)B: d\n",
            "    A<<->>B: e\n",
            "    A->B: f\n",
            "    A-->B: g\n",
            "    A--xB: h\n",
            "    A-)B: i\n",
            "    A<<-->>B: j\n",
            "    A->>A: k\n",
        );
        let texts: Vec<&str> = (0..11).map(|index| item(fence_body, index)).collect();
        assert_eq!(
            texts,
            ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k"]
        );
    }

    /// Something this scan does not know may write a message of its own, so
    /// numbering past it would be a guess.
    #[test]
    fn an_unknown_statement_refuses_every_numbered_lookup() {
        let fence_body = "sequenceDiagram\n    A->>B: one\n    A ~~> B: two\n";
        assert_eq!(
            find_item_label(fence_body, 0),
            Err(MermaidLabelRefusal::NotLocated)
        );
    }

    #[test]
    fn an_empty_text_is_refused_as_empty() {
        let fence_body = "sequenceDiagram\n    A->>B:\n    A->>B: x\n";
        assert_eq!(
            find_item_label(fence_body, 0),
            Err(MermaidLabelRefusal::EmptyLabel)
        );
        assert_eq!(item(fence_body, 1), "x");
    }

    #[test]
    fn a_multi_line_description_block_is_skipped() {
        let fence_body = concat!(
            "sequenceDiagram\n",
            "    accDescr {\n",
            "        A->>B: not a message\n",
            "    }\n",
            "    A->>B: real\n",
        );
        assert_eq!(item(fence_body, 0), "real");
    }

    /// merman reads `Box->>A` as a message from `Box`, so skipping it as a
    /// `box` block would give the next message its number, and a double-click
    /// on the first message would rewrite the second.
    #[test]
    fn a_participant_named_like_a_keyword_still_sends_messages() {
        let fence_body = concat!(
            "sequenceDiagram\n",
            "    participant Box\n",
            "    Box->>A: one\n",
            "    loop ->> Box: two\n",
            "    A->>Box: three\n",
            "    loop \u{6bce}\u{65e5}\n",
            "    end\n",
            "    title->>A: four\n",
        );
        assert_eq!(item(fence_body, 0), "one");
        assert_eq!(item(fence_body, 1), "two");
        assert_eq!(item(fence_body, 2), "three");
        assert_eq!(item(fence_body, 3), "four");
    }

    /// merman goes on after the `}` of a description block, so a statement
    /// there is one this scan would miss.
    #[test]
    fn a_statement_after_a_description_block_refuses_the_count() {
        let same_line = "sequenceDiagram\n    accDescr { d }; A->>B: one\n    A->>B: two\n";
        assert_eq!(
            find_item_label(same_line, 0),
            Err(MermaidLabelRefusal::NotLocated)
        );
        let closing_line =
            "sequenceDiagram\n    accDescr {\n    d\n    } A->>B: one\n    A->>B: two\n";
        assert_eq!(
            find_item_label(closing_line, 0),
            Err(MermaidLabelRefusal::NotLocated)
        );
        let clean = "sequenceDiagram\n    accDescr {\n    d\n    } %% x\n    A->>B: two\n";
        assert_eq!(item(clean, 0), "two");
    }

    #[test]
    fn a_newline_in_the_new_text_does_not_split_the_statement() {
        let fence_body = "sequenceDiagram\n    A->>B: old\n";
        let edited = apply(
            fence_body,
            &find_item_label(fence_body, 0).expect("found"),
            "one\ntwo",
        );
        assert_eq!(edited, "sequenceDiagram\n    A->>B: one two\n");
    }
}
