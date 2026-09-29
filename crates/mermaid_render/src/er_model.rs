//! A round-trippable editing model for Mermaid entity relationship diagrams.
//!
//! The shape mirrors [`crate::flowchart_model`]: [`parse_er`] borrows
//! `merman`'s parser and copies the parts of its ER model that map onto source
//! constructs, and [`emit_er`] writes them back.
//!
//! Two properties matter more than fidelity to the original spelling:
//!
//! - [`emit_er`] is idempotent from its second application. Parsing its own
//!   output and emitting again produces the identical string, so opening and
//!   confirming the editor twice does not keep rewriting the fence.
//! - Everything `merman`'s model does not carry — comments, `%%{init}%%`
//!   directives, `accTitle` / `accDescr`, `classDef` and `style` — survives
//!   verbatim in [`ErDocument::preserved_lines`]. A `%%` comment at the end
//!   of a statement is carried by the entity, attribute or relationship it was
//!   written after, and is written back after it.
//!
//! Deliberate re-spellings, none of which change what Mermaid draws:
//!
//! - Every entity gets a declaration of its own, ahead of the relationships,
//!   in the order the diagram introduced it. `merman` keys its entities by
//!   name and so reports them alphabetically; the order the boxes are laid out
//!   in is the order they were introduced, which is recovered from the
//!   internal ids and then written out explicitly.
//! - A `class E name` statement is written back as `E:::name` on the entity's
//!   own declaration, because the model records class membership on the entity
//!   and keeping the statement as well would apply it twice.

use std::collections::HashMap;
use std::fmt::Write as _;

use anyhow::{Result, anyhow, bail};
use merman::diagram::RenderSemanticModel;
use merman::diagrams::er::ErDiagramRenderModel;

use crate::flowchart_model::{PreservedLine, PreservedPosition};

const HEADER: &str = "erDiagram";
const INDENT: &str = "    ";

/// The layout direction of the diagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErDirection {
    TopToBottom,
    BottomToTop,
    LeftToRight,
    RightToLeft,
}

impl ErDirection {
    pub const ALL: [ErDirection; 4] = [
        ErDirection::TopToBottom,
        ErDirection::LeftToRight,
        ErDirection::BottomToTop,
        ErDirection::RightToLeft,
    ];

    pub fn keyword(self) -> &'static str {
        match self {
            Self::TopToBottom => "TB",
            Self::BottomToTop => "BT",
            Self::LeftToRight => "LR",
            Self::RightToLeft => "RL",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::TopToBottom => i18n::t!("Top to bottom (TB)"),
            Self::BottomToTop => i18n::t!("Bottom to top (BT)"),
            Self::LeftToRight => i18n::t!("Left to right (LR)"),
            Self::RightToLeft => i18n::t!("Right to left (RL)"),
        }
    }

    fn from_model(direction: &str) -> Self {
        match direction.trim() {
            "BT" => Self::BottomToTop,
            "LR" => Self::LeftToRight,
            "RL" => Self::RightToLeft,
            _ => Self::TopToBottom,
        }
    }
}

/// How many rows of one entity take part in a relationship.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErCardinality {
    /// `|o` … `o|`
    ZeroOrOne,
    /// `||`
    ExactlyOne,
    /// `}o` … `o{`
    ZeroOrMore,
    /// `}|` … `|{`
    OneOrMore,
    /// `u`, Mermaid's markdown parent. It only has a left hand spelling, so
    /// [`is_valid_cardinality_pair`] refuses it on the right and the editor's
    /// menu does not offer it there.
    MarkdownParent,
}

impl ErCardinality {
    /// The cardinalities offered on both ends of a relationship, in menu order.
    pub const ALL: [ErCardinality; 4] = [
        ErCardinality::ExactlyOne,
        ErCardinality::ZeroOrOne,
        ErCardinality::ZeroOrMore,
        ErCardinality::OneOrMore,
    ];

    /// How the cardinality is written on the left of the relationship.
    pub fn left_symbol(self) -> &'static str {
        match self {
            Self::ZeroOrOne => "|o",
            Self::ExactlyOne => "||",
            Self::ZeroOrMore => "}o",
            Self::OneOrMore => "}|",
            Self::MarkdownParent => "u",
        }
    }

    /// How the cardinality is written on the right of the relationship.
    pub fn right_symbol(self) -> &'static str {
        match self {
            Self::ZeroOrOne => "o|",
            Self::ExactlyOne => "||",
            Self::ZeroOrMore => "o{",
            Self::OneOrMore => "|{",
            // Mermaid has no right hand spelling for this one; see the variant.
            Self::MarkdownParent => "u",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::ZeroOrOne => i18n::t!("Zero or one"),
            Self::ExactlyOne => i18n::t!("Exactly one"),
            Self::ZeroOrMore => i18n::t!("Zero or more"),
            Self::OneOrMore => i18n::t!("One or more"),
            Self::MarkdownParent => i18n::t!("Parent (u)"),
        }
    }

    fn from_model(cardinality: &str) -> Option<Self> {
        match cardinality {
            "ZERO_OR_ONE" => Some(Self::ZeroOrOne),
            "ONLY_ONE" => Some(Self::ExactlyOne),
            "ZERO_OR_MORE" => Some(Self::ZeroOrMore),
            "ONE_OR_MORE" => Some(Self::OneOrMore),
            "MD_PARENT" => Some(Self::MarkdownParent),
            _ => None,
        }
    }
}

/// The key an attribute is marked with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErAttributeKey {
    Primary,
    Foreign,
    Unique,
}

impl ErAttributeKey {
    pub const ALL: [ErAttributeKey; 3] = [
        ErAttributeKey::Primary,
        ErAttributeKey::Foreign,
        ErAttributeKey::Unique,
    ];

    pub fn keyword(self) -> &'static str {
        match self {
            Self::Primary => "PK",
            Self::Foreign => "FK",
            Self::Unique => "UK",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Primary => i18n::t!("Primary key (PK)"),
            Self::Foreign => i18n::t!("Foreign key (FK)"),
            Self::Unique => i18n::t!("Unique key (UK)"),
        }
    }

    fn from_model(key: &str) -> Option<Self> {
        match key {
            "PK" => Some(Self::Primary),
            "FK" => Some(Self::Foreign),
            "UK" => Some(Self::Unique),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErAttribute {
    /// The data type, written first. `type` is a keyword, hence the name.
    pub type_name: String,
    pub name: String,
    pub keys: Vec<ErAttributeKey>,
    pub comment: Option<String>,
    /// A `%%` comment written after the attribute, kept with the row it
    /// explains.
    pub trailing_comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErEntity {
    pub name: String,
    /// The `[…]` label drawn instead of the name.
    pub alias: Option<String>,
    pub attributes: Vec<ErAttribute>,
    /// The classes assigned with `:::` or with a `class` statement.
    pub classes: Vec<String>,
    /// A `%%` comment written after the entity's declaration.
    pub trailing_comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErRelationship {
    pub left: String,
    pub right: String,
    pub left_cardinality: ErCardinality,
    pub right_cardinality: ErCardinality,
    /// `--` rather than `..`: the right entity cannot exist on its own.
    pub identifying: bool,
    pub label: String,
    /// A `%%` comment written after the relationship.
    pub trailing_comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErDocument {
    /// `None` when the source has no `direction` statement, which is Mermaid's
    /// `TB`.
    pub direction: Option<ErDirection>,
    /// Declaration order, which is also the order the boxes are laid out in.
    pub entities: Vec<ErEntity>,
    pub relationships: Vec<ErRelationship>,
    pub preserved_lines: Vec<PreservedLine>,
}

impl ErDocument {
    /// A blank diagram, used when the editor is opened on an empty fence.
    pub fn empty() -> Self {
        Self {
            direction: None,
            entities: Vec::new(),
            relationships: Vec::new(),
            preserved_lines: Vec::new(),
        }
    }

    pub fn entity_index(&self, name: &str) -> Option<usize> {
        self.entities.iter().position(|entity| entity.name == name)
    }

    /// A name no entity is using, based on `prefix`.
    pub fn unused_entity_name(&self, prefix: &str) -> String {
        if self.entity_index(prefix).is_none() {
            return prefix.to_string();
        }
        for suffix in 1..usize::MAX {
            let candidate = format!("{prefix}{suffix}");
            if self.entity_index(&candidate).is_none() {
                return candidate;
            }
        }
        prefix.to_string()
    }

    /// Why the document could not be written back as a diagram Mermaid accepts,
    /// if there is a reason. The editor calls this before confirming an edit;
    /// [`parse_er`] never produces a document that fails it.
    pub fn structure_error(&self) -> Option<String> {
        for (index, entity) in self.entities.iter().enumerate() {
            if !is_valid_entity_name(&entity.name) {
                return Some(i18n::t!("The entity name cannot be empty.").to_string());
            }
            if self.entities[..index]
                .iter()
                .any(|earlier| earlier.name == entity.name)
            {
                return Some(i18n::t!("Entity `{}` is defined twice.", entity.name));
            }
            for attribute in &entity.attributes {
                if attribute.type_name.is_empty() || attribute.name.is_empty() {
                    return Some(i18n::t!(
                        "The attributes of entity `{}` need both a type and a name.",
                        entity.name
                    ));
                }
                for word in [&attribute.type_name, &attribute.name] {
                    if !is_writable_attribute_word(word) {
                        return Some(i18n::t!(
                            "`{word}` cannot be used as an attribute type or name."
                        ));
                    }
                }
            }
            for class in &entity.classes {
                if !is_valid_class_name(class) {
                    return Some(i18n::t!("`{class}` cannot be used as a class name."));
                }
            }
            let comments = std::iter::once(&entity.trailing_comment).chain(
                entity
                    .attributes
                    .iter()
                    .map(|attribute| &attribute.trailing_comment),
            );
            for comment in comments.flatten() {
                if !is_valid_trailing_comment(comment) {
                    return Some(i18n::t!(
                        "`{comment}` cannot be written as an end-of-line comment."
                    ));
                }
            }
        }

        for relationship in &self.relationships {
            for name in [&relationship.left, &relationship.right] {
                if self.entity_index(name).is_none() {
                    return Some(i18n::t!("Entity `{name}` is not defined."));
                }
            }
            if !is_valid_cardinality_pair(
                relationship.left_cardinality,
                relationship.right_cardinality,
            ) {
                return Some(i18n::t!(
                    "`{}` cannot be used on the right side of a relationship.",
                    relationship.right_cardinality.label()
                ));
            }
            if let Some(comment) = &relationship.trailing_comment
                && !is_valid_trailing_comment(comment)
            {
                return Some(i18n::t!(
                    "`{comment}` cannot be written as an end-of-line comment."
                ));
            }
        }
        None
    }
}

/// Whether `source` is an ER diagram, i.e. whether the structured editor can
/// open it at all. Cheap enough to call before parsing.
pub fn is_er_source(source: &str) -> bool {
    first_statement_line(source).is_some_and(is_header)
}

/// Reads `source` — the body of a ```` ```mermaid ```` fence — into an
/// editable document.
pub fn parse_er(source: &str) -> Result<ErDocument> {
    if let Some(problem) = crate::mermaid_model::preprocessing_problem_for_parsed_labels(source) {
        bail!("{problem}");
    }
    let scan = scan_source(source)?;
    let comments = trailing_comments(&scan.statements)?;
    if scan.statements.is_empty() {
        // `erDiagram` on its own is not a diagram `merman` accepts, so the
        // empty fence never reaches the parser.
        return Ok(ErDocument {
            direction: None,
            entities: Vec::new(),
            relationships: Vec::new(),
            preserved_lines: scan.preserved,
        });
    }

    let parsed = crate::parse_for_render_model(source)
        .map_err(|error| anyhow!("{error}"))?
        .ok_or_else(|| anyhow!(i18n::t!("Mermaid could not read this as a diagram.")))?;
    let RenderSemanticModel::Er(model) = parsed.into_parts().1 else {
        bail!(i18n::t!("This diagram is not an ER diagram."));
    };

    let mut document = document_from_model(&model, scan.has_direction, scan.preserved)?;
    attach_trailing_comments(&mut document, comments)?;
    // Keeps [`ErDocument::structure_error`]'s promise: a document the editor
    // cannot write back never reaches it in the first place.
    if let Some(problem) = document.structure_error() {
        bail!(i18n::t!("{problem} Edit it as text."));
    }
    Ok(document)
}

fn document_from_model(
    model: &ErDiagramRenderModel,
    has_direction: bool,
    preserved: Vec<PreservedLine>,
) -> Result<ErDocument> {
    // `merman` keys entities by name, so the order the diagram introduced them
    // survives only in the counter its internal ids end with.
    let mut ordered = model
        .entities
        .iter()
        .map(|(name, entity)| {
            let counter = entity
                .id
                .strip_prefix(&format!("entity-{name}-"))
                .and_then(|suffix| suffix.parse::<usize>().ok())
                .unwrap_or(usize::MAX);
            (counter, name, entity)
        })
        .collect::<Vec<_>>();
    ordered.sort_by(|left, right| (left.0, left.1).cmp(&(right.0, right.1)));

    let mut name_of_id: HashMap<&str, &str> = HashMap::new();
    for (_, name, entity) in &ordered {
        name_of_id.insert(entity.id.as_str(), name.as_str());
    }

    let mut entities = Vec::with_capacity(ordered.len());
    for (_, name, entity) in &ordered {
        let mut attributes = Vec::with_capacity(entity.attributes.len());
        for attribute in &entity.attributes {
            let mut keys = Vec::with_capacity(attribute.keys.len());
            for key in &attribute.keys {
                keys.push(ErAttributeKey::from_model(key).ok_or_else(|| {
                    anyhow!(i18n::t!("The diagram contains an unsupported attribute key `{key}`. Edit it as text."))
                })?);
            }
            attributes.push(ErAttribute {
                type_name: decode_text(&attribute.ty),
                name: decode_text(&attribute.name),
                keys,
                comment: (!attribute.comment.is_empty()).then(|| decode_text(&attribute.comment)),
                trailing_comment: None,
            });
        }
        entities.push(ErEntity {
            name: decode_text(name.as_str()),
            alias: (!entity.alias.is_empty()).then(|| decode_text(&entity.alias)),
            attributes,
            classes: entity_classes(&entity.css_classes),
            trailing_comment: None,
        });
    }

    let mut relationships = Vec::with_capacity(model.relationships.len());
    for relationship in &model.relationships {
        let left = name_of_id
            .get(relationship.entity_a.as_str())
            .copied()
            .ok_or_else(|| {
                anyhow!(i18n::t!(
                    "Could not find the entity on the left side of a relationship."
                ))
            })?;
        let right = name_of_id
            .get(relationship.entity_b.as_str())
            .copied()
            .ok_or_else(|| {
                anyhow!(i18n::t!(
                    "Could not find the entity on the right side of a relationship."
                ))
            })?;
        // Mermaid stores the cardinality of the end it is drawn at, which is
        // the opposite of the end it is written at.
        let left_cardinality = ErCardinality::from_model(&relationship.rel_spec.card_b)
            .ok_or_else(|| anyhow!(i18n::t!("The diagram contains an unsupported cardinality.")))?;
        let right_cardinality = ErCardinality::from_model(&relationship.rel_spec.card_a)
            .ok_or_else(|| anyhow!(i18n::t!("The diagram contains an unsupported cardinality.")))?;
        let identifying = match relationship.rel_spec.rel_type.as_str() {
            "IDENTIFYING" => true,
            "NON_IDENTIFYING" => false,
            other => bail!(i18n::t!(
                "The diagram contains an unsupported relationship type `{other}`."
            )),
        };
        relationships.push(ErRelationship {
            left: decode_text(left),
            right: decode_text(right),
            left_cardinality,
            right_cardinality,
            identifying,
            label: decode_text(&relationship.role_a),
            trailing_comment: None,
        });
    }

    Ok(ErDocument {
        direction: has_direction.then(|| ErDirection::from_model(&model.direction)),
        entities,
        relationships,
        preserved_lines: preserved,
    })
}

/// The classes assigned to an entity. Every entity starts in `default`, which
/// is Mermaid's own and has no statement behind it.
fn entity_classes(css_classes: &str) -> Vec<String> {
    let mut classes = css_classes
        .split_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>();
    if classes.first().is_some_and(|class| class == "default") {
        classes.remove(0);
    }
    classes
}

/// Writes `document` back as Mermaid source, without a trailing newline.
pub fn emit_er(document: &ErDocument) -> String {
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

    if let Some(direction) = document.direction {
        let _ = writeln!(out, "{INDENT}direction {}", direction.keyword());
    }

    for entity in &document.entities {
        let head = entity_declaration(entity);
        if entity.attributes.is_empty() {
            let _ = writeln!(
                out,
                "{INDENT}{}",
                with_comment(head, &entity.trailing_comment)
            );
            continue;
        }
        let _ = writeln!(
            out,
            "{INDENT}{}",
            with_comment(format!("{head} {{"), &entity.trailing_comment)
        );
        for attribute in &entity.attributes {
            let _ = writeln!(
                out,
                "{}{}",
                INDENT.repeat(2),
                with_comment(
                    attribute_declaration(attribute),
                    &attribute.trailing_comment
                )
            );
        }
        let _ = writeln!(out, "{INDENT}}}");
    }

    for relationship in &document.relationships {
        let _ = writeln!(
            out,
            "{INDENT}{}",
            with_comment(
                relationship_statement(relationship),
                &relationship.trailing_comment
            )
        );
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

fn preserved_at(document: &ErDocument, position: PreservedPosition) -> impl Iterator<Item = &str> {
    document
        .preserved_lines
        .iter()
        .filter(move |line| line.position == position)
        .map(|line| line.text.as_str())
}

fn with_comment(statement: String, comment: &Option<String>) -> String {
    match comment {
        Some(comment) => format!("{statement} {comment}"),
        None => statement,
    }
}

fn entity_declaration(entity: &ErEntity) -> String {
    let mut out = name_token(&entity.name);
    if let Some(alias) = &entity.alias {
        let _ = write!(out, "[{}]", name_token(alias));
    }
    if !entity.classes.is_empty() {
        let _ = write!(out, ":::{}", entity.classes.join(","));
    }
    out
}

fn attribute_declaration(attribute: &ErAttribute) -> String {
    let mut out = format!(
        "{} {}",
        escape_attribute_word(&attribute.type_name),
        escape_attribute_word(&attribute.name)
    );
    if !attribute.keys.is_empty() {
        let keys = attribute
            .keys
            .iter()
            .map(|key| key.keyword())
            .collect::<Vec<_>>()
            .join(", ");
        let _ = write!(out, " {keys}");
    }
    if let Some(comment) = &attribute.comment {
        let _ = write!(out, " \"{}\"", escape_quoted(comment));
    }
    out
}

fn relationship_statement(relationship: &ErRelationship) -> String {
    format!(
        "{} {}{}{} {} : {}",
        name_token(&relationship.left),
        relationship.left_cardinality.left_symbol(),
        if relationship.identifying { "--" } else { ".." },
        relationship.right_cardinality.right_symbol(),
        name_token(&relationship.right),
        label_token(&relationship.label)
    )
}

/// An entity name or alias as it appears in a statement: bare when Mermaid's
/// name rules allow it, quoted otherwise.
fn name_token(name: &str) -> String {
    if is_bare_name(name) {
        name.to_string()
    } else {
        format!("\"{}\"", escape_quoted(name))
    }
}

/// A relationship label. Mermaid accepts a bare word or any quoted string
/// here, so only the bare spelling has to be checked.
fn label_token(label: &str) -> String {
    if is_bare_name(label) {
        label.to_string()
    } else {
        format!("\"{}\"", escape_quoted(label))
    }
}

/// Whether `name` can be written without quotes.
///
/// The lexer reads the relationship operators before it reads a name, so a
/// name that starts like one — `one`, `many`, `to`, a digit — would be taken
/// apart. So would a name that is one of the statement keywords.
fn is_bare_name(name: &str) -> bool {
    if name.is_empty() || name.starts_with(|character: char| character.is_ascii_digit()) {
        return false;
    }
    if !name.chars().all(|character| {
        !character.is_ascii()
            || character.is_ascii_alphanumeric()
            || matches!(character, '_' | '-' | '*')
    }) {
        return false;
    }
    let lower = name.to_ascii_lowercase();
    const KEYWORDS: [&str; 5] = ["erdiagram", "style", "classdef", "class", "direction"];
    if KEYWORDS.contains(&lower.as_str()) {
        return false;
    }
    // `u` in front of a `-` is the markdown parent cardinality.
    const PREFIXES: [&str; 7] = ["many", "one", "to", "0+", "1+", "--", "u-"];
    !PREFIXES.iter().any(|prefix| lower.starts_with(prefix))
}

/// Whether `name` can be used as an entity name at all.
///
/// Everything else a name may contain — quotes, `%`, `\`, newlines — is
/// written as the `#…;` entity shorthand that Mermaid decodes after lexing.
pub fn is_valid_entity_name(name: &str) -> bool {
    !name.is_empty()
}

/// Whether `word` can be used as an attribute's type or name.
///
/// Mermaid's attribute lexer takes a much narrower set of characters than the
/// rest of the grammar, and reads a bare `PK` / `FK` / `UK` as a key rather
/// than as a word.
pub fn is_valid_attribute_word(word: &str) -> bool {
    if word.is_empty()
        || ErAttributeKey::ALL
            .iter()
            .any(|key| word.eq_ignore_ascii_case(key.keyword()))
    {
        return false;
    }
    if word.chars().any(|character| {
        character.is_whitespace() || matches!(character, ',' | '"' | '{' | '}' | '#' | '%' | ';')
    }) {
        return false;
    }
    // Two or more tildes make the word one of Mermaid's generic types, which
    // it takes verbatim.
    if word.chars().filter(|character| *character == '~').count() >= 2 {
        return true;
    }
    let mut characters = word.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    let first_ok = first == '*' || first == '_' || first.is_alphabetic() || !first.is_ascii();
    first_ok
        && characters.all(|character| {
            !character.is_ascii()
                || character.is_alphanumeric()
                || matches!(character, '*' | '-' | '_' | '[' | ']' | '(' | ')')
        })
}

/// Whether `name` can be used as a class name in `:::`.
pub fn is_valid_class_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|character| {
            !character.is_ascii()
                || character.is_ascii_alphanumeric()
                || matches!(character, '_' | '-' | '*')
        })
}

/// Whether the two ends of a relationship can be written down.
/// [`ErCardinality::MarkdownParent`] has no right hand spelling in Mermaid.
pub fn is_valid_cardinality_pair(_left: ErCardinality, right: ErCardinality) -> bool {
    right != ErCardinality::MarkdownParent
}

/// Mermaid turns `#…;` into a placeholder before reading the text, and the
/// renderer turns the placeholder into the character it names. Reading a model
/// therefore means undoing both steps.
fn decode_text(text: &str) -> String {
    merman::entities::decode_mermaid_entities_to_unicode(text).into_owned()
}

/// Writes `text` inside a pair of quotes so that Mermaid reads it back
/// unchanged.
///
/// A quote would end the string, and `%`, `\` and the control characters below
/// make Mermaid read the string as a label rather than as an entity name, so
/// each becomes the `#…;` entity shorthand. A newline cannot survive inside a
/// statement at all, so it becomes the line break Mermaid draws.
fn escape_quoted(text: &str) -> String {
    let needs_entities = text.contains(['"', '%', '\\', '#', '\n', '\r', '\u{8}', '\u{b}']);
    let mut out = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '"' => out.push_str("#quot;"),
            '%' => out.push_str("#37;"),
            '\\' => out.push_str("#92;"),
            '#' => out.push_str("#35;"),
            '\u{8}' => out.push_str("#8;"),
            '\u{b}' => out.push_str("#11;"),
            '\r' => {
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
                out.push_str("<br/>");
            }
            '\n' => out.push_str("<br/>"),
            // A colon directly after a `#` inside a line that also says
            // `style` or `classDef` makes Mermaid delete the `;` that ends the
            // entity, which would then swallow the rest of the line.
            ':' if needs_entities => out.push_str("#58;"),
            _ => out.push(character),
        }
    }
    out
}

/// Attribute words are written bare, so every character Mermaid's attribute
/// lexer would refuse at that position becomes the `#…;` entity shorthand,
/// which the preprocessor turns into a placeholder of characters it accepts
/// everywhere. A word decoded out of a diagram written by hand can hold any
/// character at all, which is why the escape cannot be a fixed short list.
fn escape_attribute_word(word: &str) -> String {
    // Two or more tildes make the word one of Mermaid's generic types, which it
    // takes verbatim apart from the characters that end a word.
    let generic = word.chars().filter(|character| *character == '~').count() >= 2;
    let mut out = String::with_capacity(word.len());
    for (offset, character) in word.char_indices() {
        if attribute_character_is_bare(character, offset == 0, generic) {
            out.push(character);
        } else {
            let _ = write!(out, "#{};", character as u32);
        }
    }
    out
}

fn attribute_character_is_bare(character: char, first: bool, generic: bool) -> bool {
    if character.is_whitespace()
        || matches!(character, ',' | '"' | '{' | '}' | '#' | '%' | ';')
        || character.is_control()
    {
        return false;
    }
    if generic {
        return true;
    }
    if first {
        return character == '*'
            || character == '_'
            || character.is_alphabetic()
            || !character.is_ascii();
    }
    !character.is_ascii()
        || character.is_ascii_alphanumeric()
        || matches!(character, '*' | '-' | '_' | '[' | ']' | '(' | ')')
}

/// Whether [`escape_attribute_word`] can write `word` so that Mermaid reads it
/// back. Wider than [`is_valid_attribute_word`], which is the editor's own
/// rule for what a user may type: a word a diagram written by hand carried
/// through the entity shorthand still has to survive being written out again.
fn is_writable_attribute_word(word: &str) -> bool {
    !word.is_empty()
        && !ErAttributeKey::ALL
            .iter()
            .any(|key| word.eq_ignore_ascii_case(key.keyword()))
}

/// What the line scanner recovers from the source text that the parsed model
/// cannot express.
struct SourceScan {
    preserved: Vec<PreservedLine>,
    /// Whether the source has a `direction` statement. The model reports `TB`
    /// either way, so without this the editor could not tell a written `TB`
    /// from no statement at all.
    has_direction: bool,
    /// The lines the scanner handed to the parser, i.e. everything that is
    /// neither the header nor preserved. Only the model speaks for these, so
    /// they are where a construct the model drops has to be caught.
    statements: Vec<String>,
}

fn first_statement_line(source: &str) -> Option<&str> {
    let mut lines = source.lines().map(str::trim);
    let mut in_frontmatter = false;
    let mut seen_content = false;
    let mut in_directive = false;

    while let Some(line) = lines.next() {
        if line.is_empty() {
            continue;
        }
        if in_directive {
            if line.contains("}%%") {
                in_directive = false;
            }
            continue;
        }
        if in_frontmatter {
            if line == "---" {
                in_frontmatter = false;
            }
            continue;
        }
        if !seen_content && line == "---" {
            seen_content = true;
            in_frontmatter = true;
            continue;
        }
        seen_content = true;
        if line.starts_with("%%") {
            if line.starts_with("%%{") && !line.contains("}%%") {
                in_directive = true;
            }
            continue;
        }
        return Some(line);
    }
    None
}

fn is_header(line: &str) -> bool {
    let Some(head) = line.get(..HEADER.len()) else {
        return false;
    };
    if !head.eq_ignore_ascii_case(HEADER) {
        return false;
    }
    line.get(HEADER.len()..).is_some_and(|rest| {
        rest.is_empty()
            || rest.starts_with(|character: char| character.is_whitespace() || character == ';')
    })
}

/// Statements that `merman`'s ER model does not carry, and that are therefore
/// written back verbatim.
///
/// `class` is deliberately absent: the model does carry class membership, so
/// keeping the statement as well as re-emitting it from the model would apply
/// it twice.
fn preserved_keyword(line: &str) -> bool {
    const KEYWORDS: [&str; 4] = ["accTitle", "accDescr", "classDef", "style"];
    KEYWORDS.iter().any(|keyword| {
        line.get(..keyword.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(keyword))
            && line.get(keyword.len()..).is_some_and(|rest| {
                rest.is_empty()
                    || rest.starts_with(|character: char| {
                        character.is_whitespace() || character == ':' || character == '{'
                    })
            })
    })
}

fn is_direction_statement(line: &str) -> bool {
    line.get(.."direction".len())
        .is_some_and(|head| head.eq_ignore_ascii_case("direction"))
        && line
            .get("direction".len().."direction".len() + 1)
            .is_some_and(|rest| rest.starts_with(char::is_whitespace))
}

fn scan_source(source: &str) -> Result<SourceScan> {
    let lines = source.lines().collect::<Vec<_>>();
    let mut preserved = Vec::new();
    let mut statements = Vec::new();
    let mut has_direction = false;
    let mut index = 0;
    let mut seen_content = false;
    let mut found_header = false;

    while let Some(line) = lines.get(index).copied() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            index += 1;
            continue;
        }
        if !seen_content && trimmed == "---" {
            seen_content = true;
            let start = index;
            index += 1;
            while lines.get(index).is_some_and(|line| line.trim() != "---") {
                index += 1;
            }
            if index < lines.len() {
                index += 1;
            }
            preserved.push(PreservedLine {
                position: PreservedPosition::Preamble,
                text: join_lines(lines.get(start..index).unwrap_or_default()),
            });
            continue;
        }
        seen_content = true;
        if trimmed.starts_with("%%") {
            let start = index;
            index += 1;
            if trimmed.starts_with("%%{") && !trimmed.contains("}%%") {
                while lines.get(index).is_some_and(|line| !line.contains("}%%")) {
                    index += 1;
                }
                if index < lines.len() {
                    index += 1;
                }
            }
            preserved.push(PreservedLine {
                position: PreservedPosition::Preamble,
                text: join_lines(lines.get(start..index).unwrap_or_default()),
            });
            continue;
        }
        if !is_header(trimmed) {
            bail!(i18n::t!("This diagram is not an ER diagram."));
        }
        found_header = true;
        let rest = trimmed
            .get(HEADER.len()..)
            .unwrap_or_default()
            .trim_start_matches(';')
            .trim();
        if !rest.is_empty() {
            statements.push(rest.to_string());
        }
        index += 1;
        break;
    }

    if !found_header {
        // An empty fence: the editor opens on a blank diagram, and whatever
        // comments the user left keep their place.
        return Ok(SourceScan {
            preserved,
            has_direction: false,
            statements: Vec::new(),
        });
    }

    let mut position = PreservedPosition::Top;
    // An attribute block spans lines, and its contents must not be mistaken
    // for statements of their own.
    let mut block_depth = 0usize;
    while let Some(line) = lines.get(index).copied() {
        let trimmed = line.trim();
        index += 1;
        if trimmed.is_empty() {
            continue;
        }
        if block_depth == 0 {
            if trimmed.starts_with("%%") {
                let start = index - 1;
                if trimmed.starts_with("%%{") && !trimmed.contains("}%%") {
                    while lines.get(index).is_some_and(|line| !line.contains("}%%")) {
                        index += 1;
                    }
                    if index < lines.len() {
                        index += 1;
                    }
                }
                preserved.push(PreservedLine {
                    position,
                    text: join_lines(lines.get(start..index).unwrap_or_default()),
                });
                continue;
            }
            if preserved_keyword(trimmed) {
                let start = index - 1;
                // An `accDescr {` block runs to the first `}`, which may follow
                // text on its opening line or on the lines after it.
                let opens_block = trimmed
                    .find('{')
                    .is_some_and(|brace| !trimmed.get(brace..).unwrap_or_default().contains('}'));
                if opens_block {
                    while lines.get(index).is_some_and(|line| !line.contains('}')) {
                        index += 1;
                    }
                    if index < lines.len() {
                        index += 1;
                    }
                }
                preserved.push(PreservedLine {
                    position,
                    text: join_lines(lines.get(start..index).unwrap_or_default()),
                });
                continue;
            }
            if is_direction_statement(trimmed) {
                has_direction = true;
            }
        }

        // A brace written in a comment opens nothing.
        block_depth = block_depth.saturating_add_signed(brace_delta(split_comment(trimmed).0));
        statements.push(trimmed.to_string());
        position = PreservedPosition::Bottom;
    }

    Ok(SourceScan {
        preserved,
        has_direction,
        statements,
    })
}

/// How much `line` opens or closes attribute blocks.
///
/// Only a brace at the end of a statement groups attributes. A relationship
/// spells its cardinality with `o{`, `|{`, `}o` and `}|`, so counting every
/// brace would leave the scanner believing it was inside a block for the rest
/// of the diagram — and inside one a `style`, a `classDef` or a comment is
/// taken for a statement, which means it is neither kept verbatim nor carried
/// by the model, and confirming the editor would drop it from the fence.
fn brace_delta(line: &str) -> isize {
    let trimmed = line.trim();
    if trimmed.ends_with('{') {
        return 1;
    }
    if trimmed.starts_with('}') {
        return -1;
    }
    0
}

fn join_lines(lines: &[&str]) -> String {
    lines
        .iter()
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Where a `%%` comment starts in `statement`. The same characters inside a
/// quoted string are the user's words, which Mermaid's lexer reads as part of
/// the string.
fn comment_offset(statement: &str) -> Option<usize> {
    let mut quoted = false;
    for (offset, character) in statement.char_indices() {
        if character == '"' {
            quoted = !quoted;
            continue;
        }
        if !quoted && statement.get(offset..offset + 2) == Some("%%") {
            return Some(offset);
        }
    }
    None
}

/// `statement` without its trailing comment, and the comment from its `%%` on.
fn split_comment(statement: &str) -> (&str, Option<&str>) {
    match comment_offset(statement) {
        Some(offset) => (
            statement.get(..offset).unwrap_or_default().trim_end(),
            Some(statement.get(offset..).unwrap_or_default().trim_end()),
        ),
        None => (statement, None),
    }
}

/// Whether `comment` can be written at the end of a statement: a `%%` comment
/// that stays on its line.
pub fn is_valid_trailing_comment(comment: &str) -> bool {
    comment.starts_with("%%") && !comment.contains(['\n', '\r'])
}

/// The model item a trailing comment belongs to.
#[derive(Debug)]
enum CommentSite {
    Entity(String),
    Attribute { entity: String, index: usize },
    Relationship(usize),
}

/// A trailing comment, the item it is meant for, and the statement it was
/// written after.
#[derive(Debug)]
struct TrailingComment {
    site: CommentSite,
    comment: String,
    statement: String,
}

/// The trailing comments of a diagram, and the counts the lines they were read
/// from imply, which have to match the model's before the comments can be
/// handed to its items.
#[derive(Debug, Default)]
struct TrailingComments {
    comments: Vec<TrailingComment>,
    relationship_lines: usize,
    attribute_lines: Vec<(String, usize)>,
}

/// What a statement outside an attribute block is.
enum TopLevelStatement {
    /// An entity declaration that opens an attribute block on this line.
    BlockHead(String),
    Entity(String),
    Relationship,
    /// `direction` and `class`, which the model carries but has no row for a
    /// comment to travel with.
    Other,
}

/// Reads the trailing comments off `statements`, the lines the scanner handed
/// to the parser, and works out which item each one belongs to.
///
/// The model re-spells every statement, so a comment only survives by riding
/// on the item it was written after. A comment with no item to ride on is
/// refused rather than moved somewhere it would no longer explain.
fn trailing_comments(statements: &[String]) -> Result<TrailingComments> {
    let mut found = TrailingComments::default();
    let mut depth = 0usize;
    let mut block_entity = String::new();
    for statement in statements {
        let (body, comment) = split_comment(statement);
        if depth > 0 {
            if body.starts_with('}') {
                depth -= 1;
                if comment.is_some() {
                    bail!(i18n::t!(
                        "The comment at the end of the line that closes an attribute block ({statement}) would be lost in the structured editor. Move the comment to its own line, or edit it as text."
                    ));
                }
                continue;
            }
            if body.is_empty() {
                bail!(i18n::t!(
                    "Comments inside an attribute block would be lost in the structured editor ({statement}). Move the comment out of the block, or edit it as text."
                ));
            }
            // `int id }` closes the block after its attribute, and the lines
            // below it are statements again.
            if contains_outside_quotes(body, '}') {
                depth -= 1;
                if comment.is_some() {
                    bail!(i18n::t!(
                        "The comment at the end of the line that closes an attribute block ({statement}) would be lost in the structured editor. Move the comment to its own line, or edit it as text."
                    ));
                }
            }
            let index = match found
                .attribute_lines
                .iter_mut()
                .find(|(entity, _)| *entity == block_entity)
            {
                Some((_, count)) => {
                    *count += 1;
                    *count - 1
                }
                None => {
                    found.attribute_lines.push((block_entity.clone(), 1));
                    0
                }
            };
            if let Some(comment) = comment {
                found.comments.push(TrailingComment {
                    site: CommentSite::Attribute {
                        entity: block_entity.clone(),
                        index,
                    },
                    comment: comment.to_string(),
                    statement: body.to_string(),
                });
            }
            continue;
        }

        if body.is_empty() {
            // Only the rest of the `erDiagram` line gets here, since a comment
            // on a line of its own is kept verbatim by the scanner.
            bail!(i18n::t!(
                "The comment at the end of the `erDiagram` line ({statement}) would be lost in the structured editor. Move the comment to its own line, or edit it as text."
            ));
        }
        let site = match classify_statement(body) {
            TopLevelStatement::BlockHead(name) => {
                depth += 1;
                block_entity = name.clone();
                CommentSite::Entity(name)
            }
            TopLevelStatement::Entity(name) => CommentSite::Entity(name),
            TopLevelStatement::Relationship => {
                found.relationship_lines += 1;
                CommentSite::Relationship(found.relationship_lines - 1)
            }
            TopLevelStatement::Other => {
                if comment.is_some() {
                    bail!(i18n::t!(
                        "The structured editor would lose the comment at the end of the statement ({statement}). Move the comment to a line of its own, or edit it as text."
                    ));
                }
                continue;
            }
        };
        if let Some(comment) = comment {
            found.comments.push(TrailingComment {
                site,
                comment: comment.to_string(),
                statement: body.to_string(),
            });
        }
    }
    Ok(found)
}

fn contains_outside_quotes(text: &str, wanted: char) -> bool {
    let mut quoted = false;
    for character in text.chars() {
        if character == '"' {
            quoted = !quoted;
        } else if character == wanted && !quoted {
            return true;
        }
    }
    false
}

fn classify_statement(body: &str) -> TopLevelStatement {
    if is_direction_statement(body) {
        return TopLevelStatement::Other;
    }
    if body
        .get(.."class".len())
        .is_some_and(|head| head.eq_ignore_ascii_case("class"))
        && body
            .get("class".len()..)
            .is_some_and(|rest| rest.starts_with(char::is_whitespace))
    {
        return TopLevelStatement::Other;
    }
    let Some((name, rest)) = read_entity_name(body) else {
        return TopLevelStatement::Relationship;
    };
    let mut rest = rest.trim_start();
    // An alias can only be written on a declaration of its own.
    if let Some(alias) = rest.strip_prefix('[') {
        let alias = alias.trim_start();
        let after_quote = match alias.strip_prefix('"') {
            Some(quoted) => quoted.find('"').and_then(|end| quoted.get(end + 1..)),
            None => Some(alias),
        };
        let Some(close) = after_quote.and_then(|text| text.find(']').map(|end| (text, end))) else {
            return TopLevelStatement::Relationship;
        };
        rest = close.0.get(close.1 + 1..).unwrap_or_default().trim_start();
    }
    if let Some(classes) = rest.strip_prefix(":::") {
        let mut remainder = classes.trim_start();
        loop {
            let end = remainder
                .find(|character: char| character.is_whitespace() || character == ',')
                .unwrap_or(remainder.len());
            remainder = remainder.get(end..).unwrap_or_default().trim_start();
            match remainder.strip_prefix(',') {
                Some(next) => remainder = next.trim_start(),
                None => break,
            }
        }
        rest = remainder;
    }
    if rest.is_empty() {
        return TopLevelStatement::Entity(name);
    }
    if rest == "{" {
        return TopLevelStatement::BlockHead(name);
    }
    if rest.starts_with('{') {
        return TopLevelStatement::Entity(name);
    }
    TopLevelStatement::Relationship
}

/// The entity name a statement opens with, decoded the way the model decodes
/// it, and the rest of the statement.
fn read_entity_name(body: &str) -> Option<(String, &str)> {
    if let Some(quoted) = body.strip_prefix('"') {
        let end = quoted.find('"')?;
        return Some((decode_text(quoted.get(..end)?), quoted.get(end + 1..)?));
    }
    let end = body
        .find(|character: char| {
            character.is_whitespace() || matches!(character, '[' | '{' | '}' | ':' | '|' | '"')
        })
        .unwrap_or(body.len());
    if end == 0 {
        return None;
    }
    Some((decode_text(body.get(..end)?), body.get(end..)?))
}

/// Hands each trailing comment to the item it was written after.
///
/// The lines are matched to the model by counting, so the counts have to
/// agree first, and the line a comment ends has to hold that item and nothing
/// else: Mermaid needs no line break between statements, and a line holding
/// two would otherwise shift the counts after it and put a comment on another
/// row.
fn attach_trailing_comments(document: &mut ErDocument, found: TrailingComments) -> Result<()> {
    for TrailingComment {
        site,
        comment,
        statement,
    } in found.comments
    {
        if !statement_holds_only(&statement, &site, document) {
            bail!(i18n::t!(
                "The line with the end-of-line comment ({comment}) has two or more statements, or it could not be determined which item the comment belongs to. Put one statement on each line, or edit it as text."
            ));
        }
        let slot = match site {
            CommentSite::Entity(name) => document
                .entities
                .iter_mut()
                .find(|entity| entity.name == name)
                .map(|entity| &mut entity.trailing_comment),
            CommentSite::Attribute { entity, index } => {
                let lines = found
                    .attribute_lines
                    .iter()
                    .find(|(name, _)| *name == entity)
                    .map_or(0, |(_, count)| *count);
                document
                    .entities
                    .iter_mut()
                    .find(|candidate| candidate.name == entity)
                    .filter(|candidate| candidate.attributes.len() == lines)
                    .and_then(|candidate| candidate.attributes.get_mut(index))
                    .map(|attribute| &mut attribute.trailing_comment)
            }
            CommentSite::Relationship(index) => (document.relationships.len()
                == found.relationship_lines)
                .then(|| document.relationships.get_mut(index))
                .flatten()
                .map(|relationship| &mut relationship.trailing_comment),
        };
        let Some(slot) = slot else {
            bail!(i18n::t!(
                "Could not determine which item the end-of-line comment ({comment}) belongs to. Move the comment to its own line, or edit it as text."
            ));
        };
        if slot.is_some() {
            bail!(i18n::t!(
                "The same item has two end-of-line comments ({comment}). Merge them into one, or edit it as text."
            ));
        }
        *slot = Some(comment);
    }
    Ok(())
}

/// Whether `statement`, read on its own, is the one item `site` names in
/// `document` and nothing more.
fn statement_holds_only(statement: &str, site: &CommentSite, document: &ErDocument) -> bool {
    let read = |source: String| -> Option<ErDocument> {
        let parsed = crate::parse_for_render_model(&source).ok()??;
        let RenderSemanticModel::Er(model) = parsed.into_parts().1 else {
            return None;
        };
        document_from_model(&model, false, Vec::new()).ok()
    };
    match site {
        CommentSite::Entity(name) => {
            let source = if statement.ends_with('{') {
                format!("{HEADER}\n{statement}\n}}")
            } else {
                format!("{HEADER}\n{statement}")
            };
            read(source).is_some_and(|alone| {
                alone.relationships.is_empty()
                    && alone.entities.len() == 1
                    && alone.entities.iter().all(|entity| entity.name == *name)
            })
        }
        CommentSite::Attribute { entity, index } => {
            let Some(expected) = document
                .entities
                .iter()
                .find(|candidate| candidate.name == *entity)
                .and_then(|candidate| candidate.attributes.get(*index))
            else {
                return false;
            };
            let expected = ErAttribute {
                trailing_comment: None,
                ..expected.clone()
            };
            read(format!("{HEADER}\nX {{\n{statement}\n}}")).is_some_and(|alone| {
                alone.relationships.is_empty()
                    && alone
                        .entities
                        .iter()
                        .map(|entity| entity.attributes.as_slice())
                        .eq([std::slice::from_ref(&expected)])
            })
        }
        CommentSite::Relationship(index) => {
            let Some(expected) = document.relationships.get(*index) else {
                return false;
            };
            let expected = ErRelationship {
                trailing_comment: None,
                ..expected.clone()
            };
            read(format!("{HEADER}\n{statement}")).is_some_and(|alone| {
                alone.relationships == [expected]
                    && alone
                        .entities
                        .iter()
                        .all(|entity| entity.attributes.is_empty() && entity.alias.is_none())
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MermaidTheme, render_to_svg};

    fn round_trip(source: &str) -> (ErDocument, String, ErDocument, String) {
        let first = parse_er(source).expect("source should parse");
        let first_text = emit_er(&first);
        let second = parse_er(&first_text)
            .unwrap_or_else(|error| panic!("emitted source should parse: {error}\n{first_text}"));
        let second_text = emit_er(&second);
        (first, first_text, second, second_text)
    }

    /// The emitted source must parse to the same document and emit the same
    /// text, otherwise confirming the editor twice would keep rewriting the
    /// fence.
    fn assert_stable(source: &str) -> String {
        let (first, first_text, second, second_text) = round_trip(source);
        assert_eq!(
            first_text, second_text,
            "emitting twice changed the source:\n{first_text}\n---\n{second_text}"
        );
        assert_eq!(
            first.entities, second.entities,
            "entities changed through a round trip of:\n{first_text}"
        );
        assert_eq!(
            first.relationships, second.relationships,
            "relationships changed through a round trip of:\n{first_text}"
        );
        assert_eq!(
            first.direction, second.direction,
            "direction changed through a round trip of:\n{first_text}"
        );
        assert_eq!(
            first.structure_error(),
            None,
            "a parsed document should always be writable back:\n{first_text}"
        );
        first_text
    }

    /// `merman` writes an ER label's font stack into a `style` attribute after
    /// escaping its quotes, and then trims a trailing `;` off the declaration —
    /// which is the `;` of the last `&quot;` when the stack ends with a quoted
    /// font name, leaving an SVG no XML parser accepts. `unescape_attribute_value`
    /// puts that `;` back, so these render with the long fallback stack of
    /// `MermaidTheme::default` — the one the preview really uses — instead of
    /// working around it with a plain family, and go red again if that repair
    /// is ever lost.
    fn assert_renders(source: &str) {
        let svg = render_to_svg(source, &MermaidTheme::default())
            .unwrap_or_else(|error| panic!("emitted source should render: {error}\n{source}"));
        assert!(
            !svg.contains("Syntax error in text"),
            "emitted source rendered an error diagram:\n{source}"
        );
    }

    const EXAMPLES: [&str; 10] = [
        "erDiagram\n    CUSTOMER ||--o{ ORDER : \"発注する\"\n    ORDER ||--|{ ORDER_ITEM : \"含む\"\n    CUSTOMER {\n        int id\n        string name\n    }",
        "erDiagram\n  A ||--|| B : x",
        "erDiagram\n  direction LR\n  A ||--o{ B : has",
        "erDiagram\n  CUSTOMER {\n    int id PK\n    string name UK \"表示名\"\n    string note FK, UK\n  }",
        "erDiagram\n  \"顧客\" ||--o{ \"注文\" : \"発注する\"",
        // An alias can only be written on a declaration of its own, which is
        // where `emit_er` puts it too.
        "erDiagram\n  CUSTOMER[\"お客様\"]\n  ORDER[\"ご注文\"]\n  CUSTOMER ||--o{ ORDER : \"発注する\"",
        "erDiagram\n  A }o..o{ B : \"多対多 (任意)\"\n  C |o--|| D : \"1 対 0..1\"",
        "erDiagram\n  %% コメント\n  A ||--|| B : x\n  classDef warn fill:#f96\n  A:::warn",
        "erDiagram\n  PERSON u--o{ CHILD : \"親子\"",
        "erDiagram\n  A {\n    list~string~ tags\n    int count\n  }\n  A ||--|| B : x",
    ];

    #[test]
    fn examples_round_trip_and_render() {
        for source in EXAMPLES {
            let emitted = assert_stable(source);
            assert_renders(&emitted);
        }
    }

    #[test]
    fn er_template_round_trips() {
        let template = "erDiagram\n    %% ||--o{ は 1 対多、}o--o{ は多対多\n    CUSTOMER ||--o{ ORDER : \"発注する\"\n    ORDER ||--|{ ORDER_ITEM : \"含む\"\n    CUSTOMER {\n        int id\n        string name\n    }";
        let emitted = assert_stable(template);
        assert!(
            emitted.contains("%% ||--o{ は 1 対多"),
            "the template comment should survive:\n{emitted}"
        );
        assert!(
            emitted.contains("int id"),
            "the attributes should survive:\n{emitted}"
        );
        assert_renders(&emitted);
    }

    #[test]
    fn every_cardinality_pair_round_trips() {
        let left_hand = [
            ErCardinality::ExactlyOne,
            ErCardinality::ZeroOrOne,
            ErCardinality::ZeroOrMore,
            ErCardinality::OneOrMore,
            ErCardinality::MarkdownParent,
        ];
        for left in left_hand {
            for right in ErCardinality::ALL {
                for identifying in [true, false] {
                    let relationship = ErRelationship {
                        left: "A".into(),
                        right: "B".into(),
                        left_cardinality: left,
                        right_cardinality: right,
                        identifying,
                        label: "x".into(),
                        trailing_comment: None,
                    };
                    let source = format!(
                        "erDiagram\n{INDENT}{}",
                        relationship_statement(&relationship)
                    );
                    let document = parse_er(&source).unwrap_or_else(|error| {
                        panic!("{left:?}/{right:?} should parse: {error}\n{source}")
                    });
                    assert_eq!(
                        document.relationships.first(),
                        Some(&relationship),
                        "for {source}"
                    );
                    assert_stable(&source);
                }
            }
        }
    }

    #[test]
    fn attribute_forms_round_trip() {
        let source = "erDiagram\n  CUSTOMER {\n    int id PK\n    string name\n    string mail UK \"連絡先\"\n    int owner FK, UK \"所有者\"\n  }";
        let document = parse_er(source).expect("source should parse");
        let entity = document.entities.first().expect("one entity");
        assert_eq!(
            entity.attributes,
            vec![
                ErAttribute {
                    type_name: "int".into(),
                    name: "id".into(),
                    keys: vec![ErAttributeKey::Primary],
                    comment: None,
                    trailing_comment: None,
                },
                ErAttribute {
                    type_name: "string".into(),
                    name: "name".into(),
                    keys: Vec::new(),
                    comment: None,
                    trailing_comment: None,
                },
                ErAttribute {
                    type_name: "string".into(),
                    name: "mail".into(),
                    keys: vec![ErAttributeKey::Unique],
                    comment: Some("連絡先".into()),
                    trailing_comment: None,
                },
                ErAttribute {
                    type_name: "int".into(),
                    name: "owner".into(),
                    keys: vec![ErAttributeKey::Foreign, ErAttributeKey::Unique],
                    comment: Some("所有者".into()),
                    trailing_comment: None,
                },
            ]
        );
        let emitted = assert_stable(source);
        assert_renders(&emitted);
    }

    /// Names Mermaid would take apart if they were written bare have to come
    /// back as themselves, because the editor has no other way to spell them.
    #[test]
    fn names_that_need_quoting_survive() {
        for name in [
            "空白 を 含む",
            "顧客マスター",
            "one",
            "many",
            "total",
            "1st",
            "class",
            "direction",
            "u-turn",
            "引用 \" あり",
            "百分率 % あり",
            "円記号 \\ あり",
            "番号 # 記号",
            "コロン : あり",
            "セミコロン ; あり",
            "コメント %% ではない",
        ] {
            assert!(is_valid_entity_name(name), "`{name}` should be usable");
            let relationship = ErRelationship {
                left: name.to_string(),
                right: "B".into(),
                left_cardinality: ErCardinality::ExactlyOne,
                right_cardinality: ErCardinality::ZeroOrMore,
                identifying: true,
                label: name.to_string(),
                trailing_comment: None,
            };
            let source = format!(
                "erDiagram\n{INDENT}{}",
                relationship_statement(&relationship)
            );
            let document = parse_er(&source)
                .unwrap_or_else(|error| panic!("`{name}` should parse: {error}\n{source}"));
            assert_eq!(
                document.relationships.first(),
                Some(&relationship),
                "for {source}"
            );
            assert_renders(&source);
        }
    }

    /// An attribute comment is a quoted string like a name, so it needs the
    /// same escaping.
    #[test]
    fn attribute_comments_with_syntax_characters_survive() {
        for comment in [
            "引用 \" あり",
            "百分率 % あり",
            "円記号 \\ あり",
            "コメント %% ではない",
            "番号 # と ; あり",
        ] {
            let mut document = ErDocument::empty();
            document.entities.push(ErEntity {
                name: "A".into(),
                alias: None,
                attributes: vec![ErAttribute {
                    type_name: "int".into(),
                    name: "id".into(),
                    keys: vec![ErAttributeKey::Primary],
                    comment: Some(comment.to_string()),
                    trailing_comment: None,
                }],
                classes: Vec::new(),
                trailing_comment: None,
            });
            let emitted = emit_er(&document);
            let parsed = parse_er(&emitted)
                .unwrap_or_else(|error| panic!("`{comment}` should parse: {error}\n{emitted}"));
            assert_eq!(parsed.entities, document.entities, "{emitted}");
            assert_renders(&emitted);
        }
    }

    #[test]
    fn an_alias_and_a_class_survive() {
        let source = "erDiagram\n  CUSTOMER[\"お客様\"]:::warn {\n    int id\n  }\n  classDef warn fill:#f96";
        let document = parse_er(source).expect("source should parse");
        let entity = document.entities.first().expect("one entity");
        assert_eq!(entity.alias.as_deref(), Some("お客様"));
        assert_eq!(entity.classes, vec!["warn".to_string()]);

        let emitted = assert_stable(source);
        assert_eq!(
            emitted.matches(":::warn").count(),
            1,
            "the class must not be applied twice:\n{emitted}"
        );
        assert_renders(&emitted);
    }

    /// A `class` statement and the inline `:::` mean the same thing, so the
    /// statement is written back as the inline form rather than kept as well.
    #[test]
    fn a_class_statement_becomes_the_inline_form() {
        let source = "erDiagram\n  A ||--|| B : x\n  class A,B warn\n  classDef warn fill:#f96";
        let document = parse_er(source).expect("source should parse");
        assert_eq!(
            document
                .entities
                .iter()
                .map(|entity| entity.classes.clone())
                .collect::<Vec<_>>(),
            vec![vec!["warn".to_string()], vec!["warn".to_string()]]
        );
        let emitted = assert_stable(source);
        assert!(emitted.contains("A:::warn"), "{emitted}");
        assert!(!emitted.contains("class A,B warn"), "{emitted}");
        assert!(emitted.contains("classDef warn"), "{emitted}");
        assert_renders(&emitted);
    }

    #[test]
    fn a_style_statement_is_preserved_verbatim() {
        let emitted = assert_stable("erDiagram\n  A ||--|| B : x\n  style A fill:#f96,stroke:#333");
        assert!(
            emitted.contains("style A fill:#f96,stroke:#333"),
            "{emitted}"
        );
    }

    /// The cardinality of a relationship is spelled with braces — `o{`, `|{`,
    /// `}o` — and the scanner counts braces to know when it is inside an
    /// attribute block. A relationship must not leave it stuck there, or every
    /// statement the model keeps verbatim below it is silently dropped.
    #[test]
    fn a_relationship_does_not_hide_the_statements_under_it() {
        let emitted = assert_stable(
            "erDiagram
    顧客 ||--o{ 注文 : 発注
    在庫 {
        int 数
    }
    %% 注釈
    style 顧客 fill:#f96
    classDef warn fill:#fdd",
        );
        for kept in ["%% 注釈", "style 顧客 fill:#f96", "classDef warn fill:#fdd"] {
            assert!(emitted.contains(kept), "`{kept}` was dropped:\n{emitted}");
        }

        let document = parse_er(
            "erDiagram
    顧客 ||--|{ 注文 : 発注
    direction LR",
        )
        .expect("source should parse");
        assert_eq!(
            document.direction,
            Some(ErDirection::LeftToRight),
            "a direction below a relationship should still be read"
        );
    }

    #[test]
    fn the_declaration_order_of_entities_is_kept() {
        let source = "erDiagram\n  ZEBRA ||--o{ APPLE : x\n  APPLE ||--o{ MANGO : y";
        let document = parse_er(source).expect("source should parse");
        let names = document
            .entities
            .iter()
            .map(|entity| entity.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec!["ZEBRA", "APPLE", "MANGO"],
            "the order the diagram introduced them, not alphabetical"
        );
        let emitted = assert_stable(source);
        let zebra = emitted
            .lines()
            .position(|line| line.trim() == "ZEBRA")
            .expect("ZEBRA should be declared");
        let apple = emitted
            .lines()
            .position(|line| line.trim() == "APPLE")
            .expect("APPLE should be declared");
        assert!(
            zebra < apple,
            "the layout order must not change:\n{emitted}"
        );
    }

    #[test]
    fn a_written_direction_is_kept_and_a_missing_one_is_not_invented() {
        let with =
            parse_er("erDiagram\n  direction TB\n  A ||--|| B : x").expect("source should parse");
        assert_eq!(with.direction, Some(ErDirection::TopToBottom));
        let emitted = assert_stable("erDiagram\n  direction TB\n  A ||--|| B : x");
        assert!(emitted.contains("direction TB"), "{emitted}");

        let without = parse_er("erDiagram\n  A ||--|| B : x").expect("source should parse");
        assert_eq!(without.direction, None);
        assert!(!emit_er(&without).contains("direction"), "{without:?}");

        for direction in ErDirection::ALL {
            let source = format!(
                "erDiagram\n  direction {}\n  A ||--|| B : x",
                direction.keyword()
            );
            let document = parse_er(&source)
                .unwrap_or_else(|error| panic!("`{}` should parse: {error}", direction.keyword()));
            assert_eq!(document.direction, Some(direction));
            assert_renders(&assert_stable(&source));
        }
    }

    #[test]
    fn preserved_lines_keep_their_side_of_the_first_statement() {
        let source = "%%{init: {\"theme\": \"dark\"}}%%\nerDiagram\n  %% 先頭のメモ\n  A ||--|| B : x\n  %% 末尾のメモ\n  accTitle: 図の名前\n  classDef warn fill:#f96";
        let document = parse_er(source).expect("source should parse");
        let positions = document
            .preserved_lines
            .iter()
            .map(|line| (line.position, line.text.trim().to_string()))
            .collect::<Vec<_>>();
        assert_eq!(
            positions,
            vec![
                (
                    PreservedPosition::Preamble,
                    "%%{init: {\"theme\": \"dark\"}}%%".to_string()
                ),
                (PreservedPosition::Top, "%% 先頭のメモ".to_string()),
                (PreservedPosition::Bottom, "%% 末尾のメモ".to_string()),
                (PreservedPosition::Bottom, "accTitle: 図の名前".to_string()),
                (
                    PreservedPosition::Bottom,
                    "classDef warn fill:#f96".to_string()
                ),
            ]
        );

        let emitted = emit_er(&document);
        assert!(
            emitted
                .lines()
                .next()
                .unwrap_or_default()
                .starts_with("%%{init"),
            "the init directive must stay before the diagram keyword:\n{emitted}"
        );
        assert!(
            emitted
                .lines()
                .nth(1)
                .unwrap_or_default()
                .starts_with(HEADER),
            "{emitted}"
        );
        assert_stable(source);
    }

    /// An attribute called `style` is a word inside a block, not the statement
    /// of the same name.
    #[test]
    fn keywords_inside_an_attribute_block_stay_attributes() {
        let source = "erDiagram\n  A {\n    string style\n    string accTitle\n    string classDef\n  }\n  A ||--|| B : x";
        let document = parse_er(source).expect("source should parse");
        let entity = document.entities.first().expect("one entity");
        assert_eq!(
            entity
                .attributes
                .iter()
                .map(|attribute| attribute.name.as_str())
                .collect::<Vec<_>>(),
            vec!["style", "accTitle", "classDef"]
        );
        assert!(
            document.preserved_lines.is_empty(),
            "{:?}",
            document.preserved_lines
        );
        assert_renders(&assert_stable(source));
    }

    #[test]
    fn a_frontmatter_block_stays_ahead_of_the_keyword() {
        let source = "---\ntitle: 図の名前\n---\nerDiagram\n  A ||--|| B : x";
        let emitted = assert_stable(source);
        assert!(
            emitted.starts_with("---\ntitle: 図の名前\n---\n"),
            "{emitted}"
        );
        assert_renders(&emitted);
    }

    #[test]
    fn crlf_and_indentation_do_not_matter() {
        let crlf = "erDiagram\r\n\tA ||--|| B : x\r\n\tA {\r\n\t\tint id\r\n\t}\r\n";
        let document = parse_er(crlf).expect("CRLF source should parse");
        assert_eq!(document.entities.len(), 2);
        assert_eq!(
            document
                .entities
                .first()
                .map(|entity| entity.attributes.len()),
            Some(1)
        );
        assert_stable(crlf);
    }

    #[test]
    fn an_empty_fence_opens_as_a_blank_diagram() {
        let document = parse_er("").expect("an empty fence should parse");
        assert!(document.entities.is_empty());
        assert!(document.relationships.is_empty());
        assert_eq!(emit_er(&document), "erDiagram");
        assert_eq!(emit_er(&ErDocument::empty()), "erDiagram");
        assert_stable("erDiagram");
        assert_stable("erDiagram\n  %% まだ空");
    }

    #[test]
    fn other_diagram_types_are_refused() {
        for source in [
            "flowchart TD\n  A --> B",
            "sequenceDiagram\n  A->>B: x",
            "pie title x\n  \"a\" : 1",
        ] {
            assert!(!is_er_source(source), "{source}");
            assert!(parse_er(source).is_err(), "{source}");
        }
        assert!(is_er_source("erDiagram\n  A ||--|| B : x"));
        assert!(is_er_source("%% note\nerDiagram\n  A ||--|| B : x"));
        assert!(is_er_source("---\ntitle: x\n---\nerDiagram"));
    }

    /// A comment written after a statement rides on the item the statement
    /// became, so it is written back after that item wherever the emitter
    /// puts it. The whole emitted text is compared, since a comment dropped
    /// from both sides of a round trip would pass a comparison of documents.
    #[test]
    fn an_end_of_line_comment_rides_on_its_item() {
        let source = "erDiagram
  A ||--o{ B : has %% 行末のメモ
  B %% 宣言のメモ
  A[\"別名\"]:::warn { %% ブロックのメモ
    int id PK %% 主キーのメモ
    string name
    string mail \"連絡先 %% ではない\" %% 属性のメモ
  }
  C ||--|| A : \"100%% 完了\"
  classDef warn fill:#f96";
        let document = parse_er(source).expect("source should parse");
        assert_eq!(
            document
                .relationships
                .iter()
                .map(|relationship| relationship.trailing_comment.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("%% 行末のメモ"), None]
        );
        let entity = |name: &str| {
            document
                .entities
                .iter()
                .find(|entity| entity.name == name)
                .unwrap_or_else(|| panic!("{name} should be an entity"))
        };
        assert_eq!(
            entity("B").trailing_comment.as_deref(),
            Some("%% 宣言のメモ")
        );
        assert_eq!(
            entity("A").trailing_comment.as_deref(),
            Some("%% ブロックのメモ")
        );
        assert_eq!(
            entity("A")
                .attributes
                .iter()
                .map(|attribute| attribute.trailing_comment.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("%% 主キーのメモ"), None, Some("%% 属性のメモ")]
        );
        assert_eq!(
            entity("A")
                .attributes
                .get(2)
                .and_then(|attribute| attribute.comment.as_deref()),
            Some("連絡先 %% ではない"),
            "a comment marker inside the attribute's quoted comment is text"
        );

        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "erDiagram
    A[別名]:::warn { %% ブロックのメモ
        int id PK %% 主キーのメモ
        string name
        string mail \"連絡先 #37;#37; ではない\" %% 属性のメモ
    }
    B %% 宣言のメモ
    C
    A ||--o{ B : has %% 行末のメモ
    C ||--|| A : \"100#37;#37; 完了\"
  classDef warn fill:#f96"
        );
        assert_renders(&emitted);
    }

    /// Moving a row keeps its comment with it, and a new row has none.
    #[test]
    fn a_comment_stays_with_its_row_when_the_rows_move() {
        let source = "erDiagram\n  A ||--|| B : x1 %% 一つ目\n  A ||--|| C : x2";
        let mut document = parse_er(source).expect("source should parse");
        document.relationships.swap(0, 1);
        let emitted = emit_er(&document);
        assert!(
            emitted.ends_with("    A ||--|| C : x2\n    A ||--|| B : x1 %% 一つ目"),
            "{emitted}"
        );
    }

    #[test]
    fn a_comment_with_no_item_to_ride_on_is_refused() {
        for source in [
            "erDiagram %% 図のメモ\n  A ||--|| B : x",
            "erDiagram\n  A {\n    int id\n  } %% 閉じのメモ",
            "erDiagram\n  A {\n    %% ブロック内のメモ\n    int id\n  }",
            "erDiagram\n  direction LR %% 向きのメモ\n  A ||--|| B : x",
            "erDiagram\n  A ||--|| B : x\n  class A warn %% クラスのメモ",
            "erDiagram\n  A %% 一つ目\n  A %% 二つ目",
        ] {
            let error = parse_er(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            assert!(
                format!("{error}")
                    .to_lowercase()
                    .contains("edit it as text"),
                "{source}: {error}"
            );
        }
    }

    /// Mermaid needs no line break between statements, so a line can hold
    /// two, which shifts the counting the comments are matched by. A comment
    /// on such a line, or on a line after it, must not land on another row.
    #[test]
    fn a_comment_on_a_line_of_several_statements_is_refused() {
        for source in [
            // After the relationship, not after the entity the line opens with.
            "erDiagram\n  A { int id } A ||--|| B : x %% m",
            // Two relationships on a line, and two entities on the commented
            // one: the counts agree, but the comment is not E's.
            "erDiagram\n  A ||--|| B : x C ||--|| D : y\n  E ||--|| F : z\n  G[\"g\"] H %% m",
            // The same comment after two attributes on one line.
            "erDiagram\n  A {\n    int a string b %% m\n  }",
        ] {
            assert_renders(source);
            let error = parse_er(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            assert!(
                format!("{error}")
                    .to_lowercase()
                    .contains("edit it as text"),
                "{source}: {error}"
            );
        }
    }

    /// A `}` after the last attribute closes the block on that line, so the
    /// line below is a relationship again and keeps its own comment.
    #[test]
    fn a_block_closed_after_an_attribute_ends_the_attribute_lines() {
        let source = "erDiagram\n  A {\n    int a string b\n    int c }\n  C ||--|| D : x %% m";
        assert_renders(source);
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "erDiagram\n    A {\n        int a\n        string b\n        int c\n    }\n    C\n    D\n    C ||--|| D : x %% m"
        );
    }

    /// A re-declared entity, an alias and a quoted name keep each comment on
    /// the row it was written after.
    #[test]
    fn a_comment_stays_on_its_row_across_declarations() {
        let source = "erDiagram
  A[\"x\"] %% 別名のメモ
  \"A B\" ||--|| A : y %% 関係のメモ
  A {
    int a %% 一つ目
  }
  A {
    int b %% 二つ目
  }";
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "erDiagram
    A[x] { %% 別名のメモ
        int a %% 一つ目
        int b %% 二つ目
    }
    \"A B\"
    \"A B\" ||--|| A : y %% 関係のメモ"
        );
        assert_renders(&emitted);
    }

    /// A brace inside a comment opens no attribute block, so the statements
    /// under it are still read as statements and kept.
    #[test]
    fn a_brace_in_a_comment_opens_no_block() {
        let emitted =
            assert_stable("erDiagram\n  A ||--|| B : x %% {\n  style A fill:#f96\n  %% 後のメモ");
        assert!(emitted.contains("A ||--|| B : x %% {"), "{emitted}");
        assert!(emitted.contains("style A fill:#f96"), "{emitted}");
        assert!(emitted.contains("%% 後のメモ"), "{emitted}");
    }

    /// An `accDescr {` block whose text starts on its opening line runs to the
    /// first `}`. The lines inside it used to be taken for statements and
    /// dropped, which wrote back a block Mermaid could not read.
    #[test]
    fn an_accessibility_block_with_text_on_its_first_line_is_kept_whole() {
        let source = "erDiagram\naccTitle: graph title\naccDescr { this graph is\n  about\n  stuff\n  }\nTEACHER }o--o{ STUDENT : \"is teacher of\"";
        let emitted = assert_stable(source);
        assert!(
            emitted.contains("accDescr { this graph is\n  about\n  stuff\n  }"),
            "{emitted}"
        );
        render_to_svg(&emitted, &MermaidTheme::default())
            .unwrap_or_else(|error| panic!("`{emitted}` should render: {error}"));
    }

    /// `#34;` in a relationship label reads as a quotation mark, is written
    /// back as `#quot;`, which reads the same, and both spellings are drawn
    /// with the quotation mark.
    #[test]
    fn a_numbered_quote_entity_in_a_label_round_trips() {
        for source in [
            "erDiagram\n  A ||--|| B : \"a #34;引用#34; b\"",
            "erDiagram\n  A ||--|| B : a#34;b",
        ] {
            let document =
                parse_er(source).unwrap_or_else(|error| panic!("`{source}` should parse: {error}"));
            let label = document
                .relationships
                .first()
                .map(|relationship| relationship.label.clone())
                .expect("one relationship");
            assert!(label.contains('"'), "`{source}` read as {label:?}");
            let emitted = assert_stable(source);
            assert!(emitted.contains("#quot;"), "{emitted}");
            // `merman` 0.7 put each word in a `<tspan>` of its own; 0.8 writes
            // the label as one text, with the quote escaped or not.
            let drawn_forms = [
                format!(">{label}<"),
                format!(">{}<", label.replace('"', "&quot;")),
            ];
            for drawn in [source, emitted.as_str()] {
                let svg = render_to_svg(drawn, &MermaidTheme::default())
                    .unwrap_or_else(|error| panic!("`{drawn}` should render: {error}"));
                assert!(
                    drawn_forms.iter().any(|form| svg.contains(form.as_str())),
                    "`{drawn}` should draw {label:?}"
                );
            }
        }
    }

    #[test]
    fn names_and_words_are_validated() {
        assert!(is_valid_entity_name("CUSTOMER"));
        assert!(is_valid_entity_name("空白 あり"));
        assert!(!is_valid_entity_name(""));

        assert!(is_valid_attribute_word("int"));
        assert!(is_valid_attribute_word("列名"));
        assert!(is_valid_attribute_word("list~string~"));
        assert!(is_valid_attribute_word("value[0]"));
        assert!(!is_valid_attribute_word(""));
        assert!(!is_valid_attribute_word("PK"));
        assert!(!is_valid_attribute_word("a b"));
        assert!(!is_valid_attribute_word("a,b"));
        assert!(!is_valid_attribute_word("a\"b"));
        assert!(!is_valid_attribute_word("1st"));

        assert!(is_valid_class_name("warn"));
        assert!(!is_valid_class_name(""));
        assert!(!is_valid_class_name("a b"));
        assert!(!is_valid_class_name("a,b"));

        assert!(is_valid_cardinality_pair(
            ErCardinality::MarkdownParent,
            ErCardinality::ZeroOrMore
        ));
        assert!(!is_valid_cardinality_pair(
            ErCardinality::ExactlyOne,
            ErCardinality::MarkdownParent
        ));
    }

    /// Words the editor accepts must survive as themselves, which is the whole
    /// point of the check above.
    #[test]
    fn accepted_attribute_words_round_trip() {
        for word in ["int", "列名", "list~string~", "value[0]", "a-b", "_x"] {
            assert!(is_valid_attribute_word(word), "`{word}` should be accepted");
            let source =
                format!("erDiagram\n{INDENT}A {{\n{INDENT}{INDENT}{word} {word}\n{INDENT}}}");
            let document = parse_er(&source)
                .unwrap_or_else(|error| panic!("`{word}` should parse: {error}\n{source}"));
            let attribute = document
                .entities
                .first()
                .and_then(|entity| entity.attributes.first())
                .unwrap_or_else(|| panic!("`{word}` should produce an attribute\n{source}"));
            assert_eq!(attribute.type_name, word);
            assert_eq!(attribute.name, word);
            assert_stable(&source);
        }
    }

    #[test]
    fn unused_entity_name_skips_taken_names() {
        let mut document = ErDocument::empty();
        for name in ["E", "E1"] {
            document.entities.push(ErEntity {
                name: name.into(),
                alias: None,
                attributes: Vec::new(),
                classes: Vec::new(),
                trailing_comment: None,
            });
        }
        assert_eq!(document.unused_entity_name("E"), "E2");
        assert_eq!(document.unused_entity_name("F"), "F");
    }

    #[test]
    fn structure_error_catches_documents_mermaid_would_reject() {
        let mut document = ErDocument::empty();
        document.relationships.push(ErRelationship {
            left: "A".into(),
            right: "B".into(),
            left_cardinality: ErCardinality::ExactlyOne,
            right_cardinality: ErCardinality::ExactlyOne,
            identifying: true,
            label: "x".into(),
            trailing_comment: None,
        });
        assert!(
            document
                .structure_error()
                .is_some_and(|message| message.contains('A')),
            "an undeclared entity should be reported"
        );

        let mut document = ErDocument::empty();
        for _ in 0..2 {
            document.entities.push(ErEntity {
                name: "A".into(),
                alias: None,
                attributes: Vec::new(),
                classes: Vec::new(),
                trailing_comment: None,
            });
        }
        assert!(
            document.structure_error().is_some(),
            "a duplicate entity should be reported"
        );

        let mut document = ErDocument::empty();
        document.entities.push(ErEntity {
            name: "A".into(),
            alias: None,
            attributes: vec![ErAttribute {
                type_name: "int".into(),
                name: String::new(),
                keys: Vec::new(),
                comment: None,
                trailing_comment: None,
            }],
            classes: Vec::new(),
            trailing_comment: None,
        });
        assert!(
            document.structure_error().is_some(),
            "an attribute without a name should be reported"
        );
    }

    /// A document the editor built by hand must still emit source Mermaid
    /// accepts, which is what the checks above are for.
    #[test]
    fn a_document_built_by_hand_emits_valid_source() {
        let mut document = ErDocument::empty();
        document.direction = Some(ErDirection::LeftToRight);
        document.entities.push(ErEntity {
            name: "顧客".into(),
            alias: Some("お客様".into()),
            attributes: vec![ErAttribute {
                type_name: "int".into(),
                name: "id".into(),
                keys: vec![ErAttributeKey::Primary],
                comment: Some("採番 # 付き".into()),
                trailing_comment: None,
            }],
            classes: vec!["warn".into()],
            trailing_comment: None,
        });
        document.entities.push(ErEntity {
            name: "注文".into(),
            alias: None,
            attributes: Vec::new(),
            classes: Vec::new(),
            trailing_comment: None,
        });
        document.relationships.push(ErRelationship {
            left: "顧客".into(),
            right: "注文".into(),
            left_cardinality: ErCardinality::ExactlyOne,
            right_cardinality: ErCardinality::ZeroOrMore,
            identifying: false,
            label: "発注する; 何度でも".into(),
            trailing_comment: None,
        });
        assert_eq!(document.structure_error(), None);

        let emitted = emit_er(&document);
        let parsed = parse_er(&emitted).unwrap_or_else(|error| {
            panic!("a hand built document should parse: {error}\n{emitted}")
        });
        assert_eq!(parsed.direction, document.direction);
        assert_eq!(parsed.entities, document.entities);
        assert_eq!(parsed.relationships, document.relationships);
        assert_renders(&emitted);
    }

    #[test]
    fn further_upstream_examples_round_trip_and_render() {
        for source in [
            "erDiagram\n  A {\n    int id\n  }\n  A {\n    string name\n  }\n  A ||--|| B : x",
            "erDiagram\n  A ||--|| B : x\n  A {\n    int id PK\n  }",
            "erDiagram\n  CAR one to zero or more NAMED_DRIVER : allows",
            "erDiagram\n  PERSON only one optionally to zero or one DRIVER : is",
            "erDiagram\n  A many(1) -- many(0) B : x",
            "erDiagram\n  A 0+ -- 1+ B : x",
            "erDiagram\n  A }|..|{ B : \"1 以上対 1 以上 (非依存)\"",
            "erDiagram\n  A {\n    int id PK, FK \"主キーかつ外部キー\"\n  }\n  A ||--|| B : x",
            "erDiagram\n  \"空白 を 含む\" ||--o{ \"日本語 の 名前\" : \"関係\"",
            "erDiagram\n  A ||--|| B : \"\"",
            "erDiagram\n  direction RL\n  A }o--o{ B : x",
            "erDiagram\n  A[\"別名\"]\n  B[\"別名2\"]\n  A ||--|| B : x",
        ] {
            let emitted = assert_stable(source);
            assert_renders(&emitted);
        }
    }

    /// An attribute word is written bare, and the entity shorthand lets a
    /// diagram written by hand put any character in one. The escape has to
    /// cover all of them, not just the three that also mean something to the
    /// rest of the grammar.
    #[test]
    fn attribute_words_with_decoded_characters_survive() {
        let source = "erDiagram\n  A {\n    int a#quot;b\n    string x#35;y\n    int 名前\n  }";
        let document = parse_er(source).expect("source should parse");
        assert_eq!(
            document.entities.first().map(|entity| entity
                .attributes
                .iter()
                .map(|attribute| attribute.name.as_str())
                .collect::<Vec<_>>()),
            Some(vec!["a\"b", "x#y", "名前"])
        );
        assert_renders(&assert_stable(source));

        for (type_name, name) in [
            ("a b", "id"),
            ("PK", "id"),
            ("int", "a,b"),
            ("int", "a\"b"),
            ("int", "1st"),
            ("list~a~", "generic"),
            ("int", ""),
        ] {
            let mut document = ErDocument::empty();
            document.entities.push(ErEntity {
                name: "A".into(),
                alias: None,
                attributes: vec![ErAttribute {
                    type_name: type_name.into(),
                    name: name.into(),
                    keys: Vec::new(),
                    comment: None,
                    trailing_comment: None,
                }],
                classes: Vec::new(),
                trailing_comment: None,
            });
            let emitted = emit_er(&document);
            if document.structure_error().is_some() {
                continue;
            }
            let parsed = parse_er(&emitted)
                .unwrap_or_else(|error| panic!("`{type_name}` / `{name}`: {error}\n{emitted}"));
            assert_eq!(parsed.entities, document.entities, "{emitted}");
        }
    }

    /// `structure_error` is the editor's only gate before it writes the fence
    /// back, so a word or class name Mermaid could not read has to fail it.
    #[test]
    fn structure_error_catches_words_the_editor_could_not_write() {
        for word in ["", "PK", "fk"] {
            let mut document = ErDocument::empty();
            document.entities.push(ErEntity {
                name: "A".into(),
                alias: None,
                attributes: vec![ErAttribute {
                    type_name: word.to_string(),
                    name: "id".into(),
                    keys: Vec::new(),
                    comment: None,
                    trailing_comment: None,
                }],
                classes: Vec::new(),
                trailing_comment: None,
            });
            assert!(
                document.structure_error().is_some(),
                "`{word}` should be reported, emitted:\n{}",
                emit_er(&document)
            );
        }

        for class in ["", "a b", "a,b"] {
            let mut document = ErDocument::empty();
            document.entities.push(ErEntity {
                name: "A".into(),
                alias: None,
                attributes: Vec::new(),
                classes: vec![class.to_string()],
                trailing_comment: None,
            });
            assert!(
                document.structure_error().is_some(),
                "class `{class}` should be reported, emitted:\n{}",
                emit_er(&document)
            );
        }
    }

    /// The other half of the same contract: whatever the parser accepts, the
    /// editor can write back.
    #[test]
    fn parse_never_yields_a_document_it_could_not_write_back() {
        for source in [
            "erDiagram\n  A",
            "erDiagram\n  A {\n  }",
            "erDiagram\n  A {\n    %% メモ\n    int id\n  }",
            "erDiagram\n  direction TD\n  A ||--|| B : x",
            "erDiagram\n  A ||--|| A : 自己参照",
            "erDiagram\n  😀 ||--|| 😁 : 絵文字",
            "erDiagram\n  A {\n    int id",
            "erDiagram\n  %%{init: {\"theme\": \"dark\"}",
            "---\ntitle: x",
            "erDiagram\n  A ||--|| B : x\n  class A",
            "erDiagram\n  u--o{",
            "erDiagram\n  A {\n    int a#quot;b\n  }",
            // The entity shorthand hides a word Mermaid would read back as a
            // key rather than as the type it is.
            "erDiagram\n  A {\n    #80;K id\n  }",
        ] {
            if let Ok(document) = parse_er(source) {
                assert_eq!(
                    document.structure_error(),
                    None,
                    "parsed but not writable back: {source}"
                );
                let emitted = emit_er(&document);
                parse_er(&emitted).unwrap_or_else(|error| {
                    panic!("`{source}` emitted source Mermaid rejects: {error}\n{emitted}")
                });
            }
        }
    }

    /// An alias only has a spelling on a declaration of its own, and a label
    /// only a quoted one, so both have to survive any character.
    #[test]
    fn aliases_and_labels_with_bracket_characters_survive() {
        for (alias, label) in [
            (Some("]"), "x"),
            (Some("[a]"), "x"),
            (None, "]"),
            (None, ""),
        ] {
            let mut document = ErDocument::empty();
            for name in ["A", "B"] {
                document.entities.push(ErEntity {
                    name: name.into(),
                    alias: alias.map(str::to_string),
                    attributes: Vec::new(),
                    classes: Vec::new(),
                    trailing_comment: None,
                });
            }
            document.relationships.push(ErRelationship {
                left: "A".into(),
                right: "B".into(),
                left_cardinality: ErCardinality::ExactlyOne,
                right_cardinality: ErCardinality::ZeroOrMore,
                identifying: true,
                label: label.to_string(),
                trailing_comment: None,
            });
            assert_eq!(document.structure_error(), None);
            let emitted = emit_er(&document);
            let parsed = parse_er(&emitted).unwrap_or_else(|error| {
                panic!("alias {alias:?} / label `{label}`: {error}\n{emitted}")
            });
            assert_eq!(parsed.entities, document.entities, "{emitted}");
            assert_eq!(parsed.relationships, document.relationships, "{emitted}");
        }
    }

    #[test]
    fn statements_merman_reads_around_a_directive_are_refused() {
        crate::mermaid_model::preprocessing_cases::assert_directive_rules(
            "erDiagram",
            "A ||--o{ B : has",
            "A %%{init: {}}%% ||--o{ B : has",
        );
    }

    #[test]
    fn a_directive_opener_in_a_label_is_written_encoded() {
        let mut document = parse_er("%%{init: {}}%%\nerDiagram\n    A ||--o{ B : has")
            .expect("the diagram should open");
        document.relationships[0].label = "a %%{init: {}}%% b".to_string();
        let whole = crate::mermaid_model::MermaidDocument::Er(document.clone());
        assert_eq!(whole.structure_error(), None);
        let emitted = emit_er(&document);
        crate::render_to_svg(&emitted, &crate::MermaidTheme::default())
            .expect("the emitted diagram should render");
        let again = parse_er(&emitted).expect("the emitted diagram should reopen");
        assert_eq!(again.relationships, document.relationships);
    }
}
