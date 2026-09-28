//! A round-trippable editing model for Mermaid requirement diagrams.
//!
//! The shape mirrors [`crate::er_model`]: [`parse_requirement`] reads the body
//! of a ```` ```mermaid ```` fence into [`RequirementDocument`], and
//! [`emit_requirement`] writes it back. See [`crate::mermaid_model`] for the
//! invariants every diagram kind keeps.
//!
//! `merman` reads a requirement diagram line by line, and its model forgets
//! how a statement was spelled, so [`parse_requirement`] scans the source
//! itself, following `merman`'s own order of checks. Two things keep that scan
//! honest:
//!
//! - `merman` has to accept the fence before it is opened at all.
//! - The emitted source has to parse, in `merman`, to the same model as the
//!   original. The emitter writes the requirements, then the elements, then the
//!   relationships and last the `classDef` / `class` / `style` statements, which
//!   is the order `merman` stores them in anyway; a fence whose picture depends
//!   on where a statement sat — the order two classes were defined in, say —
//!   is refused rather than redrawn.
//!
//! Deliberate re-spellings, none of which change what Mermaid draws: keywords,
//! risks, verification methods and relationship types are written in the
//! spelling Mermaid's documentation uses, `a <- r - b` becomes `b - r -> a`,
//! and the fields of a block come in a fixed order.

use anyhow::{Result, anyhow, bail};
use merman::diagram::RenderSemanticModel;
use merman::diagrams::requirement::RequirementDiagramRenderModel;

use crate::flowchart_model::{PreservedLine, PreservedPosition};
use crate::mermaid_model::{
    INDENT, first_statement_line, join_lines, keyword_tail_ignoring_case, scan_preamble,
};

const HEADER: &str = "requirementDiagram";
const DETECTED_PREFIX: &str = "requirement";

/// The six kinds of requirement block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequirementKind {
    Requirement,
    Functional,
    Interface,
    Performance,
    Physical,
    DesignConstraint,
}

impl RequirementKind {
    pub const ALL: [RequirementKind; 6] = [
        RequirementKind::Requirement,
        RequirementKind::Functional,
        RequirementKind::Interface,
        RequirementKind::Performance,
        RequirementKind::Physical,
        RequirementKind::DesignConstraint,
    ];

    pub fn keyword(self) -> &'static str {
        match self {
            Self::Requirement => "requirement",
            Self::Functional => "functionalRequirement",
            Self::Interface => "interfaceRequirement",
            Self::Performance => "performanceRequirement",
            Self::Physical => "physicalRequirement",
            Self::DesignConstraint => "designConstraint",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Requirement => "要求",
            Self::Functional => "機能要求",
            Self::Interface => "インターフェース要求",
            Self::Performance => "性能要求",
            Self::Physical => "物理要求",
            Self::DesignConstraint => "設計制約",
        }
    }

    fn from_keyword(keyword: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.keyword().eq_ignore_ascii_case(keyword))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequirementRisk {
    Low,
    Medium,
    High,
}

impl RequirementRisk {
    pub const ALL: [RequirementRisk; 3] = [
        RequirementRisk::Low,
        RequirementRisk::Medium,
        RequirementRisk::High,
    ];

    pub fn keyword(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "リスク: 低",
            Self::Medium => "リスク: 中",
            Self::High => "リスク: 高",
        }
    }

    fn from_keyword(keyword: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|risk| risk.keyword().eq_ignore_ascii_case(keyword))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyMethod {
    Analysis,
    Inspection,
    Test,
    Demonstration,
}

impl VerifyMethod {
    pub const ALL: [VerifyMethod; 4] = [
        VerifyMethod::Analysis,
        VerifyMethod::Inspection,
        VerifyMethod::Test,
        VerifyMethod::Demonstration,
    ];

    pub fn keyword(self) -> &'static str {
        match self {
            Self::Analysis => "analysis",
            Self::Inspection => "inspection",
            Self::Test => "test",
            Self::Demonstration => "demonstration",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Analysis => "検証: 分析",
            Self::Inspection => "検証: 検査",
            Self::Test => "検証: テスト",
            Self::Demonstration => "検証: 実演",
        }
    }

    fn from_keyword(keyword: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|method| method.keyword().eq_ignore_ascii_case(keyword))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationshipKind {
    Contains,
    Copies,
    Derives,
    Satisfies,
    Verifies,
    Refines,
    Traces,
}

impl RelationshipKind {
    pub const ALL: [RelationshipKind; 7] = [
        RelationshipKind::Satisfies,
        RelationshipKind::Verifies,
        RelationshipKind::Contains,
        RelationshipKind::Derives,
        RelationshipKind::Refines,
        RelationshipKind::Traces,
        RelationshipKind::Copies,
    ];

    pub fn keyword(self) -> &'static str {
        match self {
            Self::Contains => "contains",
            Self::Copies => "copies",
            Self::Derives => "derives",
            Self::Satisfies => "satisfies",
            Self::Verifies => "verifies",
            Self::Refines => "refines",
            Self::Traces => "traces",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Contains => "含む (contains)",
            Self::Copies => "複製する (copies)",
            Self::Derives => "派生する (derives)",
            Self::Satisfies => "満たす (satisfies)",
            Self::Verifies => "検証する (verifies)",
            Self::Refines => "詳細化する (refines)",
            Self::Traces => "追跡する (traces)",
        }
    }

    fn from_keyword(keyword: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.keyword().eq_ignore_ascii_case(keyword))
    }
}

/// The layout direction of the diagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequirementDirection {
    TopToBottom,
    BottomToTop,
    LeftToRight,
    RightToLeft,
}

impl RequirementDirection {
    pub const ALL: [RequirementDirection; 4] = [
        RequirementDirection::TopToBottom,
        RequirementDirection::LeftToRight,
        RequirementDirection::BottomToTop,
        RequirementDirection::RightToLeft,
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
            Self::TopToBottom => "上から下 (TB)",
            Self::BottomToTop => "下から上 (BT)",
            Self::LeftToRight => "左から右 (LR)",
            Self::RightToLeft => "右から左 (RL)",
        }
    }

    fn from_keyword(keyword: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|direction| direction.keyword().eq_ignore_ascii_case(keyword))
    }
}

/// A field of a block (`id`, `text`, `type`, `docref`). An empty value is a
/// field the block does not write, which Mermaid draws the same way.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequirementValue {
    pub value: String,
    /// Whether the source wrapped the value in quotes. Kept so that opening
    /// and confirming an unchanged fence does not add or drop them; a value
    /// that needs the quotes gets them regardless.
    pub quoted: bool,
}

impl RequirementValue {
    pub fn new(value: impl Into<String>, quoted: bool) -> Self {
        Self {
            value: value.into(),
            quoted,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requirement {
    pub kind: RequirementKind,
    pub name: String,
    pub id: RequirementValue,
    pub text: RequirementValue,
    pub risk: Option<RequirementRisk>,
    pub verify_method: Option<VerifyMethod>,
    /// The classes assigned with `:::` on the block's own first line.
    pub classes: Vec<String>,
    /// Comments written inside the block, written back at its end.
    pub body_comments: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequirementElement {
    pub name: String,
    pub element_type: RequirementValue,
    pub doc_ref: RequirementValue,
    pub classes: Vec<String>,
    pub body_comments: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequirementRelationship {
    pub source: String,
    pub kind: RelationshipKind,
    pub target: String,
}

/// A statement that styles the boxes. The ones that name boxes carry the names
/// separately so that a rename or a deletion can follow them; the styles
/// themselves are kept as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequirementStyleStatement {
    /// A `classDef` line, verbatim. It names classes, not boxes.
    ClassDef(String),
    /// `class a,b warn` — the text after the names is `classes`.
    Class {
        targets: Vec<String>,
        classes: String,
    },
    /// `style a,b fill:#f96` — the text after the names is `styles`.
    Style {
        targets: Vec<String>,
        styles: String,
    },
    /// `a:::warn` on a line of its own.
    Shorthand { target: String, classes: String },
}

impl RequirementStyleStatement {
    /// The boxes the statement is about.
    pub fn targets(&self) -> &[String] {
        match self {
            Self::ClassDef(_) => &[],
            Self::Class { targets, .. } | Self::Style { targets, .. } => targets,
            Self::Shorthand { target, .. } => std::slice::from_ref(target),
        }
    }

    fn targets_mut(&mut self) -> &mut [String] {
        match self {
            Self::ClassDef(_) => &mut [],
            Self::Class { targets, .. } | Self::Style { targets, .. } => targets,
            Self::Shorthand { target, .. } => std::slice::from_mut(target),
        }
    }
}

/// An editable requirement diagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequirementDocument {
    /// `None` when the source has no `direction` statement, which is Mermaid's
    /// `TB`.
    pub direction: Option<RequirementDirection>,
    pub requirements: Vec<Requirement>,
    pub elements: Vec<RequirementElement>,
    pub relationships: Vec<RequirementRelationship>,
    pub style_statements: Vec<RequirementStyleStatement>,
    pub preserved_lines: Vec<PreservedLine>,
}

impl RequirementDocument {
    /// A blank requirement diagram, used when the editor is opened on an empty fence.
    pub fn empty() -> Self {
        Self {
            direction: None,
            requirements: Vec::new(),
            elements: Vec::new(),
            relationships: Vec::new(),
            style_statements: Vec::new(),
            preserved_lines: Vec::new(),
        }
    }

    /// The names of every box, requirements first.
    pub fn names(&self) -> Vec<String> {
        self.requirements
            .iter()
            .map(|requirement| requirement.name.clone())
            .chain(self.elements.iter().map(|element| element.name.clone()))
            .collect()
    }

    pub fn has_name(&self, name: &str) -> bool {
        self.requirements
            .iter()
            .any(|requirement| requirement.name == name)
            || self.elements.iter().any(|element| element.name == name)
    }

    /// A name no box is using, based on `prefix`.
    pub fn unused_name(&self, prefix: &str) -> String {
        if !self.has_name(prefix) {
            return prefix.to_string();
        }
        for suffix in 1..usize::MAX {
            let candidate = format!("{prefix}{suffix}");
            if !self.has_name(&candidate) {
                return candidate;
            }
        }
        prefix.to_string()
    }

    /// How many relationships and styling statements name `name`, which is
    /// what deleting the box takes with it.
    pub fn references_to(&self, name: &str) -> usize {
        self.relationships
            .iter()
            .filter(|relationship| relationship.source == name || relationship.target == name)
            .count()
            + self
                .style_statements
                .iter()
                .filter(|statement| statement.targets().iter().any(|target| target == name))
                .count()
    }

    /// Points every relationship and styling statement that names an old
    /// spelling at the new one. All the renames apply at once, so swapping two
    /// names does not collapse them into one.
    pub fn rename(&mut self, renames: &[(String, String)]) {
        if renames.is_empty() {
            return;
        }
        let renamed = |name: &mut String| {
            if let Some((_, new)) = renames.iter().find(|(old, _)| old == name) {
                *name = new.clone();
            }
        };
        for relationship in &mut self.relationships {
            renamed(&mut relationship.source);
            renamed(&mut relationship.target);
        }
        for statement in &mut self.style_statements {
            for target in statement.targets_mut() {
                renamed(target);
            }
        }
    }

    /// Takes away everything that names `name`: a relationship left behind
    /// would point at nothing, which Mermaid cannot draw, and a styling
    /// statement loses the name from its list, or goes when it named nothing
    /// else.
    pub fn remove_references(&mut self, name: &str) {
        self.relationships
            .retain(|relationship| relationship.source != name && relationship.target != name);
        self.style_statements
            .retain_mut(|statement| match statement {
                RequirementStyleStatement::ClassDef(_) => true,
                RequirementStyleStatement::Class { targets, .. }
                | RequirementStyleStatement::Style { targets, .. } => {
                    targets.retain(|target| target != name);
                    !targets.is_empty()
                }
                RequirementStyleStatement::Shorthand { target, .. } => target != name,
            });
    }

    /// Why the document cannot be written back as a diagram Mermaid accepts, in
    /// Japanese, or `None` when it can. [`parse_requirement`] never produces a
    /// document that fails it.
    pub fn structure_error(&self) -> Option<String> {
        let mut seen: Vec<&str> = Vec::new();
        let boxes = self
            .requirements
            .iter()
            .map(|requirement| (requirement.name.as_str(), &requirement.classes))
            .chain(
                self.elements
                    .iter()
                    .map(|element| (element.name.as_str(), &element.classes)),
            );
        for (name, classes) in boxes {
            if !is_valid_requirement_name(name) {
                return Some(invalid_name_message(name));
            }
            if seen.contains(&name) {
                return Some(format!("名前 `{name}` が二重に使われています。"));
            }
            seen.push(name);
            for class in classes {
                if !is_valid_requirement_class_name(class) {
                    return Some(format!("`{class}` はクラス名として使えません。"));
                }
            }
        }

        for requirement in &self.requirements {
            for value in [&requirement.id, &requirement.text] {
                if !is_valid_requirement_value(&value.value) {
                    return Some(invalid_value_message(&requirement.name, &value.value));
                }
            }
        }
        for element in &self.elements {
            for value in [&element.element_type, &element.doc_ref] {
                if !is_valid_requirement_value(&value.value) {
                    return Some(invalid_value_message(&element.name, &value.value));
                }
            }
        }

        for relationship in &self.relationships {
            for name in [&relationship.source, &relationship.target] {
                if !seen.contains(&name.as_str()) {
                    return Some(format!(
                        "関係が指す `{name}` という要求・要素がありません。"
                    ));
                }
            }
        }

        for statement in &self.style_statements {
            if let RequirementStyleStatement::Class { targets, .. }
            | RequirementStyleStatement::Style { targets, .. } = statement
                && targets.is_empty()
            {
                return Some("対象のないスタイル指定があります。".to_string());
            }
            for name in statement.targets() {
                if !seen.contains(&name.as_str()) {
                    return Some(format!(
                        "スタイル指定が指す `{name}` という要求・要素がありません。"
                    ));
                }
            }
        }
        None
    }
}

/// Why `name` cannot name a box, in the words the form shows.
pub fn invalid_name_message(name: &str) -> String {
    if name.is_empty() {
        return "名前が空の要求・要素があります。".to_string();
    }
    format!(
        "「{name}」は名前に使えません (記号 \" - < > {{ }} : , # % ; と前後の空白、単語 direction は不可)。"
    )
}

/// Why `value` cannot be a field of the box named `owner`.
pub fn invalid_value_message(owner: &str, value: &str) -> String {
    format!(
        "`{owner}` の「{value}」は書けません (改行や前後の空白、`%%{{`、`\"` で始まる値、`\"` と `#` / `%%` の混在は不可)。"
    )
}

/// Whether `name` can name a requirement or an element.
///
/// A relationship is split at its first `-` and at `<-` / `->` wherever they
/// sit, quotes or not, so a name cannot hold a dash or an arrow head. A quoted
/// name cannot hold a quote, `:::` and `,` separate classes and names, `#` and
/// `%%` start comments, and a line holding the word `direction` followed by a
/// direction is taken for a `direction` statement whatever else it says.
pub fn is_valid_requirement_name(name: &str) -> bool {
    !name.is_empty()
        && name.trim() == name
        && !name.chars().any(|character| {
            matches!(
                character,
                '"' | '-' | '<' | '>' | '{' | '}' | ':' | ',' | '#' | '%' | ';'
            ) || character.is_control()
                || (character.is_whitespace() && character != ' ')
        })
        && !name
            .split(' ')
            .any(|word| word.eq_ignore_ascii_case("direction"))
}

/// Whether `value` can be written as a field of a block. A value holding a
/// quote cannot itself be quoted, so it can hold neither of the comment
/// markers the quotes would have protected, and it cannot start with the quote.
/// `merman` cuts `%%{ … }%%` out of the source before anything else, quotes or
/// not, so a value holding `%%{` would be drawn without it, or would take the
/// rest of the diagram with it when nothing closes it.
pub fn is_valid_requirement_value(value: &str) -> bool {
    value.trim() == value
        && !value.chars().any(char::is_control)
        && !value.contains("%%{")
        && (!value.contains('"')
            || (!value.starts_with('"') && !value.contains('#') && !value.contains("%%")))
}

/// Whether `name` can be written as a class after `:::`.
pub fn is_valid_requirement_class_name(name: &str) -> bool {
    !name.is_empty()
        && !name.chars().any(|character| {
            character.is_whitespace()
                || character.is_control()
                || matches!(character, ',' | '"' | '{' | '}' | ':' | '#' | '%' | ';')
        })
}

/// Whether `source` is a requirement diagram: its first statement is `requirementDiagram`.
///
/// The detector that runs before `merman`'s parser only looks for a
/// case-sensitive `requirement`, and the parser then takes the whole word in
/// any case, so `requirementdiagram` is a requirement diagram but
/// `RequirementDiagram` is not a diagram at all.
pub fn is_requirement_source(source: &str) -> bool {
    first_statement_line(source).is_some_and(|line| {
        line.starts_with(DETECTED_PREFIX) && keyword_tail_ignoring_case(line, HEADER).is_some()
    })
}

/// Reads `source` — the body of a ```` ```mermaid ```` fence — into an editable
/// document.
pub fn parse_requirement(source: &str) -> Result<RequirementDocument> {
    let lines = source.lines().collect::<Vec<_>>();
    let preamble = scan_preamble(&lines);
    let mut document = RequirementDocument {
        preserved_lines: preamble.preserved,
        ..RequirementDocument::empty()
    };
    // An empty fence: the editor opens on a blank diagram, and whatever
    // comments the user left keep their place.
    let Some(header_index) = preamble.header else {
        return Ok(document);
    };

    let header = lines.get(header_index).copied().unwrap_or_default();
    if !strip_inline_comment(header)
        .trim()
        .eq_ignore_ascii_case(HEADER)
    {
        bail!("要求図ではありません (`requirementDiagram` で始まる図だけを構造化編集できます)。");
    }
    let original = merman_model(source)?;
    reject_trailing_comment(header)?;

    let mut index = header_index + 1;
    let mut position = PreservedPosition::Top;
    while let Some(line) = lines.get(index).copied() {
        let start = index;
        index += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("%%{") && !trimmed.contains("}%%") {
            while lines.get(index).is_some_and(|line| !line.contains("}%%")) {
                index += 1;
            }
            if index < lines.len() {
                index += 1;
            }
            let text = join_lines(lines.get(start..index).unwrap_or_default());
            reject_statement_after_directive(&text)?;
            document
                .preserved_lines
                .push(PreservedLine { position, text });
            continue;
        }
        reject_statement_after_directive(line)?;

        let stripped = strip_inline_comment(line);
        let statement = stripped.trim();
        // A line that is nothing but a comment, `%%` or `#`, and the
        // accessibility statements, which the model does not carry.
        if statement.is_empty() || is_acc_title(statement) {
            document.preserved_lines.push(PreservedLine {
                position,
                text: line.trim_end().to_string(),
            });
            continue;
        }
        if let Some(opens_block) = acc_descr_opens_block(statement) {
            if opens_block {
                while lines.get(index).is_some_and(|line| !line.contains('}')) {
                    index += 1;
                }
                if index < lines.len() {
                    index += 1;
                }
            }
            document.preserved_lines.push(PreservedLine {
                position,
                text: join_lines(lines.get(start..index).unwrap_or_default()),
            });
            continue;
        }

        reject_trailing_comment(line)?;
        position = PreservedPosition::Bottom;

        if reads_as_direction(statement) {
            let words = statement.split_whitespace().collect::<Vec<_>>();
            let direction = match words.as_slice() {
                [keyword, direction] if keyword.eq_ignore_ascii_case("direction") => {
                    RequirementDirection::from_keyword(direction)
                }
                _ => None,
            };
            let Some(direction) = direction else {
                bail!(
                    "`{statement}` は `direction` の文として読まれるため、構造化編集できません。テキストのまま編集してください。"
                );
            };
            if document.direction.is_some() {
                bail!(
                    "`direction` が 2 回書かれています。1 つにまとめてからもう一度開いてください。"
                );
            }
            document.direction = Some(direction);
            continue;
        }

        if let Some((keyword, rest)) = block_opening(statement) {
            let (name, classes) = read_name_and_classes(rest)?;
            let body = read_block(&lines, &mut index, statement)?;
            if document.has_name(&name) {
                bail!(
                    "`{name}` が二重に定義されています。Mermaid は 2 つ目を無視するため、1 つにまとめてからもう一度開いてください。"
                );
            }
            match RequirementKind::from_keyword(keyword) {
                Some(kind) => document
                    .requirements
                    .push(requirement_from_block(kind, name, classes, body)?),
                None => document
                    .elements
                    .push(element_from_block(name, classes, body)?),
            }
            continue;
        }

        if let Some(style_statement) = read_style_statement(statement)? {
            for target in style_statement.targets() {
                if !document.has_name(target) {
                    bail!(
                        "`{statement}` は、その時点で定義されていない `{target}` を指しています。Mermaid はこの指定を無視するため、構造化編集で並べ替えると図が変わります。定義の後ろに移すか、テキストのまま編集してください。"
                    );
                }
            }
            document.style_statements.push(style_statement);
            continue;
        }

        if let Some(relationship) = read_relationship(statement)? {
            document.relationships.push(relationship);
            continue;
        }

        bail!("`{statement}` は要求図の文として読めません。テキストのまま編集してください。");
    }

    if let Some(problem) = document.structure_error() {
        bail!("{problem}テキストのまま編集してください。");
    }

    // The scan above follows `merman`'s parser; this is what proves it did,
    // and that moving the statements into the emitter's order changes nothing
    // Mermaid draws.
    let rewritten = merman_model(&emit_requirement(&document))?;
    if !same_drawing(&original, &rewritten)? {
        bail!(
            "書き直すと図の内容が変わるため開けません (スタイル指定の順序などに依存しています)。テキストのまま編集してください。"
        );
    }
    Ok(document)
}

fn merman_model(source: &str) -> Result<RequirementDiagramRenderModel> {
    let parsed = crate::parse_for_render_model(source)
        .map_err(|error| {
            anyhow!(
                "Mermaid の構文として読めませんでした ({error})。テキストのまま編集してください。"
            )
        })?
        .ok_or_else(|| anyhow!("Mermaid の図として解釈できませんでした。"))?;
    let RenderSemanticModel::Requirement(model) = parsed.into_parts().1 else {
        bail!("この図は要求図ではありません。");
    };
    Ok(model)
}

fn same_drawing(
    original: &RequirementDiagramRenderModel,
    rewritten: &RequirementDiagramRenderModel,
) -> Result<bool> {
    Ok(serde_json::to_value(original)? == serde_json::to_value(rewritten)?)
}

/// `merman` cuts every line that is not a styling statement at the first `#`
/// or `%%` outside quotes. Styling statements keep theirs, since `#` starts a
/// colour there.
fn strip_inline_comment(line: &str) -> String {
    let lowered = line.trim_start().to_ascii_lowercase();
    if lowered.starts_with("style")
        || lowered.starts_with("classdef")
        || lowered.starts_with("class ")
        || lowered == "class"
    {
        return line.to_string();
    }
    let mut in_quotes = false;
    let mut previous_percent = None;
    for (offset, character) in line.char_indices() {
        if character == '"' {
            in_quotes = !in_quotes;
            previous_percent = None;
            continue;
        }
        if !in_quotes {
            if character == '#' {
                return line.get(..offset).unwrap_or_default().to_string();
            }
            if character == '%'
                && let Some(start) = previous_percent
            {
                return line.get(..start).unwrap_or_default().to_string();
            }
        }
        previous_percent = (character == '%').then_some(offset);
    }
    line.to_string()
}

/// A comment after a statement is gone from `merman`'s model, so the emitter
/// could not write it back.
fn reject_trailing_comment(line: &str) -> Result<()> {
    if strip_inline_comment(line).trim() != line.trim() {
        bail!(
            "文の末尾のコメント (`#` または `%%` 以降) は構造化編集で失われます ({})。コメントを独立した行に移すか、`#` を含む値は `\"` で囲んでください。",
            line.trim()
        );
    }
    Ok(())
}

/// `merman` cuts every `%%{ … }%%` out of the source before it reads a line,
/// so a statement written after one on the same line is read as a statement,
/// while this scan would keep the whole line as a comment: out of the form's
/// reach, where a rename or a deletion could not follow it.
fn reject_statement_after_directive(text: &str) -> Result<()> {
    let text = text.trim_start();
    let mut rest = text;
    while let Some(inner) = rest.strip_prefix("%%{") {
        let Some(end) = inner.find("}%%") else {
            return Ok(());
        };
        rest = inner.get(end + 3..).unwrap_or_default().trim_start();
    }
    if rest.len() != text.len() && !strip_inline_comment(rest).trim().is_empty() {
        bail!(
            "`%%{{ … }}%%` の後ろに同じ行で書かれた文は構造化編集で扱えません ({})。文を次の行に移してからもう一度開いてください。",
            text.trim()
        );
    }
    Ok(())
}

fn is_acc_title(statement: &str) -> bool {
    statement
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("acctitle"))
        && statement
            .get(8..)
            .is_some_and(|rest| rest.trim_start().starts_with(':'))
}

/// `Some(true)` for an `accDescr {` that runs over the following lines,
/// `Some(false)` for one that ends on its own line, `None` for anything else.
fn acc_descr_opens_block(statement: &str) -> Option<bool> {
    if !statement
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("accdescr"))
    {
        return None;
    }
    let rest = statement.get(8..)?.trim_start();
    if rest.starts_with(':') {
        return Some(false);
    }
    let after = rest.strip_prefix('{')?;
    Some(!after.contains('}'))
}

/// Whether `merman` takes the line for a `direction` statement: it looks for
/// the first `direction` word anywhere on the line and at the word after it.
fn reads_as_direction(statement: &str) -> bool {
    let words = statement.split_whitespace().collect::<Vec<_>>();
    words
        .iter()
        .position(|word| word.eq_ignore_ascii_case("direction"))
        .and_then(|index| words.get(index + 1))
        .is_some_and(|next| RequirementDirection::from_keyword(next).is_some())
}

/// The keyword and the rest of a line that opens a requirement or element
/// block, without the brace.
fn block_opening(statement: &str) -> Option<(&str, &str)> {
    let without_brace = statement.strip_suffix('{')?.trim_end();
    let (keyword, rest) = without_brace
        .split_once(char::is_whitespace)
        .unwrap_or((without_brace, ""));
    (keyword.eq_ignore_ascii_case("element") || RequirementKind::from_keyword(keyword).is_some())
        .then_some((keyword, rest.trim()))
}

fn read_name_and_classes(text: &str) -> Result<(String, Vec<String>)> {
    let (name_text, classes) = match text.find(":::") {
        Some(offset) => (
            text.get(..offset).unwrap_or_default(),
            read_id_list(text.get(offset + 3..).unwrap_or_default())?,
        ),
        None => (text, Vec::new()),
    };
    let name = read_name(name_text)?;
    if name.is_empty() {
        bail!("名前のない要求・要素があります ({text})。");
    }
    Ok((name, classes))
}

/// A name, quoted or not. `merman` ignores whatever follows the closing quote,
/// which the emitter could not write back.
fn read_name(text: &str) -> Result<String> {
    let text = text.trim();
    if text.starts_with('"') {
        let Some((name, rest)) = read_quoted(text) else {
            bail!("閉じていない `\"` があります ({text})。");
        };
        if !rest.trim().is_empty() {
            bail!(
                "`\"{name}\"` の後ろの `{}` は Mermaid が無視するため、構造化編集できません。",
                rest.trim()
            );
        }
        return Ok(name);
    }
    Ok(text.to_string())
}

/// The text between a pair of quotes, which `merman` reads with no escapes,
/// and what follows the closing one.
fn read_quoted(text: &str) -> Option<(String, &str)> {
    let inner = text.strip_prefix('"')?;
    let end = inner.find('"')?;
    Some((inner.get(..end)?.to_string(), inner.get(end + 1..)?))
}

/// A comma separated list of classes, as `merman` reads what follows `:::`.
fn read_id_list(text: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut rest = text.trim_start();
    while !rest.is_empty() {
        let (item, after) = if rest.starts_with('"') {
            read_quoted(rest).ok_or_else(|| anyhow!("閉じていない `\"` があります ({text})。"))?
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            (
                rest.get(..end).unwrap_or_default().trim().to_string(),
                rest.get(end..).unwrap_or_default(),
            )
        };
        if !item.is_empty() {
            out.push(item);
        }
        rest = after.trim_start();
        match rest.strip_prefix(',') {
            Some(after_comma) => rest = after_comma,
            None => break,
        }
    }
    Ok(out)
}

struct BlockBody {
    fields: Vec<(String, RequirementValue)>,
    comments: Vec<String>,
}

/// Reads the lines of a block up to its closing brace, advancing `index` past
/// it.
fn read_block(lines: &[&str], index: &mut usize, opening: &str) -> Result<BlockBody> {
    let mut body = BlockBody {
        fields: Vec::new(),
        comments: Vec::new(),
    };
    while let Some(line) = lines.get(*index).copied() {
        *index += 1;
        reject_statement_after_directive(line)?;
        let stripped = strip_inline_comment(line);
        let statement = stripped.trim();
        if statement.is_empty() {
            if !line.trim().is_empty() {
                body.comments.push(line.trim().to_string());
            }
            continue;
        }
        reject_trailing_comment(line)?;
        if statement == "}" {
            return Ok(body);
        }
        let Some((key, value)) = statement.split_once(':') else {
            bail!("`{statement}` はブロックの項目として読めません (`{opening}`)。");
        };
        let key = key.trim().to_ascii_lowercase();
        if body.fields.iter().any(|(known, _)| *known == key) {
            bail!(
                "`{opening}` の `{key}` が 2 回書かれています。Mermaid は後の方だけを使うため、1 つにまとめてからもう一度開いてください。"
            );
        }
        body.fields.push((key, read_value(value)?));
    }
    bail!("`{opening}` のブロックが `}}` で閉じられていません。")
}

fn read_value(text: &str) -> Result<RequirementValue> {
    let text = text.trim();
    if text.starts_with('"') {
        let Some((value, rest)) = read_quoted(text) else {
            bail!("閉じていない `\"` があります ({text})。");
        };
        if !rest.trim().is_empty() {
            bail!(
                "`\"{value}\"` の後ろに余分な `{}` があります。",
                rest.trim()
            );
        }
        return Ok(RequirementValue::new(value.trim(), true));
    }
    Ok(RequirementValue::new(text, false))
}

fn requirement_from_block(
    kind: RequirementKind,
    name: String,
    classes: Vec<String>,
    body: BlockBody,
) -> Result<Requirement> {
    let mut requirement = Requirement {
        kind,
        name,
        id: RequirementValue::default(),
        text: RequirementValue::default(),
        risk: None,
        verify_method: None,
        classes,
        body_comments: body.comments,
    };
    for (key, value) in body.fields {
        match key.as_str() {
            "id" => requirement.id = value,
            "text" => requirement.text = value,
            "risk" => {
                let risk = RequirementRisk::from_keyword(&value.value)
                    .ok_or_else(|| anyhow!("`{}` はリスクとして読めません。", value.value))?;
                requirement.risk = Some(risk);
            }
            "verifymethod" => {
                let method = VerifyMethod::from_keyword(&value.value)
                    .ok_or_else(|| anyhow!("`{}` は検証方法として読めません。", value.value))?;
                requirement.verify_method = Some(method);
            }
            other => bail!("`{other}` は要求の項目として読めません。"),
        }
    }
    Ok(requirement)
}

fn element_from_block(
    name: String,
    classes: Vec<String>,
    body: BlockBody,
) -> Result<RequirementElement> {
    let mut element = RequirementElement {
        name,
        element_type: RequirementValue::default(),
        doc_ref: RequirementValue::default(),
        classes,
        body_comments: body.comments,
    };
    for (key, value) in body.fields {
        match key.as_str() {
            "type" => element.element_type = value,
            "docref" => element.doc_ref = value,
            other => bail!("`{other}` は要素の項目として読めません。"),
        }
    }
    Ok(element)
}

/// The styling statements, checked in `merman`'s order: a `:::` line, then
/// `style`, `classDef` and `class`, each recognised by its first letters alone.
fn read_style_statement(statement: &str) -> Result<Option<RequirementStyleStatement>> {
    if let Some(offset) = statement.find(":::")
        && !statement.ends_with('{')
    {
        let target = read_name(statement.get(..offset).unwrap_or_default())?;
        let classes = statement.get(offset + 3..).unwrap_or_default().trim();
        if target.is_empty() || classes.is_empty() {
            bail!("`{statement}` はクラスの指定として読めません。");
        }
        return Ok(Some(RequirementStyleStatement::Shorthand {
            target,
            classes: classes.to_string(),
        }));
    }
    let lowered = statement.to_ascii_lowercase();
    if lowered.starts_with("style") {
        let (targets, styles) = read_target_list(statement.get(5..).unwrap_or_default())?;
        return Ok(Some(RequirementStyleStatement::Style {
            targets,
            styles: styles.to_string(),
        }));
    }
    if lowered.starts_with("classdef") {
        return Ok(Some(RequirementStyleStatement::ClassDef(
            statement.to_string(),
        )));
    }
    if lowered.starts_with("class") {
        let (targets, classes) = read_target_list(statement.get(5..).unwrap_or_default())?;
        return Ok(Some(RequirementStyleStatement::Class {
            targets,
            classes: classes.to_string(),
        }));
    }
    Ok(None)
}

/// The names a `style` or `class` statement opens with — separated by commas,
/// each quoted or ending at whitespace — and what follows them.
fn read_target_list(text: &str) -> Result<(Vec<String>, &str)> {
    let mut targets = Vec::new();
    let mut rest = text.trim_start();
    while !rest.is_empty() {
        let (target, after) = if rest.starts_with('"') {
            read_quoted(rest).ok_or_else(|| anyhow!("閉じていない `\"` があります ({text})。"))?
        } else {
            let end = rest
                .find(|character: char| character == ',' || character.is_whitespace())
                .unwrap_or(rest.len());
            if end == 0 {
                bail!("スタイル指定の対象が読めません ({text})。");
            }
            (
                rest.get(..end).unwrap_or_default().to_string(),
                rest.get(end..).unwrap_or_default(),
            )
        };
        targets.push(target);
        rest = after.trim_start();
        match rest.strip_prefix(',') {
            Some(after_comma) => rest = after_comma,
            None => break,
        }
    }
    let rest = rest.trim();
    if targets.is_empty() || rest.is_empty() {
        bail!("スタイル指定として読めません ({text})。");
    }
    Ok((targets, rest))
}

/// A relationship, split the way `merman` splits it: at `<-` or `->`, and then
/// at the first `-`, quotes or not.
fn read_relationship(statement: &str) -> Result<Option<RequirementRelationship>> {
    let (source, kind, target) = if let Some(offset) = statement.find("<-") {
        let left = statement.get(..offset).unwrap_or_default();
        let rest = statement.get(offset + 2..).unwrap_or_default();
        let Some((kind, right)) = rest.split_once('-') else {
            bail!("`{statement}` は関係として読めません。");
        };
        (right, kind, left)
    } else if let Some(offset) = statement.find("->") {
        let left = statement.get(..offset).unwrap_or_default();
        let right = statement.get(offset + 2..).unwrap_or_default();
        let Some((source, kind)) = left.split_once('-') else {
            bail!("`{statement}` は関係として読めません。");
        };
        (source, kind, right)
    } else {
        return Ok(None);
    };
    let Some(kind) = RelationshipKind::from_keyword(kind.trim()) else {
        return Ok(None);
    };
    Ok(Some(RequirementRelationship {
        source: read_name(source)?,
        kind,
        target: read_name(target)?,
    }))
}

/// Writes the document back as Mermaid source, without a trailing newline.
pub fn emit_requirement(document: &RequirementDocument) -> String {
    let mut lines: Vec<String> = Vec::new();

    lines.extend(preserved_at(document, PreservedPosition::Preamble));
    lines.push(HEADER.to_string());
    lines.extend(preserved_at(document, PreservedPosition::Top));
    if let Some(direction) = document.direction {
        lines.push(format!("{INDENT}direction {}", direction.keyword()));
    }

    for requirement in &document.requirements {
        lines.push(block_opening_line(
            requirement.kind.keyword(),
            &requirement.name,
            &requirement.classes,
        ));
        push_field(&mut lines, "id", &requirement.id);
        push_field(&mut lines, "text", &requirement.text);
        if let Some(risk) = requirement.risk {
            lines.push(format!("{INDENT}{INDENT}risk: {}", risk.keyword()));
        }
        if let Some(method) = requirement.verify_method {
            lines.push(format!(
                "{INDENT}{INDENT}verifymethod: {}",
                method.keyword()
            ));
        }
        for comment in &requirement.body_comments {
            lines.push(format!("{INDENT}{INDENT}{comment}"));
        }
        lines.push(format!("{INDENT}}}"));
    }

    for element in &document.elements {
        lines.push(block_opening_line(
            "element",
            &element.name,
            &element.classes,
        ));
        push_field(&mut lines, "type", &element.element_type);
        push_field(&mut lines, "docref", &element.doc_ref);
        for comment in &element.body_comments {
            lines.push(format!("{INDENT}{INDENT}{comment}"));
        }
        lines.push(format!("{INDENT}}}"));
    }

    for relationship in &document.relationships {
        lines.push(format!(
            "{INDENT}{} - {} -> {}",
            name_token(&relationship.source),
            relationship.kind.keyword(),
            name_token(&relationship.target)
        ));
    }

    for statement in &document.style_statements {
        let text = match statement {
            RequirementStyleStatement::ClassDef(text) => text.clone(),
            RequirementStyleStatement::Class { targets, classes } => {
                format!("class {} {classes}", target_list(targets))
            }
            RequirementStyleStatement::Style { targets, styles } => {
                format!("style {} {styles}", target_list(targets))
            }
            RequirementStyleStatement::Shorthand { target, classes } => {
                format!("{}:::{classes}", name_token(target))
            }
        };
        lines.push(format!("{INDENT}{text}"));
    }

    lines.extend(preserved_at(document, PreservedPosition::Bottom));

    let mut out = lines.join("\n");
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

fn preserved_at(
    document: &RequirementDocument,
    position: PreservedPosition,
) -> impl Iterator<Item = String> + '_ {
    document
        .preserved_lines
        .iter()
        .filter(move |line| line.position == position)
        .map(|line| line.text.clone())
}

fn block_opening_line(keyword: &str, name: &str, classes: &[String]) -> String {
    let mut text = format!("{INDENT}{keyword} {}", name_token(name));
    if !classes.is_empty() {
        text.push_str(":::");
        text.push_str(&classes.join(","));
    }
    text.push_str(" {");
    text
}

fn push_field(lines: &mut Vec<String>, key: &str, value: &RequirementValue) {
    if value.value.is_empty() {
        return;
    }
    lines.push(format!("{INDENT}{INDENT}{key}: {}", value_token(value)));
}

/// A value is quoted when the source quoted it or when a comment marker in it
/// would otherwise cut it short — and never when it holds a quote itself,
/// which [`is_valid_requirement_value`] keeps apart from the comment markers.
fn value_token(value: &RequirementValue) -> String {
    let text = value.value.as_str();
    let needs_quotes = text.contains('#') || text.contains("%%");
    if !text.contains('"') && (value.quoted || needs_quotes) {
        format!("\"{text}\"")
    } else {
        text.to_string()
    }
}

/// A name is quoted when it holds anything but ASCII letters, digits and `_`
/// — Mermaid's lexer refuses a bare Japanese name, and `style` and `class`
/// lists end a bare name at a space — or when it starts like `style` or
/// `class`, which would turn a relationship written from it into a styling
/// statement.
fn name_token(name: &str) -> String {
    let lowered = name.to_ascii_lowercase();
    if !name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '_')
        || lowered.starts_with("style")
        || lowered.starts_with("class")
    {
        format!("\"{name}\"")
    } else {
        name.to_string()
    }
}

fn target_list(targets: &[String]) -> String {
    targets
        .iter()
        .map(|target| name_token(target))
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::{MermaidTheme, render_to_svg};

    const TEMPLATE: &str = "requirementDiagram
    %% requirement は要求、element は満たす側。- satisfies -> で結ぶ
    requirement login {
        id: 1
        text: \"ログインできること\"
        risk: high
        verifymethod: test
    }
    element login_page {
        type: \"画面\"
    }
    login_page - satisfies -> login";

    /// The emitted source must parse to the same document and emit the same
    /// text, otherwise confirming the editor twice would keep rewriting the
    /// fence.
    fn assert_stable(source: &str) -> String {
        let first = parse_requirement(source)
            .unwrap_or_else(|error| panic!("{source}\nshould parse: {error}"));
        let first_text = emit_requirement(&first);
        let second = parse_requirement(&first_text)
            .unwrap_or_else(|error| panic!("emitted source should parse: {error}\n{first_text}"));
        let second_text = emit_requirement(&second);
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

    fn assert_refused(source: &str, reason: &str) {
        let error = parse_requirement(source)
            .err()
            .unwrap_or_else(|| panic!("`{source}` should be refused"));
        let message = format!("{error}");
        assert!(
            message.contains(reason),
            "the reason for\n{source}\nshould mention `{reason}`: {message}"
        );
    }

    fn requirement(name: &str) -> Requirement {
        Requirement {
            kind: RequirementKind::Requirement,
            name: name.to_string(),
            id: RequirementValue::new("1", false),
            text: RequirementValue::new("本文", true),
            risk: Some(RequirementRisk::Low),
            verify_method: Some(VerifyMethod::Test),
            classes: Vec::new(),
            body_comments: Vec::new(),
        }
    }

    fn element(name: &str) -> RequirementElement {
        RequirementElement {
            name: name.to_string(),
            element_type: RequirementValue::new("画面", true),
            doc_ref: RequirementValue::default(),
            classes: Vec::new(),
            body_comments: Vec::new(),
        }
    }

    #[test]
    fn the_keyword_is_recognized_the_way_merman_recognizes_it() {
        for source in [
            "requirementDiagram\n    requirement r {\n    }",
            "requirementdiagram\n",
            "---\ntitle: t\n---\nrequirementDiagram",
        ] {
            assert!(
                is_requirement_source(source),
                "{source} should be a requirement diagram"
            );
        }
        for source in [
            "requirement r {\n}",
            "requirementDiagrams",
            "RequirementDiagram",
            "",
            "%% requirement",
            "flowchart TD\n    A --> B",
        ] {
            assert!(
                !is_requirement_source(source),
                "{source} should not be a requirement diagram"
            );
        }
    }

    #[test]
    fn the_insert_template_round_trips_byte_for_byte() {
        let document = parse_requirement(TEMPLATE).expect("the template should parse");
        assert_eq!(document.requirements.len(), 1);
        assert_eq!(document.elements.len(), 1);
        let login = &document.requirements[0];
        assert_eq!(login.name, "login");
        assert_eq!(login.id, RequirementValue::new("1", false));
        assert_eq!(
            login.text,
            RequirementValue::new("ログインできること", true)
        );
        assert_eq!(login.risk, Some(RequirementRisk::High));
        assert_eq!(login.verify_method, Some(VerifyMethod::Test));
        assert_eq!(
            document.relationships,
            vec![RequirementRelationship {
                source: "login_page".to_string(),
                kind: RelationshipKind::Satisfies,
                target: "login".to_string(),
            }]
        );
        assert_eq!(emit_requirement(&document), TEMPLATE);
        assert_renders(TEMPLATE);
    }

    #[test]
    fn examples_round_trip_and_render() {
        for source in [
            TEMPLATE,
            "requirementDiagram\n\nrequirement test_req {\n  id: test_id\n  text: the test text.\n  risk: High\n  verifyMethod: Analysis\n}\n",
            "requirementDiagram\n    direction LR\n    functionalRequirement 機能 {\n        id: 1.1\n    }\n    designConstraint 制約 {\n    }\n    element 部品 {\n        type: 部品\n        docref: docs/a.md\n    }\n    部品 - verifies -> 機能\n    機能 <- derives - 制約",
            "requirementDiagram\n    requirement \"名前 に 空白\" {\n        text: 値\n    }\n    element styled_box {\n    }\n    element classroom {\n    }\n    \"styled_box\" - traces -> \"名前 に 空白\"\n    \"classroom\" - refines -> \"styled_box\"\n    style \"名前 に 空白\",styled_box fill:#f9f,stroke:#333\n    classDef warn fill:#f96\n    class classroom warn\n    styled_box:::warn",
            "requirementDiagram\n    requirement a {\n        text: \"issue #12 を直す\"\n        id: \"100%% 達成\"\n    }\n    element b {\n        type: 引用 \"あり\" の値\n    }\n    b - satisfies -> a\n    a - contains -> a",
            "---\ntitle: 図\n---\n%%{init: {\"theme\": \"dark\"}}%%\nrequirementDiagram\n    accTitle: 概要\n    %% 先頭のメモ\n    requirement a {\n        %% ブロック内のメモ\n        id: 1\n    }\n    # 井桁のコメント\n    accDescr {\n        複数行の\n        説明\n    }",
            "requirementDiagram",
        ] {
            let emitted = assert_stable(source);
            assert_renders(&emitted);
        }
    }

    #[test]
    fn the_emitted_source_draws_what_the_original_drew() {
        // `parse_requirement` refuses a fence whose rewrite `merman` reads
        // differently; checked here directly as well, against the reordering
        // the emitter does.
        let source = "requirementDiagram\nelement e {\n}\nrequirement r:::c {\n}\nclassDef c fill:#f96\ne - satisfies -> r\nclass e c\nstyle r stroke:#333\n";
        let document = parse_requirement(source).expect("source should parse");
        let emitted = emit_requirement(&document);
        let original = merman_model(source).expect("the original should parse");
        let rewritten = merman_model(&emitted).expect("the rewrite should parse");
        assert_eq!(
            serde_json::to_value(&original).ok(),
            serde_json::to_value(&rewritten).ok(),
            "{emitted}"
        );
        assert_eq!(
            emitted,
            "requirementDiagram\n    requirement r:::c {\n    }\n    element e {\n    }\n    e - satisfies -> r\n    classDef c fill:#f96\n    class e c\n    style r stroke:#333"
        );
    }

    #[test]
    fn preserved_lines_keep_their_side_of_the_first_statement() {
        let source = "%% 前置き\nrequirementDiagram\n  accTitle: 題\n  %% 先頭\n  requirement a {\n    %% 中\n  }\n  %% 途中\n  element b {\n  }\n  %% 末尾";
        let document = parse_requirement(source).expect("source should parse");
        let positions = document
            .preserved_lines
            .iter()
            .map(|line| (line.position, line.text.trim().to_string()))
            .collect::<Vec<_>>();
        assert_eq!(
            positions,
            vec![
                (PreservedPosition::Preamble, "%% 前置き".to_string()),
                (PreservedPosition::Top, "accTitle: 題".to_string()),
                (PreservedPosition::Top, "%% 先頭".to_string()),
                (PreservedPosition::Bottom, "%% 途中".to_string()),
                (PreservedPosition::Bottom, "%% 末尾".to_string()),
            ]
        );
        assert_eq!(document.requirements[0].body_comments, vec!["%% 中"]);
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "%% 前置き\nrequirementDiagram\n  accTitle: 題\n  %% 先頭\n    requirement a {\n        %% 中\n    }\n    element b {\n    }\n  %% 途中\n  %% 末尾"
        );
    }

    #[test]
    fn a_rename_follows_relationships_and_styles() {
        let source = "requirementDiagram\n    requirement a {\n    }\n    element b {\n    }\n    b - satisfies -> a\n    a - traces -> a\n    style a,b fill:#f96\n    class a warn\n    a:::warn\n    classDef a fill:#000";
        let mut document = parse_requirement(source).expect("source should parse");
        document.requirements[0].name = "b".to_string();
        document.elements[0].name = "a".to_string();
        document.rename(&[
            ("a".to_string(), "b".to_string()),
            ("b".to_string(), "a".to_string()),
        ]);
        assert_eq!(document.structure_error(), None);
        let emitted = emit_requirement(&document);
        assert_eq!(
            emitted,
            "requirementDiagram\n    requirement b {\n    }\n    element a {\n    }\n    a - satisfies -> b\n    b - traces -> b\n    style b,a fill:#f96\n    class b warn\n    b:::warn\n    classDef a fill:#000",
            "the two names should swap everywhere but in the classDef, which names a class"
        );

        let mut document = parse_requirement(source).expect("source should parse");
        document.requirements[0].name = "新しい 名前".to_string();
        document.rename(&[("a".to_string(), "新しい 名前".to_string())]);
        let emitted = emit_requirement(&document);
        assert!(
            emitted.contains("b - satisfies -> \"新しい 名前\"")
                && emitted.contains("style \"新しい 名前\",b fill:#f96")
                && emitted.contains("class \"新しい 名前\" warn"),
            "{emitted}"
        );
        assert_renders(&emitted);
    }

    #[test]
    fn removing_a_box_takes_what_names_it_along() {
        let source = "requirementDiagram\n    requirement a {\n    }\n    element b {\n    }\n    element c {\n    }\n    b - satisfies -> a\n    c - traces -> b\n    classDef warn fill:#000\n    style a,b fill:#f96\n    class a warn\n    a:::warn";
        let mut document = parse_requirement(source).expect("source should parse");
        assert_eq!(document.references_to("a"), 4);
        assert_eq!(document.references_to("c"), 1);
        document.requirements.remove(0);
        assert!(document.structure_error().is_some());
        document.remove_references("a");
        assert_eq!(document.structure_error(), None);
        assert_eq!(
            emit_requirement(&document),
            "requirementDiagram\n    element b {\n    }\n    element c {\n    }\n    c - traces -> b\n    classDef warn fill:#000\n    style b fill:#f96"
        );
    }

    /// `merman` reads a bare Japanese name; Mermaid's lexer does not, so a
    /// name that is not ASCII letters, digits and `_` is written quoted.
    #[test]
    fn a_name_mermaid_does_not_read_bare_is_quoted() {
        let emitted = assert_stable(
            "requirementDiagram\n    requirement 要求 {\n    }\n    element login_page {\n    }\n    login_page - satisfies -> 要求",
        );
        assert_eq!(
            emitted,
            "requirementDiagram\n    requirement \"要求\" {\n    }\n    element login_page {\n    }\n    login_page - satisfies -> \"要求\""
        );
    }

    #[test]
    fn what_would_be_drawn_differently_is_refused() {
        // A trailing comment is gone from merman's model, and `#` starts one
        // even inside a value unless the value is quoted.
        assert_refused(
            "requirementDiagram\n    requirement a {\n        text: issue #12\n    }",
            "コメント",
        );
        assert_refused(
            "requirementDiagram\n    requirement a { %% メモ\n    }",
            "コメント",
        );
        assert_refused(
            "requirementDiagram %% メモ\n    element a {\n    }",
            "コメント",
        );
        // Mermaid keeps the first of two blocks with one name and the last of
        // two keys in a block; the form could show neither choice.
        assert_refused(
            "requirementDiagram\n    requirement a {\n    }\n    requirement a {\n        id: 2\n    }",
            "二重",
        );
        assert_refused(
            "requirementDiagram\n    requirement a {\n    }\n    element a {\n    }",
            "二重",
        );
        assert_refused(
            "requirementDiagram\n    requirement a {\n        id: 1\n        id: 2\n    }",
            "2 回",
        );
        assert_refused(
            "requirementDiagram\n    direction LR\n    direction TB\n    element a {\n    }",
            "2 回",
        );
        // Styled before the box exists: Mermaid ignores it, the emitter would
        // move it after the box.
        assert_refused(
            "requirementDiagram\n    style a fill:#f96\n    element a {\n    }",
            "定義されていない",
        );
        // Mermaid cannot draw a relationship to nothing.
        assert_refused(
            "requirementDiagram\n    element a {\n    }\n    a - satisfies -> b",
            "ありません",
        );
        // A dash in a name cannot be written into a relationship.
        assert_refused(
            "requirementDiagram\n    element a-b {\n    }",
            "名前に使えません",
        );
        // `merman` 0.7 dropped the word after a quoted name, which the model
        // refused for what it would have lost; 0.8 refuses the line itself, as
        // Mermaid does.
        assert_refused(
            "requirementDiagram\n    element \"a\" b {\n    }",
            "Mermaid の構文",
        );
        // Two classes applied at a block's definition, defined in the other
        // order before it: moving the block ahead of both changes which style
        // wins.
        assert_refused(
            "requirementDiagram\n    classDef c2 fill:#00f\n    classDef c1 fill:#f00\n    element a:::c1,c2 {\n    }",
            "変わる",
        );
        for source in [
            "flowchart TD\n    A --> B",
            "requirementDiagram\n    requirement a {\n",
        ] {
            assert!(parse_requirement(source).is_err(), "{source}");
        }
    }

    /// `merman` cuts a `%%{ … }%%` out of its line and reads what follows it,
    /// which a preserved comment line would have carried out of the form's
    /// reach — a rename would leave it naming the old box.
    #[test]
    fn a_statement_after_a_directive_on_its_line_is_refused() {
        for source in [
            "requirementDiagram\n    element a {\n    }\n    element b {\n    }\n    %%{init: {\"theme\":\"dark\"}}%% a - satisfies -> b",
            "requirementDiagram\n    element a {\n    }\n    %%{init: {}}%%   style a fill:#f96",
            "requirementDiagram\n    element a {\n    }\n    element b {\n    }\n    %%{init:\n {\"theme\":\"dark\"}}%% a - satisfies -> b",
            "requirementDiagram\n    element a {\n    }\n    %%{init: {}}%% %%{wrap}%% a:::warn",
            "requirementDiagram\n    requirement a {\n        %%{init: {}}%% id: 1\n    }",
        ] {
            assert_refused(source, "%%{ … }%%");
        }
        for source in [
            "requirementDiagram\n    %%{init: {\"theme\":\"dark\"}}%%\n    element a {\n    }",
            "requirementDiagram\n    element a {\n    }\n    %%{init: {}}%% %% メモ",
            "requirementDiagram\n    element a {\n        %%{init: {}}%%\n    }",
        ] {
            let emitted = assert_stable(source);
            assert_renders(&emitted);
        }
    }

    #[test]
    fn names_values_and_classes_are_validated() {
        for name in [
            "login",
            "ログイン画面",
            "名前 に 空白",
            "a.b_c(1)",
            "**太字**",
        ] {
            assert!(is_valid_requirement_name(name), "{name}");
        }
        for name in [
            "",
            " a",
            "a ",
            "a-b",
            "a<b",
            "a:b",
            "a,b",
            "a#b",
            "a%b",
            "a\"b",
            "a{",
            "a\tb",
            "a\u{3000}b",
            "x direction LR",
        ] {
            assert!(!is_valid_requirement_name(name), "{name}");
        }
        for value in [
            "",
            "1.2",
            "issue #12",
            "100%% 達成",
            "引用 \"あり\"",
            "a: b",
        ] {
            assert!(is_valid_requirement_value(value), "{value}");
        }
        for value in [
            " a",
            "a\nb",
            "\"a",
            "引用 \"あり\" #1",
            "\"a\" %% b",
            "x %%{init: {}}%% y",
            "100%%{",
        ] {
            assert!(!is_valid_requirement_value(value), "{value}");
        }
        assert!(is_valid_requirement_class_name("warn"));
        assert!(!is_valid_requirement_class_name("a b"));
        assert!(!is_valid_requirement_class_name("a,b"));
        assert!(!is_valid_requirement_class_name(""));
    }

    /// Every name and value the validators accept has to come back from
    /// `merman` exactly as it was written.
    #[test]
    fn accepted_names_and_values_survive_merman() {
        let names = [
            "login",
            "ログイン画面",
            "名前 に 空白",
            "styleguide",
            "Class A",
            "a.b_c(1)",
            "requirement",
            "element",
            "directions",
        ];
        let values = [
            "1.2",
            "issue #12",
            "100%% 達成",
            "引用 \"あり\"",
            "a: b",
            "}",
            "ログイン {できる}",
        ];
        for name in names {
            for value in values {
                let mut first = requirement(name);
                first.text = RequirementValue::new(value, false);
                let mut second = element("相手");
                second.doc_ref = RequirementValue::new(value, true);
                let document = RequirementDocument {
                    direction: None,
                    requirements: vec![first],
                    elements: vec![second],
                    relationships: vec![RequirementRelationship {
                        source: "相手".to_string(),
                        kind: RelationshipKind::Satisfies,
                        target: name.to_string(),
                    }],
                    style_statements: vec![
                        RequirementStyleStatement::ClassDef("classDef warn fill:#f96".to_string()),
                        RequirementStyleStatement::Style {
                            targets: vec![name.to_string(), "相手".to_string()],
                            styles: "stroke:#333".to_string(),
                        },
                        RequirementStyleStatement::Shorthand {
                            target: name.to_string(),
                            classes: "warn".to_string(),
                        },
                    ],
                    preserved_lines: Vec::new(),
                };
                assert_eq!(document.structure_error(), None, "{name} / {value}");
                let emitted = emit_requirement(&document);
                let parsed = parse_requirement(&emitted)
                    .unwrap_or_else(|error| panic!("{error}\n{emitted}"));
                assert_eq!(parsed.requirements[0].name, name, "name through\n{emitted}");
                assert_eq!(
                    parsed.requirements[0].text.value, value,
                    "value through\n{emitted}"
                );
                assert_eq!(parsed.elements[0].doc_ref.value, value, "{emitted}");
                assert_eq!(parsed.relationships, document.relationships, "{emitted}");
                assert_eq!(parsed.style_statements, document.style_statements);

                // `merman`'s own reading, which is what gets drawn.
                let model = merman_model(&emitted).expect("merman should read it");
                let drawn = model
                    .requirements
                    .first()
                    .map(|requirement| (requirement.name.as_str(), requirement.text.as_str()));
                assert_eq!(drawn, Some((name, value)), "merman's reading of\n{emitted}");
                assert_eq!(
                    model
                        .elements
                        .first()
                        .map(|element| element.doc_ref.as_str()),
                    Some(value)
                );
                assert_eq!(
                    model
                        .relationships
                        .first()
                        .map(|relationship| relationship.dst.as_str()),
                    Some(name)
                );
                assert_eq!(
                    model.requirements[0].css_styles,
                    vec!["stroke:#333".to_string(), "fill:#f96".to_string()],
                    "the style and the class should both reach `{name}`:\n{emitted}"
                );
                assert_renders(&emitted);
            }
        }
    }

    #[test]
    fn an_empty_fence_opens_as_a_blank_diagram() {
        let document = parse_requirement("").expect("an empty fence should parse");
        assert_eq!(document, RequirementDocument::empty());
        assert_eq!(emit_requirement(&document), "requirementDiagram");
        let document = parse_requirement("requirementdiagram").expect("the header alone");
        assert_eq!(document, RequirementDocument::empty());
        assert_renders("requirementDiagram");
    }

    #[test]
    fn crlf_and_trailing_whitespace_are_tolerated() {
        let source = "requirementDiagram  \r\n  element a {  \r\n    type: t  \r\n  }\r\n";
        let document = parse_requirement(source).expect("source should parse");
        assert_eq!(
            emit_requirement(&document),
            "requirementDiagram\n    element a {\n        type: t\n    }"
        );
    }

    /// Every requirement diagram in `merman`'s own fixtures either opens — and
    /// then survives a round trip that `merman` reads back identically and
    /// draws — or is refused with a reason in Japanese.
    #[test]
    fn merman_fixtures_round_trip_or_are_refused_with_a_reason() {
        let Some(directory) = crate::merman_fixture_directory("requirement") else {
            eprintln!("skipped: merman's fixtures were not found in the cargo git checkouts");
            return;
        };
        let mut paths = std::fs::read_dir(&directory)
            .expect("the fixture directory should be readable")
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "mmd"))
            .collect::<Vec<_>>();
        paths.sort();
        assert!(paths.len() >= 40, "only {} fixtures found", paths.len());

        let mut opened = 0;
        let mut refused = Vec::new();
        for path in &paths {
            let source = std::fs::read_to_string(path).expect("fixture should be readable");
            assert!(is_requirement_source(&source), "{}", path.display());
            match parse_requirement(&source) {
                Ok(_) => {
                    let emitted = assert_stable(&source);
                    let original = merman_model(&source).expect("the fixture should parse");
                    let rewritten = merman_model(&emitted).expect("the rewrite should parse");
                    assert_eq!(
                        serde_json::to_value(&original).ok(),
                        serde_json::to_value(&rewritten).ok(),
                        "{}",
                        path.display()
                    );
                    assert_renders(&emitted);
                    opened += 1;
                }
                Err(error) => {
                    let message = format!("{error}");
                    assert!(
                        message.chars().any(|character| character > '\u{7f}'),
                        "the reason for {} should be Japanese: {message}",
                        path.display()
                    );
                    refused.push(format!(
                        "{}: {message}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    ));
                }
            }
        }
        eprintln!("opened {opened}, refused {}:", refused.len());
        for line in &refused {
            eprintln!("  {line}");
        }
        assert!(
            opened * 4 >= (opened + refused.len()) * 3,
            "too many fixtures are refused:\n{}",
            refused.join("\n")
        );
    }
}
