//! A round-trippable editing model for Mermaid Venn diagrams.
//!
//! A Venn diagram is a list of areas — a `set` names one circle, a `union`
//! names the overlap of two or more — each with an optional label and size,
//! plus `text` statements that write a note into an area and `style`
//! statements that colour one. The model keeps the areas in source order,
//! because `merman` only accepts a `union` of sets declared above it.
//!
//! The notes and styles are not edited field by field, but they name sets, so
//! they are held with their target lists parsed out and the rest of the
//! statement verbatim: renaming or deleting a set reaches them too.
//!
//! Two layouts of `text` exist. One that follows a `set` or `union` on a deeper
//! indented line belongs to that area ([`VennArea::notes`]); one that names its
//! sets itself (`text A,B note`) is [`VennDocument::texts`]. `merman` reads
//! every indented `text` after an area as the first kind, so the second kind is
//! written back at the start of its line.
//!
//! A document is opened only when writing it back gives `merman` the same
//! diagram the fence gave it, so a statement this module reads differently
//! from `merman` is refused rather than rewritten.

use anyhow::{Result, anyhow, bail};
use merman::diagram::RenderSemanticModel;

use crate::flowchart_model::{PreservedLine, PreservedPosition};
use crate::mermaid_model::{
    INDENT, first_statement_line, join_lines, keyword_tail, keyword_tail_ignoring_case,
    scan_preamble,
};
use crate::sankey_model::refuse_statement_after_directive;

const HEADER: &str = "venn-beta";

/// One circle (`set`) or one overlap (`union`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VennArea {
    /// The set ids the area covers, as written. One id is a `set`, two or more
    /// a `union`.
    pub sets: Vec<String>,
    /// The text drawn in the area. `None` when the statement has no label, or
    /// an empty one, which Mermaid draws the same.
    pub label: Option<String>,
    /// The size as the source spells it, e.g. `20` or `5.3`. `None` leaves the
    /// size to Mermaid.
    pub size: Option<String>,
    /// The indented `text` statements under the area, each the verbatim rest
    /// of its line after `text`.
    pub notes: Vec<String>,
}

impl VennArea {
    pub fn is_set(&self) -> bool {
        self.sets.len() == 1
    }
}

/// A `text` or `style` statement: the sets it names and the rest of it as
/// written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VennTargeted {
    pub targets: Vec<String>,
    pub rest: String,
}

/// An editable Venn diagram.
#[derive(Debug, Clone, PartialEq)]
pub struct VennDocument {
    /// The title, without the `title` keyword. `Some("")` is a `title`
    /// statement with nothing after it.
    pub title: Option<String>,
    pub areas: Vec<VennArea>,
    /// `text` statements that name their own sets.
    pub texts: Vec<VennTargeted>,
    pub styles: Vec<VennTargeted>,
    pub preserved_lines: Vec<PreservedLine>,
}

impl VennDocument {
    /// A blank Venn diagram, used when the editor is opened on an empty fence.
    pub fn empty() -> Self {
        Self {
            title: None,
            areas: Vec::new(),
            texts: Vec::new(),
            styles: Vec::new(),
            preserved_lines: Vec::new(),
        }
    }

    /// The ids the `set` statements declare, in order.
    pub fn set_ids(&self) -> Vec<&str> {
        self.areas
            .iter()
            .filter(|area| area.is_set())
            .filter_map(|area| area.sets.first().map(String::as_str))
            .collect()
    }

    /// Renames sets everywhere they are named, all at once, so that swapping
    /// two names does not turn both into one.
    pub fn rename_sets(&mut self, renames: &[(String, String)]) {
        if renames.is_empty() {
            return;
        }
        let rename = |id: &mut String| {
            if let Some((_, new)) = renames.iter().find(|(old, _)| old == id) {
                *id = new.clone();
            }
        };
        for area in &mut self.areas {
            area.sets.iter_mut().for_each(rename);
        }
        for statement in self.texts.iter_mut().chain(&mut self.styles) {
            statement.targets.iter_mut().for_each(rename);
        }
    }

    /// The areas that go when the set at `index` is deleted: the set and every
    /// overlap it is part of, in ascending order. Empty when `index` is not a
    /// set.
    pub fn areas_removed_with(&self, index: usize) -> Vec<usize> {
        let Some(id) = self
            .areas
            .get(index)
            .filter(|area| area.is_set())
            .and_then(|area| area.sets.first())
        else {
            return Vec::new();
        };
        self.areas
            .iter()
            .enumerate()
            .filter(|(position, area)| {
                *position == index || (!area.is_set() && area.sets.contains(id))
            })
            .map(|(position, _)| position)
            .collect()
    }

    /// Deletes the set at `index` with the overlaps, notes and styles that name
    /// it, which would otherwise point at a circle that is no longer drawn —
    /// and an overlap of a missing set is one `merman` refuses outright.
    /// Returns the indices of the areas removed, in ascending order.
    pub fn remove_set(&mut self, index: usize) -> Vec<usize> {
        let removed = self.areas_removed_with(index);
        let Some(id) = self
            .areas
            .get(index)
            .and_then(|area| area.sets.first())
            .cloned()
        else {
            return removed;
        };
        for position in removed.iter().rev() {
            if *position < self.areas.len() {
                self.areas.remove(*position);
            }
        }
        self.texts
            .retain(|statement| !statement.targets.contains(&id));
        self.styles
            .retain(|statement| !statement.targets.contains(&id));
        removed
    }

    /// Why the document cannot be written back as a diagram Mermaid draws, in
    /// Japanese, or `None` when it can.
    pub fn structure_error(&self) -> Option<String> {
        if let Some(title) = &self.title
            && !is_valid_venn_title(title)
        {
            return Some(
                i18n::t!(
                    "The title cannot contain `%%` or line breaks (nor spaces at either end)."
                )
                .to_string(),
            );
        }
        let mut declared: Vec<&str> = Vec::new();
        let mut unions: Vec<Vec<&str>> = Vec::new();
        for area in &self.areas {
            for id in &area.sets {
                if !is_valid_venn_id(id) {
                    return Some(i18n::t!(
                        "The set ID “{id}” cannot be used (it cannot be empty, contain `\"` or line breaks, or have spaces at either end)."
                    ));
                }
            }
            let name = area.sets.join(",");
            if let Some(label) = &area.label
                && !is_valid_venn_label(label)
            {
                return Some(i18n::t!(
                    "The label “{label}” of “{name}” cannot be used (`\"`, line breaks and spaces at either end are not allowed)."
                ));
            }
            if let Some(size) = &area.size
                && !is_valid_venn_size(size)
            {
                return Some(i18n::t!(
                    "The size “{size}” of “{name}” cannot be used (only a number of 0 or more)."
                ));
            }
            match area.sets.as_slice() {
                [] => {
                    return Some(i18n::t!("There is an overlap that contains no set.").to_string());
                }
                [id] => {
                    if declared.contains(&id.as_str()) {
                        return Some(i18n::t!("The set “{id}” is written twice."));
                    }
                    declared.push(id);
                }
                members => {
                    let mut sorted = members.iter().map(String::as_str).collect::<Vec<_>>();
                    sorted.sort_unstable();
                    if sorted.windows(2).any(|pair| pair.first() == pair.get(1)) {
                        return Some(i18n::t!(
                            "The overlap “{name}” contains the same set twice."
                        ));
                    }
                    if let Some(missing) = members
                        .iter()
                        .find(|member| !declared.contains(&member.as_str()))
                    {
                        return Some(if self.set_ids().contains(&missing.as_str()) {
                            i18n::t!(
                                "The overlap “{name}” is written before the set “{missing}”. Place an overlap after the sets it uses."
                            )
                        } else {
                            i18n::t!("The set “{missing}” of the overlap “{name}” does not exist.")
                        });
                    }
                    if unions.contains(&sorted) {
                        return Some(i18n::t!("The overlap “{name}” is written twice."));
                    }
                    unions.push(sorted);
                }
            }
        }
        None
    }
}

/// Whether `source` is a Venn diagram: its first statement is `venn-beta`.
pub fn is_venn_source(source: &str) -> bool {
    first_statement_line(source).is_some_and(|line| keyword_tail(line, HEADER).is_some())
}

/// Reads `source` — the body of a ```` ```mermaid ```` fence — into an editable
/// document.
pub fn parse_venn(source: &str) -> Result<VennDocument> {
    let lines = source.lines().collect::<Vec<_>>();
    let preamble = scan_preamble(&lines);
    let mut document = VennDocument {
        title: None,
        areas: Vec::new(),
        texts: Vec::new(),
        styles: Vec::new(),
        preserved_lines: preamble.preserved,
    };
    let Some(header_index) = preamble.header else {
        return Ok(document);
    };

    let original = merman_venn(source)?;

    let header = lines.get(header_index).copied().unwrap_or_default().trim();
    reject_trailing_comment(header)?;
    if header != HEADER {
        bail!(i18n::t!(
            "The structured editor cannot handle `{header}` (write only `venn-beta` on its line, and the statements from the next line on)."
        ));
    }

    let mut index = header_index + 1;
    let mut position = PreservedPosition::Top;
    let mut indent_mode = false;
    while let Some(line) = lines.get(index).copied() {
        let trimmed = line.trim();
        let start = index;
        index += 1;
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
            refuse_statement_after_directive(&text)?;
            document
                .preserved_lines
                .push(PreservedLine { position, text });
            continue;
        }
        reject_trailing_comment(trimmed)?;

        let indented = line.starts_with([' ', '\t']);
        if indented
            && indent_mode
            && let Some(rest) = keyword_tail_ignoring_case(trimmed, "text")
        {
            let Some(area) = document.areas.last_mut() else {
                bail!(i18n::t!(
                    "The `text` line is not under any set ({trimmed})."
                ));
            };
            area.notes.push(rest.trim().to_string());
            continue;
        }
        if !indented {
            indent_mode = false;
        }

        if let Some(rest) = keyword_ignoring_case(trimmed, "title") {
            if document.title.is_some() {
                bail!(i18n::t!(
                    "`title` is written twice. Merge them into one, then open it again."
                ));
            }
            document.title = Some(rest.trim().to_string());
        } else if let Some(rest) = keyword_ignoring_case(trimmed, "set") {
            let (id, rest) = read_identifier(rest).ok_or_else(|| unreadable(trimmed))?;
            document.areas.push(read_area(vec![id], rest, trimmed)?);
            indent_mode = true;
            position = PreservedPosition::Bottom;
        } else if let Some(rest) = keyword_ignoring_case(trimmed, "union") {
            let (sets, rest) = read_identifier_list(rest).ok_or_else(|| unreadable(trimmed))?;
            document.areas.push(read_area(sets, rest, trimmed)?);
            indent_mode = true;
            position = PreservedPosition::Bottom;
        } else if let Some(rest) = keyword_ignoring_case(trimmed, "text") {
            document.texts.push(read_targeted(rest, trimmed)?);
        } else if let Some(rest) = keyword_ignoring_case(trimmed, "style") {
            document.styles.push(read_targeted(rest, trimmed)?);
        } else {
            return Err(unreadable(trimmed));
        }
    }

    if let Some(error) = document.structure_error() {
        bail!("{error}");
    }
    let emitted = emit_venn(&document);
    let written = merman_venn(&emitted).map_err(|error| {
        anyhow!(i18n::t!(
            "Mermaid could not read the diagram written back ({error}). Edit it as text."
        ))
    })?;
    if original != written {
        bail!(i18n::t!(
            "Read by the structured editor, this diagram would differ from what Mermaid reads (for example in the order of `text`). Edit it as text."
        ));
    }
    Ok(document)
}

fn unreadable(statement: &str) -> anyhow::Error {
    anyhow!(i18n::t!(
        "This could not be read as a statement of a Venn diagram ({statement}). Edit it as text."
    ))
}

/// What `merman` reads from `source`.
fn merman_venn(source: &str) -> Result<merman::diagrams::venn::VennDiagramRenderModel> {
    let parsed = crate::parse_for_render_model(source)
        .map_err(|error| {
            anyhow!(i18n::t!(
                "Mermaid could not parse this diagram ({error}). Edit it as text."
            ))
        })?
        .ok_or_else(|| anyhow!(i18n::t!("Mermaid could not read this as a diagram.")))?;
    let RenderSemanticModel::Venn(model) = parsed.into_parts().1 else {
        bail!(i18n::t!("This diagram is not a Venn diagram."));
    };
    Ok(model)
}

/// `merman`'s keywords ignore case and end at anything that cannot continue an
/// identifier, so `set"A"` is a `set` but `settings` is not.
fn keyword_ignoring_case<'a>(statement: &'a str, keyword: &str) -> Option<&'a str> {
    let prefix = statement.get(..keyword.len())?;
    if !prefix.eq_ignore_ascii_case(keyword) {
        return None;
    }
    let rest = statement.get(keyword.len()..)?;
    if rest.chars().next().is_some_and(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
    }) {
        return None;
    }
    Some(rest)
}

/// Finds a `%%` outside quotes and brackets, where `merman` cuts the line.
fn reject_trailing_comment(statement: &str) -> Result<()> {
    let mut in_quote = false;
    let mut depth = 0usize;
    let mut previous_percent = false;
    for character in statement.chars() {
        let percent = character == '%' && !in_quote && depth == 0;
        if percent && previous_percent {
            bail!(i18n::t!(
                "The structured editor would lose the comment at the end of the statement ({statement}). Move the comment to a line of its own, or edit it as text."
            ));
        }
        previous_percent = percent;
        match character {
            '"' => in_quote = !in_quote,
            '[' if !in_quote => depth += 1,
            ']' if !in_quote => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

fn skip_blanks(text: &str) -> &str {
    text.trim_start_matches([' ', '\t'])
}

/// An identifier the way `merman` reads one: a quoted string, whose quotes and
/// surrounding blanks are dropped, or a run of ASCII letters, digits, `_` and
/// `-` that starts with a letter or `_`.
fn read_identifier(text: &str) -> Option<(String, &str)> {
    let text = skip_blanks(text);
    if let Some(quoted) = text.strip_prefix('"') {
        let end = quoted.find('"')?;
        let value = quoted.get(..end)?;
        return Some((value.to_string(), quoted.get(end + 1..)?));
    }
    let first = text.chars().next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    let length = text
        .find(|character: char| {
            !(character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
        })
        .unwrap_or(text.len());
    Some((text.get(..length)?.to_string(), text.get(length..)?))
}

fn read_identifier_list(text: &str) -> Option<(Vec<String>, &str)> {
    let (first, mut rest) = read_identifier(text)?;
    let mut identifiers = vec![first];
    while let Some(after_comma) = skip_blanks(rest).strip_prefix(',') {
        let (next, after) = read_identifier(after_comma)?;
        identifiers.push(next);
        rest = after;
    }
    Some((identifiers, rest))
}

/// The label and size after an area's sets, and nothing else.
fn read_area(sets: Vec<String>, text: &str, statement: &str) -> Result<VennArea> {
    let mut rest = skip_blanks(text);
    let mut label = None;
    if let Some(after_bracket) = rest.strip_prefix('[') {
        let (text, after) = match after_bracket.strip_prefix('"') {
            Some(quoted) => {
                let end = quoted.find("\"]").ok_or_else(|| unreadable(statement))?;
                (
                    quoted.get(..end).unwrap_or_default(),
                    quoted.get(end + 2..).unwrap_or_default(),
                )
            }
            None => {
                let end = after_bracket
                    .find(']')
                    .ok_or_else(|| unreadable(statement))?;
                (
                    after_bracket.get(..end).unwrap_or_default(),
                    after_bracket.get(end + 1..).unwrap_or_default(),
                )
            }
        };
        label = Some(normalize_label(text)).filter(|label| !label.is_empty());
        rest = skip_blanks(after);
    }
    let mut size = None;
    if let Some(after_colon) = rest.strip_prefix(':') {
        let (token, after) = read_number(skip_blanks(after_colon)).ok_or_else(|| {
            anyhow!(i18n::t!(
                "The size could not be read as a number ({statement})."
            ))
        })?;
        size = Some(token.to_string());
        rest = after;
    }
    if !rest.trim().is_empty() {
        return Err(unreadable(statement));
    }
    Ok(VennArea {
        sets,
        label,
        size,
        notes: Vec::new(),
    })
}

/// `merman` trims a label and drops one pair of quotes around it.
fn normalize_label(text: &str) -> String {
    let trimmed = text.trim();
    match trimmed
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
    {
        Some(inner) => inner.to_string(),
        None => trimmed.to_string(),
    }
}

/// A number the way `merman` reads a size: an optional sign, digits and one
/// optional fraction.
fn read_number(text: &str) -> Option<(&str, &str)> {
    let mut length = 0;
    let bytes = text.as_bytes();
    if bytes
        .first()
        .is_some_and(|byte| matches!(byte, b'+' | b'-'))
    {
        length += 1;
    }
    let digits_before = bytes
        .get(length..)?
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    length += digits_before;
    let mut digits_after = 0;
    if bytes.get(length) == Some(&b'.') {
        length += 1;
        digits_after = bytes
            .get(length..)?
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        length += digits_after;
    }
    if digits_before == 0 && digits_after == 0 {
        return None;
    }
    Some((text.get(..length)?, text.get(length..)?))
}

fn read_targeted(text: &str, statement: &str) -> Result<VennTargeted> {
    let (targets, rest) = read_identifier_list(text).ok_or_else(|| unreadable(statement))?;
    Ok(VennTargeted {
        targets,
        rest: rest.trim().to_string(),
    })
}

/// Writes the document back as Mermaid source, without a trailing newline.
pub fn emit_venn(document: &VennDocument) -> String {
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
    if let Some(title) = &document.title {
        out.push_str(INDENT);
        out.push_str("title");
        if !title.is_empty() {
            out.push(' ');
            out.push_str(title);
        }
        out.push('\n');
    }
    for area in &document.areas {
        out.push_str(INDENT);
        out.push_str(if area.is_set() { "set " } else { "union " });
        out.push_str(&write_identifiers(&area.sets));
        if let Some(label) = &area.label {
            out.push_str(&format!("[\"{label}\"]"));
        }
        if let Some(size) = &area.size {
            out.push(':');
            out.push_str(size);
        }
        out.push('\n');
        for note in &area.notes {
            out.push_str(&format!("{INDENT}{INDENT}text {note}\n"));
        }
    }
    for style in &document.styles {
        out.push_str(&format!(
            "{INDENT}style {}{}\n",
            write_identifiers(&style.targets),
            spaced(&style.rest)
        ));
    }
    // An indented `text` after an area is read as a note of that area, so a
    // statement that names its own sets starts at the beginning of its line.
    for text in &document.texts {
        out.push_str(&format!(
            "text {}{}\n",
            write_identifiers(&text.targets),
            spaced(&text.rest)
        ));
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

fn spaced(rest: &str) -> String {
    if rest.is_empty() {
        String::new()
    } else {
        format!(" {rest}")
    }
}

fn preserved_at(
    document: &VennDocument,
    position: PreservedPosition,
) -> impl Iterator<Item = &str> {
    document
        .preserved_lines
        .iter()
        .filter(move |line| line.position == position)
        .map(|line| line.text.as_str())
}

fn write_identifiers(ids: &[String]) -> String {
    ids.iter()
        .map(|id| write_identifier(id))
        .collect::<Vec<_>>()
        .join(",")
}

/// An id bare when `merman` reads it back as one, quoted otherwise.
fn write_identifier(id: &str) -> String {
    let bare = id
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'));
    if bare {
        id.to_string()
    } else {
        format!("\"{id}\"")
    }
}

/// Whether `id` can be written as a set id. A quoted id cannot hold a quote,
/// and `merman` keeps the blanks inside the quotes, which would make two sets
/// look alike in the form.
pub fn is_valid_venn_id(id: &str) -> bool {
    !id.is_empty() && id.trim() == id && !id.contains(['"', '\n', '\r'])
}

/// Whether `label` can be written as an area label: it is written as `["…"]`,
/// which ends at the first quote, and `merman` trims it.
pub fn is_valid_venn_label(label: &str) -> bool {
    label.trim() == label && !label.contains(['"', '\n', '\r'])
}

/// Whether `title` can be written as the diagram's title, which runs to the
/// end of its line.
pub fn is_valid_venn_title(title: &str) -> bool {
    !title.contains("%%") && !title.contains(['\n', '\r']) && title.trim() == title
}

/// Whether `text` can be written as an area size: a number `merman` reads, and
/// not a negative one, which has no area to draw.
pub fn is_valid_venn_size(text: &str) -> bool {
    read_number(text).is_some_and(|(_, rest)| rest.is_empty())
        && text
            .parse::<f64>()
            .is_ok_and(|size| size.is_finite() && size >= 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sankey_model::merman_fixtures;
    use crate::{MermaidTheme, render_to_svg};

    const TEMPLATE: &str = "venn-beta\n    %% set で集合、union で重なりに名前を付ける\n    title 担当の重なり\n    set Front[\"フロントエンド\"]\n    set Back[\"バックエンド\"]\n    union Front,Back[\"API\"]";

    fn assert_stable(source: &str) -> String {
        let first =
            parse_venn(source).unwrap_or_else(|error| panic!("{source} should parse: {error}"));
        let first_text = emit_venn(&first);
        let second = parse_venn(&first_text)
            .unwrap_or_else(|error| panic!("emitted source should parse: {error}\n{first_text}"));
        assert_eq!(first, second, "the document changed through:\n{first_text}");
        assert_eq!(first_text, emit_venn(&second));
        first_text
    }

    fn assert_renders(source: &str) {
        let svg = render_to_svg(source, &MermaidTheme::default())
            .unwrap_or_else(|error| panic!("source should render: {error}\n{source}"));
        assert!(!svg.contains("Syntax error in text"), "{source}");
    }

    #[test]
    fn the_keyword_is_recognized_the_way_merman_recognizes_it() {
        for source in ["venn-beta\n    set A", "%%{init: {}}%%\nvenn-beta"] {
            assert!(is_venn_source(source), "{source} should be a Venn diagram");
        }
        for source in [
            "venn\n  set A",
            "venn-betas",
            "Venn-beta",
            "",
            "%% venn",
            "flowchart TD\n    A --> B",
        ] {
            assert!(
                !is_venn_source(source),
                "{source} should not be a Venn diagram"
            );
        }
    }

    #[test]
    fn the_insert_template_round_trips_byte_for_byte() {
        let document = parse_venn(TEMPLATE).expect("the template should parse");
        assert_eq!(document.title.as_deref(), Some("担当の重なり"));
        assert_eq!(document.set_ids(), vec!["Front", "Back"]);
        assert_eq!(emit_venn(&document), TEMPLATE);
        assert_renders(TEMPLATE);
    }

    #[test]
    fn notes_styles_and_quoted_ids_round_trip() {
        let source = "venn-beta\n  set A[\"Alpha\"]:20\n    text A1[\"React\"]\n    text A2[\"Design Systems\"]\n  set \"日本 語\"[Beta]:12\n  union A,\"日本 語\"[\"AB\"]:3\n  style A fill:#ff6b6b\n  style A,\"日本 語\" color:#333\n  style A1 color:red";
        let document = parse_venn(source).expect("source should parse");
        assert_eq!(document.set_ids(), vec!["A", "日本 語"]);
        assert_eq!(
            document.areas.first().map(|area| area.notes.clone()),
            Some(vec![
                "A1[\"React\"]".to_string(),
                "A2[\"Design Systems\"]".to_string()
            ])
        );
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "venn-beta\n    set A[\"Alpha\"]:20\n        text A1[\"React\"]\n        text A2[\"Design Systems\"]\n    set \"日本 語\"[\"Beta\"]:12\n    union A,\"日本 語\"[\"AB\"]:3\n    style A fill:#ff6b6b\n    style A,\"日本 語\" color:#333\n    style A1 color:red"
        );
        assert_renders(&emitted);
    }

    #[test]
    fn a_text_that_names_its_sets_starts_its_line() {
        let source =
            "venn-beta\nset A\nset B\nunion A,B\ntext A alpha[\"Alpha note\"]\ntext A,B 42";
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "venn-beta\n    set A\n    set B\n    union A,B\ntext A alpha[\"Alpha note\"]\ntext A,B 42"
        );
        assert_renders(&emitted);
    }

    #[test]
    fn renaming_a_set_reaches_overlaps_texts_and_styles() {
        let source = "venn-beta\nset A\nset B\nunion A,B\nstyle A,B fill:red\ntext A note";
        let mut document = parse_venn(source).expect("source should parse");
        document.rename_sets(&[
            ("A".to_string(), "B".to_string()),
            ("B".to_string(), "A".to_string()),
        ]);
        assert_eq!(
            emit_venn(&document),
            "venn-beta\n    set B\n    set A\n    union B,A\n    style B,A fill:red\ntext B note"
        );
        document.rename_sets(&[("B".to_string(), "日本".to_string())]);
        let emitted = emit_venn(&document);
        assert_eq!(
            emitted,
            "venn-beta\n    set \"日本\"\n    set A\n    union \"日本\",A\n    style \"日本\",A fill:red\ntext \"日本\" note"
        );
        assert_stable(&emitted);
        assert_renders(&emitted);
    }

    #[test]
    fn deleting_a_set_takes_its_overlaps_texts_and_styles_with_it() {
        let source = "venn-beta\n    set A\n    set B\n    set C\n    union A,B\n    union B,C\n    style A,B fill:red\n    style C fill:blue\ntext B note";
        let mut document = parse_venn(source).expect("source should parse");
        assert_eq!(document.areas_removed_with(1), vec![1, 3, 4]);
        assert_eq!(document.areas_removed_with(3), Vec::<usize>::new());
        assert_eq!(document.remove_set(1), vec![1, 3, 4]);
        let emitted = emit_venn(&document);
        assert_eq!(
            emitted,
            "venn-beta\n    set A\n    set C\n    style C fill:blue"
        );
        assert_eq!(document.structure_error(), None);
        assert_renders(&emitted);
    }

    #[test]
    fn broken_structures_are_refused() {
        let mut document = parse_venn(TEMPLATE).expect("the template should parse");
        document.areas.swap(0, 2);
        let error = document.structure_error().expect("the union comes first");
        assert!(error.contains("is written before"), "{error}");
        // The reason still holds: `merman` refuses a union of undeclared sets.
        assert!(merman_venn(&emit_venn(&document)).is_err());

        for (source_areas, expected) in [
            (vec![vec!["A"], vec!["A"]], "written twice"),
            (vec![vec!["A"], vec!["A", "Z"]], "does not exist"),
            (vec![vec!["A"], vec!["A", "A"]], "same set twice"),
            (
                vec![vec!["A"], vec!["B"], vec!["A", "B"], vec!["B", "A"]],
                "written twice",
            ),
            (vec![vec!["A B "]], "cannot be used"),
        ] {
            let mut document = VennDocument::empty();
            document.areas = source_areas
                .into_iter()
                .map(|sets| VennArea {
                    sets: sets.into_iter().map(str::to_string).collect(),
                    label: None,
                    size: None,
                    notes: Vec::new(),
                })
                .collect();
            let error = document
                .structure_error()
                .unwrap_or_else(|| panic!("{document:?} should be refused"));
            assert!(error.contains(expected), "{error}");
        }

        for size in ["-1", "abc", "1e3", "", "1.2.3"] {
            assert!(!is_valid_venn_size(size), "{size:?}");
        }
        for size in ["0", "1", "20", "5.3", ".5", "3."] {
            assert!(is_valid_venn_size(size), "{size:?}");
        }
        assert!(!is_valid_venn_label("引用\"あり"));
        assert!(is_valid_venn_label("日本語 ラベル"));
        assert!(!is_valid_venn_id(""));
        assert!(is_valid_venn_id("日本 語"));
    }

    #[test]
    fn preserved_lines_keep_their_side_of_the_first_area() {
        let source =
            "---\ntitle: 前置き\n---\nvenn-beta\n    %% 先頭\n    set A\n    %% 途中\n    set B";
        let document = parse_venn(source).expect("source should parse");
        assert_eq!(
            document
                .preserved_lines
                .iter()
                .map(|line| (line.position, line.text.trim()))
                .collect::<Vec<_>>(),
            vec![
                (PreservedPosition::Preamble, "---\ntitle: 前置き\n---"),
                (PreservedPosition::Top, "%% 先頭"),
                (PreservedPosition::Bottom, "%% 途中"),
            ]
        );
        assert_renders(&assert_stable(source));
    }

    #[test]
    fn what_cannot_be_held_is_refused_with_a_japanese_reason() {
        let _japanese = crate::mermaid_model::japanese_for_this_thread();
        for source in [
            "venn-beta\n    set A %% メモ",
            "venn-beta\n    title 一\n    title 二",
            "venn-beta\n    set A\n    union A,Z",
            "venn-beta set A",
            "venn-beta\n    set A:-1",
            "venn-beta\n    set A\n    set A",
            "venn-beta\n    set A\n    set B\n    text A,B x\n",
            "venn-beta\ntext A first\n    set A\n        text second",
            "venn-beta\n    accTitle: 概要",
        ] {
            let error = parse_venn(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            let message = format!("{error}");
            assert!(
                message.chars().any(|character| character > '\u{7f}'),
                "the message should be Japanese for `{source}`: {message}"
            );
        }
    }

    #[test]
    fn an_empty_fence_opens_as_a_blank_diagram() {
        let document = parse_venn("").expect("an empty fence should parse");
        assert_eq!(document, VennDocument::empty());
        assert_eq!(emit_venn(&document), "venn-beta");
        assert_eq!(
            parse_venn("venn-beta").expect("a bare header"),
            VennDocument::empty()
        );
    }

    /// The sources `merman`'s own Venn parser tests use.
    #[test]
    fn merman_parser_examples_round_trip() {
        for source in [
            "venn-beta\n          title foo bar\n          set A\n          set B\n          union A,B\n      ",
            "venn-beta\n          title foo bar\n          set A[\"Alpha\"]:20\n          set B[Beta]:12\n          set C[\"Gamma\"]:30\n          union A,B[\"AB\"]:5.3\n          union C,A,B:1\n      ",
            "venn-beta\n          set A[\"Frontend\"]\n            text A1[\"React\"]\n            text A2\n          set B[\"Backend\"]\n            text B1\n          union A,B[\"APIs\"]\n            text AB1[\"OpenAPI\"]\n      ",
            "venn-beta\n          set A\n          set B\n          union A,B\n          style A fill:#ff6b6b, color:#333\n          style A,B fill:rgb(255, 0, 128)\n          style B fill:rgba(255, 0, 128, 0.5)\n      ",
            "venn-beta\n        set \"Foo Bar\"\n        set Buz\n        union \"Foo Bar\",Buz\n    ",
        ] {
            let emitted = assert_stable(source);
            assert_eq!(
                merman_venn(source).ok(),
                merman_venn(&emitted).ok(),
                "{emitted}"
            );
            assert_renders(&emitted);
        }
    }

    #[test]
    fn merman_fixtures_round_trip_or_are_refused_in_japanese() {
        let fixtures = merman_fixtures("venn");
        if fixtures.is_empty() {
            eprintln!("the merman checkout was not found; skipping its Venn examples");
            return;
        }
        let mut opened = 0;
        let mut refused = Vec::new();
        for (name, source) in &fixtures {
            match parse_venn(source) {
                Ok(document) => {
                    opened += 1;
                    let emitted = emit_venn(&document);
                    let again = parse_venn(&emitted).unwrap_or_else(|error| {
                        panic!("{name}: emitted source should parse: {error}\n{emitted}")
                    });
                    assert_eq!(again, document, "{name}");
                    assert_eq!(emit_venn(&again), emitted, "{name}");
                    assert_eq!(
                        merman_venn(source).ok(),
                        merman_venn(&emitted).ok(),
                        "{name}"
                    );
                    assert_renders(&emitted);
                }
                Err(error) => {
                    let message = format!("{error}");
                    assert!(
                        message.chars().any(|character| character > '\u{7f}'),
                        "{name}: {message}"
                    );
                    refused.push((name.clone(), message));
                }
            }
        }
        eprintln!(
            "venn fixtures: {opened} opened, {} refused: {refused:#?}",
            refused.len()
        );
        assert!(opened > 0);
    }
}
