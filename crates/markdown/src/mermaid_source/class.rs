//! Class diagrams: a class's display name, inside the `["…"]` of a `class`
//! statement, and a relation's label, after the `:` of a relation statement.
//!
//! A class written without a display name is drawn with its name, which every
//! relation, member statement and `style` line refers to it by, so it is never
//! offered for rewriting here.
//!
//! merman's class lexer only reads `%%` as a comment where a token would start,
//! so a relation's label — which runs from its `:` to the end of the line —
//! keeps a `%%` written inside it, while one written before the `:` comments the
//! label out.

use super::{MermaidLabelLookup, MermaidLabelQuoting, MermaidLabelRefusal, MermaidLabelSpan};
use super::{checked, decode_label, starts_with_word};

/// One statement of a class diagram, as far as this module reads it.
enum ClassStatement {
    Declaration {
        name: String,
        label: Option<MermaidLabelSpan>,
    },
    Relation {
        left: String,
        right: String,
        label: Option<MermaidLabelSpan>,
    },
    Other,
}

/// Statements that open with a word and name nothing this module edits.
const SKIPPED_KEYWORDS: &[&str] = &[
    "classDiagram",
    "direction",
    "namespace",
    "note",
    "cssClass",
    "style",
    "classDef",
    "click",
    "link",
    "callback",
    "accTitle",
];

/// The statements of a class diagram, with the byte offset each one starts at.
///
/// A `class A {` opens a member block that only a line starting with `}`
/// closes — merman reads every other line inside it, `}` and all, as a member —
/// and a multi-line `accDescr { … }` runs to the next `}`.
fn class_statements(fence_body: &str) -> Vec<(usize, &str)> {
    let mut statements = Vec::new();
    let mut line_start = 0;
    let mut in_members = false;
    let mut in_description = false;
    for line in fence_body.split_inclusive('\n') {
        let offset = line_start;
        line_start += line.len();
        let content = line.trim_end_matches(['\n', '\r']);
        let trimmed = content.trim();
        if in_description {
            if trimmed.contains('}') {
                in_description = false;
            }
            continue;
        }
        if in_members {
            if trimmed.starts_with('}') {
                in_members = false;
            }
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with("%%") || trimmed.starts_with('}') {
            continue;
        }
        if starts_with_word(trimmed, "accDescr") {
            let after = trimmed["accDescr".len()..].trim_start();
            if after.starts_with('{') && !after.contains('}') {
                in_description = true;
            }
            continue;
        }
        if starts_with_word(trimmed, "class") && class_opens_members(trimmed) {
            in_members = true;
        }
        let leading = content.len() - content.trim_start().len();
        statements.push((offset + leading, content.trim_start()));
    }
    statements
}

/// Whether a `class` statement opens a member block, which is a `{` outside
/// the quotes of its display name.
fn class_opens_members(statement: &str) -> bool {
    let mut quoted = false;
    for character in statement.chars() {
        match character {
            '"' => quoted = !quoted,
            '{' if !quoted => return true,
            _ => {}
        }
    }
    false
}

/// How far merman's class lexer reads a name, from the start of `text`.
fn name_length(text: &str) -> usize {
    if let Some(body) = text.strip_prefix('`') {
        return match body.find('`') {
            Some(end) => end + 2,
            None => 0,
        };
    }
    let mut length = 0;
    while let Some(rest) = text.get(length..) {
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
        length += character.len_utf8();
    }
    length
}

/// The id a name token refers to: without the backquotes that let it hold
/// anything, and without the `~T~` a generic class carries.
fn class_id(token: &str) -> String {
    if let Some(inner) = token
        .strip_prefix('`')
        .and_then(|rest| rest.strip_suffix('`'))
    {
        return inner.to_string();
    }
    token.split('~').next().unwrap_or_default().to_string()
}

fn skip_whitespace(text: &str, position: &mut usize) {
    while let Some(character) = text.get(*position..).and_then(|rest| rest.chars().next()) {
        if character.is_whitespace() {
            *position += character.len_utf8();
        } else {
            break;
        }
    }
}

/// Reads the name at `position`, returning its id.
fn read_name(statement: &str, position: &mut usize) -> Option<String> {
    skip_whitespace(statement, position);
    let length = name_length(statement.get(*position..)?);
    if length == 0 {
        return None;
    }
    let token = statement.get(*position..*position + length)?;
    *position += length;
    Some(class_id(token))
}

/// A `"…"` cardinality or display name at `position`, returning the range it
/// covers, quotes included.
fn read_quoted(statement: &str, position: &mut usize) -> Option<std::ops::Range<usize>> {
    skip_whitespace(statement, position);
    let body = statement.get(*position..)?.strip_prefix('"')?;
    let end = body.find('"')?;
    let range = *position..(*position + end + 2);
    *position = range.end;
    Some(range)
}

/// Splits a relation operator off the start of `text`: an optional marker, a
/// `--` or `..` line, and an optional marker, with nothing between them.
fn operator_length(text: &str) -> Option<usize> {
    fn marker(text: &str) -> usize {
        ["<|", "|>", "()", "*", "o", "<", ">"]
            .iter()
            .find(|marker| text.starts_with(**marker))
            .map_or(0, |marker| marker.len())
    }
    let head = marker(text);
    let line = text.get(head..)?;
    if !(line.starts_with("--") || line.starts_with("..")) {
        return None;
    }
    let tail = marker(text.get(head + 2..)?);
    Some(head + 2 + tail)
}

fn parse_class_statement(offset: usize, statement: &str) -> ClassStatement {
    if SKIPPED_KEYWORDS
        .iter()
        .any(|keyword| starts_with_word(statement, keyword))
        || statement.starts_with("<<")
    {
        return ClassStatement::Other;
    }
    if starts_with_word(statement, "class") {
        let mut position = "class".len();
        let Some(name) = read_name(statement, &mut position) else {
            return ClassStatement::Other;
        };
        skip_whitespace(statement, &mut position);
        let label = statement
            .get(position..)
            .and_then(|rest| rest.strip_prefix('['))
            .and_then(|_| {
                let mut cursor = position + 1;
                let range = read_quoted(statement, &mut cursor)?;
                skip_whitespace(statement, &mut cursor);
                statement
                    .get(cursor..)?
                    .starts_with(']')
                    .then(|| MermaidLabelSpan {
                        range: (offset + range.start)..(offset + range.end),
                        label: decode_label(&statement[range]),
                        quoting: MermaidLabelQuoting::ClassLabel,
                    })
            });
        return ClassStatement::Declaration { name, label };
    }

    let mut position = 0;
    let Some(left) = read_name(statement, &mut position) else {
        return ClassStatement::Other;
    };
    let mut probe = position;
    if read_quoted(statement, &mut probe).is_some() {
        position = probe;
    }
    skip_whitespace(statement, &mut position);
    let Some(operator) = statement.get(position..).and_then(operator_length) else {
        return ClassStatement::Other;
    };
    position += operator;
    let mut probe = position;
    if read_quoted(statement, &mut probe).is_some() {
        position = probe;
    }
    let Some(right) = read_name(statement, &mut position) else {
        return ClassStatement::Other;
    };
    skip_whitespace(statement, &mut position);
    let rest = statement.get(position..).unwrap_or_default();
    let label = match rest.strip_prefix(':') {
        Some(after) if !after.starts_with("::") => {
            let mut start = position + 1;
            skip_whitespace(statement, &mut start);
            let raw = statement.get(start..).unwrap_or_default().trim_end();
            Some(MermaidLabelSpan {
                range: (offset + start)..(offset + start + raw.len()),
                label: decode_label(raw),
                quoting: MermaidLabelQuoting::ClassRelationLabel,
            })
        }
        _ => None,
    };
    ClassStatement::Relation { left, right, label }
}

/// The display name a class is drawn with.
///
/// merman keeps the last one a class is given, so that is the token on screen.
pub(super) fn find_class_label(fence_body: &str, class: &str) -> MermaidLabelLookup {
    let mut labelled = None;
    let mut mentioned = false;
    for (offset, statement) in class_statements(fence_body) {
        match parse_class_statement(offset, statement) {
            ClassStatement::Declaration { name, label } => {
                if name != class {
                    continue;
                }
                mentioned = true;
                if label.is_some() {
                    labelled = label;
                }
            }
            ClassStatement::Relation { left, right, .. } => {
                mentioned |= left == class || right == class;
            }
            ClassStatement::Other => {}
        }
    }
    match labelled {
        Some(span) => checked(fence_body, span),
        None if mentioned => Err(MermaidLabelRefusal::NoLabelToken),
        None => Err(MermaidLabelRefusal::NotLocated),
    }
}

/// The label of the `occurrence`-th relation written from `from` to `to`.
pub(super) fn find_relation_label(
    fence_body: &str,
    from: &str,
    to: &str,
    occurrence: usize,
) -> MermaidLabelLookup {
    let mut seen = 0;
    for (offset, statement) in class_statements(fence_body) {
        let ClassStatement::Relation { left, right, label } =
            parse_class_statement(offset, statement)
        else {
            continue;
        };
        if left != from || right != to {
            continue;
        }
        if seen == occurrence {
            return checked(fence_body, label.ok_or(MermaidLabelRefusal::NotLocated)?);
        }
        seen += 1;
    }
    Err(MermaidLabelRefusal::NotLocated)
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

    const DIAGRAM: &str = concat!(
        "classDiagram\n",
        "    class \u{52d5}\u{7269}[\"\u{3069}\u{3046}\u{3076}\u{3064}\"] {\n",
        "        +String \u{540d}\u{524d}\n",
        "        class Fake[\"not a declaration\"]\n",
        "    }\n",
        "    class Duck\n",
        "    \u{52d5}\u{7269} <|-- Duck : \u{7d99}\u{627f}\n",
        "    \u{52d5}\u{7269} <|-- Duck : \u{4e8c}\u{3064}\u{76ee}\n",
        "    Duck --> Pond\n",
        "    %% Duck --> Pond : commented\n",
    );

    #[test]
    fn a_display_name_is_the_label_and_a_bare_name_is_refused() {
        let span =
            find_class_label(DIAGRAM, "\u{52d5}\u{7269}").expect("the label should be found");
        assert_eq!(
            &DIAGRAM[span.range.clone()],
            "\"\u{3069}\u{3046}\u{3076}\u{3064}\""
        );
        assert_eq!(span.label, "\u{3069}\u{3046}\u{3076}\u{3064}");
        assert_eq!(
            apply(DIAGRAM, &span, "\u{3051}\u{3082}\u{306e} \"A\""),
            DIAGRAM.replace(
                "[\"\u{3069}\u{3046}\u{3076}\u{3064}\"]",
                "[\"\u{3051}\u{3082}\u{306e} #quot;A#quot;\"]"
            )
        );

        assert_eq!(
            find_class_label(DIAGRAM, "Duck"),
            Err(MermaidLabelRefusal::NoLabelToken)
        );
        assert_eq!(
            find_class_label(DIAGRAM, "Pond"),
            Err(MermaidLabelRefusal::NoLabelToken)
        );
        assert_eq!(
            find_class_label(DIAGRAM, "Fake"),
            Err(MermaidLabelRefusal::NotLocated),
            "a line inside a member block declares nothing"
        );
    }

    #[test]
    fn relation_labels_are_found_by_their_ends_and_position() {
        let first = find_relation_label(DIAGRAM, "\u{52d5}\u{7269}", "Duck", 0).expect("first");
        assert_eq!(&DIAGRAM[first.range], "\u{7d99}\u{627f}");
        let second = find_relation_label(DIAGRAM, "\u{52d5}\u{7269}", "Duck", 1).expect("second");
        assert_eq!(&DIAGRAM[second.range.clone()], "\u{4e8c}\u{3064}\u{76ee}");
        assert_eq!(
            apply(DIAGRAM, &second, "\u{65b0}\nline"),
            DIAGRAM.replace(": \u{4e8c}\u{3064}\u{76ee}", ": \u{65b0} line")
        );
        assert_eq!(
            find_relation_label(DIAGRAM, "Duck", "Pond", 0),
            Err(MermaidLabelRefusal::NotLocated),
            "an unlabelled relation has no token, and the commented one is not a relation"
        );
        assert_eq!(
            find_relation_label(DIAGRAM, "\u{52d5}\u{7269}", "Duck", 2),
            Err(MermaidLabelRefusal::NotLocated)
        );
    }

    /// A relation's label runs to the end of its line, `%%` included, since
    /// merman only reads one as a comment where a token would start.
    #[test]
    fn a_percent_pair_inside_a_label_belongs_to_it() {
        let fence_body = "classDiagram\n    A --> B : 50%% off\n    A --> C %% note\n";
        let span = find_relation_label(fence_body, "A", "B", 0).expect("found");
        assert_eq!(&fence_body[span.range], "50%% off");
        assert_eq!(
            find_relation_label(fence_body, "A", "C", 0),
            Err(MermaidLabelRefusal::NotLocated)
        );
    }

    #[test]
    fn cardinalities_generics_and_backquoted_names_are_read() {
        let fence_body = concat!(
            "classDiagram\n",
            "    class List~T~[\"\u{30ea}\u{30b9}\u{30c8}\"]\n",
            "    Customer \"1\" --> \"*\" Ticket : \u{8cfc}\u{5165}\n",
            "    `A B` ..|> List~T~ : x\n",
        );
        assert_eq!(
            find_class_label(fence_body, "List").map(|span| span.label),
            Ok("\u{30ea}\u{30b9}\u{30c8}".to_string())
        );
        assert_eq!(
            find_relation_label(fence_body, "Customer", "Ticket", 0).map(|span| span.label),
            Ok("\u{8cfc}\u{5165}".to_string())
        );
        assert_eq!(
            find_relation_label(fence_body, "A B", "List", 0).map(|span| span.label),
            Ok("x".to_string())
        );
    }

    #[test]
    fn an_empty_label_is_refused_as_empty() {
        let fence_body = "classDiagram\n    class A[\"\"]\n    A --> B :\n";
        assert_eq!(
            find_class_label(fence_body, "A").map(|span| span.label),
            Ok(String::new()),
            "the quotes are the token, so an empty name can still be replaced"
        );
        assert_eq!(
            find_relation_label(fence_body, "A", "B", 0),
            Err(MermaidLabelRefusal::EmptyLabel)
        );
    }
}
