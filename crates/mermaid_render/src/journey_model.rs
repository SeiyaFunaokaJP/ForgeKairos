//! A round-trippable editing model for Mermaid user journeys.
//!
//! The shape of this module mirrors [`crate::timeline_model`]: [`parse_journey`]
//! reads the body of a ```` ```mermaid ```` fence into [`JourneyDocument`], and
//! [`emit_journey`] writes it back. See [`crate::mermaid_model`] for the
//! invariants every diagram kind keeps.
//!
//! `merman` reads a journey line by line, and this module reads it the same way:
//!
//! - A line ends at its first `#` or `;` (a `#name;` entity excepted), and what
//!   follows is dropped. A line with anything but a lone `;` there is refused
//!   rather than written back without it.
//! - A task is `name: score: person, person`. The name runs to the first `:`,
//!   the score to the second, and the people to the third; anything after a
//!   third `:` is dropped, so such a line is refused.
//! - A section name runs to the first `:`; a name with one in it is refused.
//! - Tasks written before the first `section` belong to no section and are
//!   drawn without a band.
//!
//! What the parser rewrites, all of it stable from the first emit: the
//! indentation, the whitespace around a value, the case of a keyword, and
//! comment lines in the middle of the body, which move below it as they do in a
//! timeline.

use std::collections::BTreeSet;
use std::ops::RangeInclusive;

use anyhow::{Result, anyhow, bail};
use merman::diagram::RenderSemanticModel;

use crate::flowchart_model::{PreservedLine, PreservedPosition};
use crate::mermaid_model::{INDENT, first_statement_line, join_lines, keyword_tail, scan_preamble};

const HEADER: &str = "journey";

/// The indentation of a task under a `section` line, one step deeper than the
/// section, the way Mermaid's own examples write it.
const TASK_INDENT: &str = "      ";

/// The scores the drawing has a face height for. Mermaid reads any number, but
/// one outside these moves the face off the chart.
pub const JOURNEY_SCORES: RangeInclusive<u8> = 1..=5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JourneyTask {
    pub name: String,
    pub score: u8,
    /// The people taking part, in the order written. `[""]` stands for a task
    /// written with a trailing `:` and nobody after it, which Mermaid draws as
    /// one person with no name.
    pub people: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JourneySection {
    /// `None` for the tasks written before the first `section` line. Only the
    /// first section can be nameless: a later one would run into the section
    /// above it.
    pub name: Option<String>,
    pub tasks: Vec<JourneyTask>,
}

/// An editable user journey.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JourneyDocument {
    /// The title, without the `title` keyword. Mermaid draws no title for a
    /// `title` statement with nothing after it, so an empty title is `None`.
    pub title: Option<String>,
    pub sections: Vec<JourneySection>,
    pub preserved_lines: Vec<PreservedLine>,
}

impl JourneyDocument {
    /// A blank user journey, used when the editor is opened on an empty fence.
    pub fn empty() -> Self {
        Self {
            title: None,
            sections: Vec::new(),
            preserved_lines: Vec::new(),
        }
    }

    /// Every task, in the order the fence writes them.
    pub fn tasks(&self) -> impl Iterator<Item = &JourneyTask> {
        self.sections.iter().flat_map(|section| &section.tasks)
    }

    /// Everyone who takes part in some task, sorted the way the legend of the
    /// drawing lists them.
    pub fn people(&self) -> Vec<String> {
        self.tasks()
            .flat_map(|task| &task.people)
            .filter(|person| !person.is_empty())
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Renames `old` to `new` in every task. A task that already had `new`
    /// keeps one of them, since Mermaid would otherwise draw the person twice.
    pub fn rename_person(&mut self, old: &str, new: &str) {
        for section in &mut self.sections {
            for task in &mut section.tasks {
                if !task.people.iter().any(|person| person == old) {
                    continue;
                }
                let mut people: Vec<String> = Vec::with_capacity(task.people.len());
                for person in &task.people {
                    let person = if person == old { new } else { person.as_str() };
                    if !people.iter().any(|kept| kept == person) {
                        people.push(person.to_string());
                    }
                }
                task.people = people;
            }
        }
    }

    /// Why the document cannot be written back as a diagram Mermaid draws the
    /// way the form shows it, in Japanese, or `None` when it can.
    pub fn structure_error(&self) -> Option<String> {
        if let Some(title) = &self.title
            && !is_valid_journey_title(title)
        {
            return Some(
                i18n::t!("The title cannot contain line breaks, `#` or `;` (and cannot start or end with whitespace).")
                    .to_string(),
            );
        }

        for (index, section) in self.sections.iter().enumerate() {
            match &section.name {
                Some(name) if !is_valid_journey_section_name(name) => {
                    return Some(i18n::t!(
                        "The section name `{name}` cannot be used (no `:`, `#`, `;` or line breaks, and it cannot be empty)."
                    ));
                }
                None if index > 0 => {
                    return Some(
                        i18n::t!("Every section after the first needs a name (without one, it merges with the section above).")
                            .to_string(),
                    );
                }
                _ => {}
            }
        }

        for task in self.tasks() {
            if !is_valid_journey_task_name(&task.name) {
                return Some(i18n::t!(
                    "The task name `{}` cannot be used (no `:`, `#`, `;` or line breaks, and it cannot be empty. It also cannot start with `title `, `section `, `accTitle`, `accDescr` or `%%`).",
                    task.name
                ));
            }
            if !JOURNEY_SCORES.contains(&task.score) {
                return Some(i18n::t!(
                    "Task `{}` has a score of {}. Use a score from 1 to 5.",
                    task.name,
                    task.score
                ));
            }
            if !are_valid_journey_people(&task.people) {
                return Some(i18n::t!(
                    "Task `{}` cannot list the people `{}` (names cannot contain `:`, `,`, `#`, `;` or line breaks, and cannot be empty).",
                    task.name,
                    task.people.join(", ")
                ));
            }
        }
        None
    }
}

/// Whether `source` is a user journey: its first statement is `journey`.
pub fn is_journey_source(source: &str) -> bool {
    first_statement_line(source).is_some_and(|line| keyword_tail(line, HEADER).is_some())
}

/// Reads `source` — the body of a ```` ```mermaid ```` fence — into an editable
/// document.
pub fn parse_journey(source: &str) -> Result<JourneyDocument> {
    let document = scan_journey(source)?;

    // `merman` is asked last, so that a construct this module knows about is
    // reported with the reason that says what to do.
    if !source.trim().is_empty() {
        let parsed = crate::parse_for_render_model(source)
            .map_err(|error| {
                anyhow!(i18n::t!(
                    "Mermaid could not parse this diagram ({error}). Edit it as text."
                ))
            })?
            .ok_or_else(|| anyhow!(i18n::t!("Mermaid could not read this as a diagram.")))?;
        if !matches!(parsed.model(), RenderSemanticModel::Journey(_)) {
            bail!(i18n::t!("This diagram is not a user journey."));
        }
    }

    Ok(document)
}

fn scan_journey(source: &str) -> Result<JourneyDocument> {
    let lines = source.lines().collect::<Vec<_>>();
    let preamble = scan_preamble(&lines);
    let mut document = JourneyDocument::empty();
    document.preserved_lines = preamble.preserved;

    let Some(header_index) = preamble.header else {
        return Ok(document);
    };

    let header = lines.get(header_index).copied().unwrap_or_default().trim();
    let Some(tail) = keyword_tail(header, HEADER) else {
        bail!(i18n::t!(
            "This is not a user journey (only diagrams that start with `journey` can be edited in the structured editor)."
        ));
    };
    if !tail.is_empty() {
        bail!(i18n::t!(
            "`{tail}` on the `journey` line is not supported by the structured editor. Edit it as text."
        ));
    }

    let mut index = header_index + 1;
    let mut position = PreservedPosition::Top;
    let mut seen_title = false;
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
            if let Some(rest) = text_after_directive(&text) {
                bail!(i18n::t!(
                    "Mermaid reads `{rest}` after the directive `%%{{…}}%%` as a statement, but the structured editor would move it along with the comments. Put the directive on a line of its own."
                ));
            }
            document
                .preserved_lines
                .push(PreservedLine { position, text });
            continue;
        }
        if trimmed.starts_with('#') {
            // Mermaid turns `#name;` into an entity before it reads the line,
            // so such a line is not the comment it looks like.
            if entity_length(trimmed.as_bytes()).is_some() {
                bail!(i18n::t!(
                    "Mermaid reads `{trimmed}` as a character reference, so it cannot be kept as a comment. Edit it as text."
                ));
            }
            document.preserved_lines.push(PreservedLine {
                position,
                text: line.trim_end().to_string(),
            });
            continue;
        }

        if drops_hex_style_semicolon(line) {
            bail!(i18n::t!(
                "Mermaid removes the `;` that follows `:#…` after `style` or `classDef` before reading the line, so writing it back would change its meaning ({trimmed}). Add a space after the `:`, or edit it as text."
            ));
        }
        let statement = match statement_end(line) {
            Some(end) => {
                // A lone `;` at the end is a statement separator with nothing
                // after it, so dropping it loses nothing.
                if line.get(end..).unwrap_or_default().trim() != ";" {
                    bail!(i18n::t!(
                        "Mermaid skips everything from `#` or `;` on ({trimmed}). Remove it, or edit it as text."
                    ));
                }
                line.get(..end).unwrap_or_default()
            }
            None => line,
        };

        match classify(statement.trim_start()) {
            Statement::Accessibility { block } => {
                if block && !statement.contains('}') {
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
            }
            Statement::Title(value) => {
                // Mermaid keeps the last one, so a second statement — even an
                // empty one, which clears the title — would make writing the
                // journey back drop a line the user can see.
                if seen_title {
                    bail!(i18n::t!(
                        "`title` is written twice. Merge them into one, then open it again."
                    ));
                }
                seen_title = true;
                let value = value.trim();
                if !value.is_empty() {
                    document.title = Some(value.to_string());
                }
            }
            Statement::Section(value) => {
                if value.contains(':') {
                    bail!(i18n::t!(
                        "Mermaid skips everything in a section name from `:` on ({trimmed}). Delete the `:`, or edit it as text."
                    ));
                }
                let name = value.trim();
                if name.is_empty() {
                    bail!(i18n::t!(
                        "A `section` without a name is not supported by the structured editor ({trimmed}). Give it a name, or edit it as text."
                    ));
                }
                document.sections.push(JourneySection {
                    name: Some(name.to_string()),
                    tasks: Vec::new(),
                });
                position = PreservedPosition::Bottom;
            }
            Statement::Task { name, data } => {
                let task = parse_task(name, data, trimmed)?;
                if document.sections.is_empty() {
                    document.sections.push(JourneySection {
                        name: None,
                        tasks: Vec::new(),
                    });
                }
                if let Some(section) = document.sections.last_mut() {
                    section.tasks.push(task);
                }
                position = PreservedPosition::Bottom;
            }
            Statement::Unknown => {
                bail!(i18n::t!(
                    "`{trimmed}` cannot be read as a task (`name: score: people`) or as any other statement. Edit it as text."
                ));
            }
        }
    }

    Ok(document)
}

/// What one line of a journey body is, decided in the order `merman`'s journey
/// parser tries the possibilities.
#[derive(Debug, PartialEq, Eq)]
enum Statement<'a> {
    Title(&'a str),
    Accessibility { block: bool },
    Section(&'a str),
    Task { name: &'a str, data: &'a str },
    Unknown,
}

fn classify(statement: &str) -> Statement<'_> {
    if let Some(value) = keyword_argument(statement, "title") {
        return Statement::Title(value);
    }
    if let Some(block) = accessibility_kind(statement) {
        return Statement::Accessibility { block };
    }
    if let Some(value) = keyword_argument(statement, "section") {
        return Statement::Section(value);
    }
    match statement.find(':') {
        Some(colon) => Statement::Task {
            name: statement.get(..colon).unwrap_or_default(),
            data: statement.get(colon + 1..).unwrap_or_default(),
        },
        None => Statement::Unknown,
    }
}

/// What follows `keyword` at the start of `line`, the way `merman`'s journey
/// parser reads a keyword argument: the word is matched without regard to case
/// and has to be followed by one whitespace character, which is dropped.
fn keyword_argument<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
    if !starts_with_keyword(line, keyword) {
        return None;
    }
    let rest = line.get(keyword.len()..)?;
    let first = rest.chars().next()?;
    if !first.is_whitespace() {
        return None;
    }
    rest.get(first.len_utf8()..)
}

/// `get` rather than slicing, because a line that opens with a multibyte name
/// would otherwise be cut inside a character.
fn starts_with_keyword(line: &str, keyword: &str) -> bool {
    line.get(..keyword.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(keyword))
}

/// Whether the statement is an accessibility statement, and whether it opens
/// an `accDescr { … }` block.
fn accessibility_kind(statement: &str) -> Option<bool> {
    for keyword in ["accTitle", "accDescr"] {
        if !starts_with_keyword(statement, keyword) {
            continue;
        }
        let rest = statement
            .get(keyword.len()..)
            .unwrap_or_default()
            .trim_start();
        if rest.starts_with(':') {
            return Some(false);
        }
        if keyword == "accDescr" && rest.starts_with('{') {
            return Some(true);
        }
    }
    None
}

/// Whether Mermaid removes a `;` from `line` before the journey is read. To keep
/// a colour like `style x fill:#f00;` from being read as the `#f00;` entity,
/// it drops the last `;` after a `:#` that follows `style` or `classDef`
/// anywhere in a line, so `#9829;` written straight after a task's `:` there is
/// no longer an entity but the start of a comment.
fn drops_hex_style_semicolon(line: &str) -> bool {
    ["style", "classDef"].into_iter().any(|keyword| {
        line.match_indices(keyword).any(|(start, _)| {
            let after_keyword = line.get(start + keyword.len()..).unwrap_or_default();
            after_keyword.match_indices(':').any(|(colon, _)| {
                let after_colon = after_keyword.get(colon + 1..).unwrap_or_default();
                after_colon
                    .chars()
                    .take_while(|character| !character.is_whitespace())
                    .position(|character| character == '#')
                    .is_some_and(|hash| {
                        after_colon
                            .chars()
                            .skip(hash + 1)
                            .any(|character| character == ';')
                    })
            })
        })
    })
}

/// What follows the last `}%%` of a directive block, when there is something.
/// Mermaid cuts the directive out and reads the rest as an ordinary line,
/// which a preserved comment block would carry to another place.
pub(crate) fn text_after_directive(block: &str) -> Option<&str> {
    if !block.trim_start().starts_with("%%{") {
        return None;
    }
    let end = block.rfind("}%%")?;
    let rest = block.get(end + "}%%".len()..)?.trim();
    (!rest.is_empty()).then_some(rest)
}

/// The length of the word of a `#word;` entity at the start of `text`, which
/// Mermaid replaces with a placeholder before the journey parser sees it.
fn entity_length(text: &[u8]) -> Option<usize> {
    let rest = text.strip_prefix(b"#")?;
    let word = rest
        .iter()
        .take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_')
        .count();
    (word > 0 && rest.get(word) == Some(&b';')).then_some(word)
}

/// Where `merman` ends a statement: at the first `#` or `;` that is not part
/// of an entity. Bytes rather than characters, since both markers are ASCII
/// and never occur inside a multibyte character.
fn statement_end(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b';' => return Some(index),
            b'#' => match entity_length(bytes.get(index..).unwrap_or_default()) {
                Some(word) => {
                    index += word + 2;
                    continue;
                }
                None => return Some(index),
            },
            _ => {}
        }
        index += 1;
    }
    None
}

fn parse_task(name: &str, data: &str, trimmed: &str) -> Result<JourneyTask> {
    let name = name.trim();
    if name.is_empty() {
        bail!(i18n::t!(
            "Mermaid skips a line without a task name ({trimmed}). Give it a name, or edit it as text."
        ));
    }
    let pieces = data.split(':').collect::<Vec<_>>();
    if pieces.len() > 2 {
        bail!(i18n::t!(
            "Mermaid skips everything from the third `:` on ({trimmed}). Remove the `:`, or edit it as text."
        ));
    }
    let score_text = pieces.first().copied().unwrap_or_default().trim();
    let score = score_text
        .parse::<u8>()
        .ok()
        .filter(|score| {
            score_text.bytes().all(|byte| byte.is_ascii_digit()) && JOURNEY_SCORES.contains(score)
        })
        .ok_or_else(|| {
            anyhow!(i18n::t!(
                "The score `{score_text}` is not supported by the structured editor ({trimmed}). Use a whole number from 1 to 5, or edit it as text."
            ))
        })?;
    let people = match pieces.get(1) {
        None => Vec::new(),
        Some(list) => {
            let people = list
                .split(',')
                .map(|person| person.trim().to_string())
                .collect::<Vec<_>>();
            if people.len() > 1 && people.iter().any(String::is_empty) {
                bail!(i18n::t!(
                    "One of the people has an empty name ({trimmed}). Remove it, or edit it as text."
                ));
            }
            people
        }
    };
    Ok(JourneyTask {
        name: name.to_string(),
        score,
        people,
    })
}

/// Writes the document back as Mermaid source, without a trailing newline.
pub fn emit_journey(document: &JourneyDocument) -> String {
    let mut out = String::new();

    for line in preserved_at(document, PreservedPosition::Preamble) {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(HEADER);
    out.push('\n');
    // The lines kept from before the first section go above the title, which
    // is where the insert template writes its explanatory comment.
    for line in preserved_at(document, PreservedPosition::Top) {
        out.push_str(line);
        out.push('\n');
    }
    if let Some(title) = &document.title {
        out.push_str(&format!("{INDENT}title {title}\n"));
    }

    for section in &document.sections {
        let indent = match &section.name {
            Some(name) => {
                out.push_str(&format!("{INDENT}section {name}\n"));
                TASK_INDENT
            }
            None => INDENT,
        };
        for task in &section.tasks {
            out.push_str(indent);
            out.push_str(&task_line(task));
            out.push('\n');
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

fn task_line(task: &JourneyTask) -> String {
    let people = task.people.join(", ");
    if task.people.is_empty() {
        format!("{}: {}", task.name, task.score)
    } else if people.is_empty() {
        format!("{}: {}:", task.name, task.score)
    } else {
        format!("{}: {}: {people}", task.name, task.score)
    }
}

fn preserved_at(
    document: &JourneyDocument,
    position: PreservedPosition,
) -> impl Iterator<Item = &str> {
    document
        .preserved_lines
        .iter()
        .filter(move |line| line.position == position)
        .map(|line| line.text.as_str())
}

/// Whether `text` can stand in a statement without being cut short: no line
/// break, and no `#` or `;` that Mermaid would end the statement at.
fn is_single_statement_text(text: &str) -> bool {
    !text.contains(['\n', '\r']) && statement_end(text).is_none()
}

/// Whether `title` can be written after `title`.
pub fn is_valid_journey_title(title: &str) -> bool {
    !title.is_empty() && title.trim() == title && is_single_statement_text(title)
}

/// Whether `name` can be written after `section`, which Mermaid cuts at the
/// first `:`.
pub fn is_valid_journey_section_name(name: &str) -> bool {
    !name.is_empty() && name.trim() == name && !name.contains(':') && is_single_statement_text(name)
}

/// Whether `name` reads back as the name of a task rather than as a statement
/// of some other kind or as a name cut short.
pub fn is_valid_journey_task_name(name: &str) -> bool {
    !name.is_empty()
        && name.trim() == name
        && !name.contains(':')
        && !name.starts_with('#')
        && !name.starts_with("%%")
        && is_single_statement_text(name)
        && classify(&format!("{name}: 3")) == Statement::Task { name, data: " 3" }
}

/// Whether one person's name can be written in a task's list of people.
pub fn is_valid_journey_person(person: &str) -> bool {
    !person.is_empty()
        && person.trim() == person
        && !person.contains([':', ','])
        && is_single_statement_text(person)
}

/// Whether a task's list of people reads back as itself. The one list with an
/// empty name in it that can be written is `[""]`, a trailing `:`.
pub fn are_valid_journey_people(people: &[String]) -> bool {
    people == [String::new()] || people.iter().all(|person| is_valid_journey_person(person))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MermaidTheme, render_to_svg};
    use merman::{Engine, ParseOptions};

    /// The template `markdown::InsertMermaidDiagram` inserts, copied from
    /// `editor::actions::MermaidDiagramKind::Journey`; the editor's own tests
    /// read it from the action.
    const TEMPLATE: &str = "journey\n    %% 作業 : 満足度 (1〜5) : 関わる人\n    title 購入までの流れ\n    section 探す\n      商品を検索する: 4: 利用者\n      比較する: 3: 利用者\n    section 買う\n      カートに入れる: 5: 利用者\n      支払う: 2: 利用者, 店舗";

    type Reading = (Option<String>, Vec<(String, String, i64, Vec<String>)>);

    fn parse(source: &str) -> JourneyDocument {
        parse_journey(source).unwrap_or_else(|error| panic!("{source} should parse: {error}"))
    }

    /// Emitting must reach a fixed point after one pass, and the text is what
    /// is compared, so a line that vanished from both documents still fails.
    fn assert_stable(source: &str) -> String {
        let first = parse(source);
        let first_text = emit_journey(&first);
        let second = parse(&first_text);
        let second_text = emit_journey(&second);
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

    fn task(name: &str, score: u8, people: &[&str]) -> JourneyTask {
        JourneyTask {
            name: name.to_string(),
            score,
            people: people.iter().map(|person| person.to_string()).collect(),
        }
    }

    /// What `merman` itself reads from `source`: the title, and each task as
    /// its section, trimmed name, score and people.
    fn merman_reading(source: &str) -> Reading {
        let parsed = Engine::new()
            .parse_diagram_for_render_model_sync(source, ParseOptions::strict())
            .unwrap_or_else(|error| panic!("merman should read {source}: {error}"))
            .unwrap_or_else(|| panic!("merman should find a diagram in {source}"));
        let RenderSemanticModel::Journey(model) = parsed.into_parts().1 else {
            panic!("merman should read {source} as a journey");
        };
        (
            model
                .title
                .map(|title| title.trim().to_string())
                .filter(|title| !title.is_empty()),
            model
                .tasks
                .into_iter()
                .map(|task| {
                    (
                        task.section.trim().to_string(),
                        task.task.trim().to_string(),
                        task.score,
                        task.people,
                    )
                })
                .collect(),
        )
    }

    /// The model's reading of a journey, in the shape of [`merman_reading`].
    fn model_reading(document: &JourneyDocument) -> Reading {
        (
            document.title.clone(),
            document
                .sections
                .iter()
                .flat_map(|section| {
                    section.tasks.iter().map(|task| {
                        (
                            section.name.clone().unwrap_or_default(),
                            task.name.clone(),
                            i64::from(task.score),
                            task.people.clone(),
                        )
                    })
                })
                .collect(),
        )
    }

    fn merman_fixtures(kind: &str) -> Vec<(String, String)> {
        crate::merman_fixtures(kind)
            .into_iter()
            .map(|(path, source)| (path.display().to_string(), source))
            .collect()
    }

    #[test]
    fn the_insert_template_round_trips_byte_for_byte() {
        let document = parse(TEMPLATE);
        assert_eq!(document.title.as_deref(), Some("購入までの流れ"));
        assert_eq!(
            document.sections,
            vec![
                JourneySection {
                    name: Some("探す".to_string()),
                    tasks: vec![
                        task("商品を検索する", 4, &["利用者"]),
                        task("比較する", 3, &["利用者"]),
                    ],
                },
                JourneySection {
                    name: Some("買う".to_string()),
                    tasks: vec![
                        task("カートに入れる", 5, &["利用者"]),
                        task("支払う", 2, &["利用者", "店舗"]),
                    ],
                },
            ]
        );
        assert_eq!(emit_journey(&document), TEMPLATE);
        assert_eq!(document.structure_error(), None);
        assert_eq!(model_reading(&document), merman_reading(TEMPLATE));
        assert_renders(TEMPLATE);
    }

    #[test]
    fn tasks_before_the_first_section_and_the_people_forms_survive() {
        let source = "journey\n  最初の作業: 3\n  section 準備\n    A: 5:Bob, Charlie\n    B: 1 : \n    C: 4\nsection 仕上げ\n    D:2: Alice;";
        let document = parse(source);
        assert_eq!(
            document.sections,
            vec![
                JourneySection {
                    name: None,
                    tasks: vec![task("最初の作業", 3, &[])],
                },
                JourneySection {
                    name: Some("準備".to_string()),
                    tasks: vec![
                        task("A", 5, &["Bob", "Charlie"]),
                        task("B", 1, &[""]),
                        task("C", 4, &[]),
                    ],
                },
                JourneySection {
                    name: Some("仕上げ".to_string()),
                    tasks: vec![task("D", 2, &["Alice"])],
                },
            ]
        );
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "journey\n    最初の作業: 3\n    section 準備\n      A: 5: Bob, Charlie\n      B: 1:\n      C: 4\n    section 仕上げ\n      D: 2: Alice"
        );
        assert_eq!(model_reading(&document), merman_reading(source));
        assert_eq!(merman_reading(&emitted), merman_reading(source));
        assert_renders(&emitted);
    }

    #[test]
    fn comments_accessibility_and_the_preamble_keep_their_place() {
        let source = "---\ntitle: 図\n---\n%%{init: {\"theme\": \"dark\"}}%%\njourney\n    %% 先頭のメモ\n    accTitle: 概要\n    title 一日\n    # シャープのコメント\n    section 朝\n      起きる: 3: 私\n    accDescr {\n      説明の 1 行目\n      2 行目\n    }\n    %% 途中のメモ\n      出かける: 4: 私";
        let document = parse(source);
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
                    "---\ntitle: 図\n---".to_string()
                ),
                (
                    PreservedPosition::Preamble,
                    "%%{init: {\"theme\": \"dark\"}}%%".to_string()
                ),
                (PreservedPosition::Top, "%% 先頭のメモ".to_string()),
                (PreservedPosition::Top, "accTitle: 概要".to_string()),
                (PreservedPosition::Top, "# シャープのコメント".to_string()),
                (
                    PreservedPosition::Bottom,
                    "accDescr {\n      説明の 1 行目\n      2 行目\n    }".to_string()
                ),
                (PreservedPosition::Bottom, "%% 途中のメモ".to_string()),
            ]
        );
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "---\ntitle: 図\n---\n%%{init: {\"theme\": \"dark\"}}%%\njourney\n    %% 先頭のメモ\n    accTitle: 概要\n    # シャープのコメント\n    title 一日\n    section 朝\n      起きる: 3: 私\n      出かける: 4: 私\n    accDescr {\n      説明の 1 行目\n      2 行目\n    }\n    %% 途中のメモ"
        );
        assert_eq!(merman_reading(&emitted), merman_reading(source));
        assert_renders(&emitted);
    }

    #[test]
    fn constructs_that_would_be_dropped_are_refused() {
        for (source, expected) in [
            ("journey\n    title a\n    title b", "written twice"),
            ("journey\n    title a\n    title ", "written twice"),
            (
                "journey\n    section 前半: 補足\n    A: 5",
                "section name from `:` on",
            ),
            ("journey\n    section \n    A: 5", "without a name"),
            ("journey\n    A: 5: Me # メモ", "from `#` or `;` on"),
            ("journey\n    A: 5: Me; B: 3: Me", "from `#` or `;` on"),
            ("journey\n    A: 5: Me: 補足", "third `:`"),
            ("journey\n    A: 7: Me", "score `7`"),
            ("journey\n    A: 2.5: Me", "score `2.5`"),
            ("journey\n    A: : Me", "score ``"),
            ("journey\n    A:", "score ``"),
            ("journey\n    A: 5: Me, , You", "empty name"),
            ("journey\n    : 5: Me", "without a task name"),
            ("journey\n    #x; 何か: 5", "character reference"),
            ("journey\n    作業だけ", "cannot be read as a task"),
            ("journey LR\n    A: 5", "on the `journey` line"),
            (
                "journey\n    section 前半\n      A: 3\n    %%{init: {}}%% section 後半\n      B: 4",
                "after the directive",
            ),
            (
                "journey\n    title 甲\n    %%{init: {}}%% title 乙",
                "after the directive",
            ),
            ("journey\n    Lifestyle: 3:#9829;", "`style`"),
        ] {
            let error = parse_journey(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            let message = format!("{error}");
            assert!(
                message.contains(expected),
                "the reason for `{source}` should mention {expected}: {message}"
            );
        }
    }

    /// Mermaid cuts a directive out of the line and reads what follows it, so
    /// only a directive on a line of its own may move with the comments.
    #[test]
    fn a_directive_on_its_own_line_is_kept_and_the_journey_stays_in_order() {
        let source = "journey\n    section 前半\n      A: 3\n    %%{init: {\"theme\": \"dark\"}}%%\n    section 後半\n      B: 4";
        let emitted = assert_stable(source);
        assert!(
            emitted.contains("%%{init: {\"theme\": \"dark\"}}%%"),
            "{emitted}"
        );
        assert_eq!(merman_reading(&emitted), merman_reading(source));
    }

    #[test]
    fn entities_are_text_rather_than_the_end_of_a_statement() {
        let source = "journey\n    section 家族\n      車に乗る: 3: 父, 子#1;, 子#9829;";
        let document = parse(source);
        assert_eq!(
            document
                .sections
                .first()
                .map(|section| section.tasks.clone()),
            Some(vec![task("車に乗る", 3, &["父", "子#1;", "子#9829;"])])
        );
        assert_eq!(document.structure_error(), None);
        let emitted = assert_stable(source);
        assert_eq!(emitted, source);
        assert_renders(&emitted);
    }

    #[test]
    fn renaming_a_person_follows_every_task_and_merges_duplicates() {
        let mut document = parse(TEMPLATE);
        assert_eq!(
            document.people(),
            vec!["利用者".to_string(), "店舗".to_string()]
        );
        document.rename_person("利用者", "顧客");
        assert!(
            document
                .tasks()
                .all(|task| task.people.first().map(String::as_str) == Some("顧客"))
        );
        document.rename_person("店舗", "顧客");
        assert_eq!(
            document.tasks().last().map(|task| task.people.clone()),
            Some(vec!["顧客".to_string()])
        );
        assert_eq!(document.people(), vec!["顧客".to_string()]);
        let emitted = emit_journey(&document);
        assert!(!emitted.contains("利用者"), "{emitted}");
        assert!(!emitted.contains("店舗"), "{emitted}");
        assert_eq!(parse(&emitted), document);
        assert_eq!(model_reading(&document), merman_reading(&emitted));
        assert_renders(&emitted);
    }

    fn first_task(document: &mut JourneyDocument) -> Option<&mut JourneyTask> {
        document
            .sections
            .get_mut(0)
            .and_then(|section| section.tasks.get_mut(0))
    }

    #[test]
    fn structure_errors_catch_what_the_form_can_type() {
        let mut document = parse(TEMPLATE);
        assert_eq!(document.structure_error(), None);

        let edits: [(&str, fn(&mut JourneyDocument)); 7] = [
            ("The title", |document| {
                document.title = Some("a # b".to_string())
            }),
            ("section name", |document| {
                if let Some(section) = document.sections.get_mut(0) {
                    section.name = Some("a: b".to_string());
                }
            }),
            ("after the first", |document| {
                if let Some(section) = document.sections.get_mut(1) {
                    section.name = None;
                }
            }),
            ("task name", |document| {
                if let Some(task) = first_task(document) {
                    task.name = "section 次".to_string();
                }
            }),
            ("task name", |document| {
                if let Some(task) = first_task(document) {
                    task.name = "a: b".to_string();
                }
            }),
            ("score of", |document| {
                if let Some(task) = first_task(document) {
                    task.score = 0;
                }
            }),
            ("cannot list the people", |document| {
                if let Some(task) = first_task(document) {
                    task.people = vec!["a".to_string(), String::new()];
                }
            }),
        ];
        for (expected, edit) in edits {
            let mut edited = document.clone();
            edit(&mut edited);
            let error = edited
                .structure_error()
                .unwrap_or_else(|| panic!("the edit for {expected} should be refused"));
            assert!(error.contains(expected), "{expected}: {error}");
        }

        // The nameless band before the first section is drawn, unlike a
        // timeline's, so it may sit in front of named sections.
        document.sections.insert(
            0,
            JourneySection {
                name: None,
                tasks: vec![task("前置き", 3, &[])],
            },
        );
        assert_eq!(document.structure_error(), None);
        let emitted = emit_journey(&document);
        assert_eq!(parse(&emitted), document, "{emitted}");
        assert_eq!(model_reading(&document), merman_reading(&emitted));
    }

    #[test]
    fn values_are_validated() {
        for name in ["作業", "Titles", "title", "sections", "Section", "子#1;"] {
            assert!(is_valid_journey_task_name(name), "{name}");
        }
        for name in [
            "",
            " a",
            "a:b",
            "a;b",
            "title x",
            "section x",
            "accTitle",
            "accDescr",
            "%% x",
            "#メモ",
            "C#",
            "改行\nあり",
        ] {
            assert!(!is_valid_journey_task_name(name), "{name}");
        }
        for person in ["Me", "子#1;", "山田 太郎"] {
            assert!(is_valid_journey_person(person), "{person}");
        }
        for person in ["", " a", "a,b", "a:b", "C#", "a;"] {
            assert!(!is_valid_journey_person(person), "{person}");
        }
        assert!(are_valid_journey_people(&[]));
        assert!(are_valid_journey_people(&[String::new()]));
        assert!(!are_valid_journey_people(&[String::new(), String::new()]));
        assert!(is_valid_journey_section_name("探す<br>見る"));
        assert!(!is_valid_journey_section_name("a:b"));
        assert!(is_valid_journey_title("一日: 概要"));
        assert!(!is_valid_journey_title("a; b"));

        // Everything the predicates accept has to come back as itself.
        let document = JourneyDocument {
            title: Some("一日: 概要".to_string()),
            sections: vec![JourneySection {
                name: Some("探す<br>見る".to_string()),
                tasks: vec![
                    task("Titles", 1, &["Me", "山田 太郎"]),
                    task("title", 5, &[""]),
                    task("sections", 3, &[]),
                ],
            }],
            preserved_lines: Vec::new(),
        };
        assert_eq!(document.structure_error(), None);
        let emitted = emit_journey(&document);
        assert_eq!(parse(&emitted), document, "{emitted}");
        assert_eq!(model_reading(&document), merman_reading(&emitted));
        assert_renders(&emitted);
    }

    #[test]
    fn an_empty_fence_opens_as_a_blank_journey() {
        assert_eq!(parse(""), JourneyDocument::empty());
        assert_eq!(parse("journey"), JourneyDocument::empty());
        assert_eq!(emit_journey(&JourneyDocument::empty()), "journey");
        assert_renders("journey");
    }

    #[test]
    fn crlf_and_trailing_whitespace_are_tolerated() {
        let source = "journey\r\n    title 一日  \r\n    A: 5: 私  \r\n";
        let document = parse(source);
        assert_eq!(document.title.as_deref(), Some("一日"));
        assert_eq!(
            emit_journey(&document),
            "journey\n    title 一日\n    A: 5: 私"
        );
    }

    #[test]
    fn the_keyword_is_recognized_the_way_merman_recognizes_it() {
        for source in ["journey\n    title x", "%% メモ\njourney"] {
            assert!(
                is_journey_source(source),
                "{source} should be a user journey"
            );
        }
        for source in [
            "journeys\n  title x",
            "Journey\n  title x",
            "",
            "%% journey",
            "flowchart TD\n    A --> B",
        ] {
            assert!(
                !is_journey_source(source),
                "{source} should not be a user journey"
            );
        }
        assert!(parse_journey("gantt\n    A :a, 2026-01-01, 1d").is_err());
    }

    /// Every user journey in `merman`'s own fixtures either opens, reads the
    /// way `merman` reads it and comes back drawing the same journey, or is
    /// refused with a reason.
    #[test]
    fn merman_fixtures_open_and_round_trip() {
        let fixtures = merman_fixtures("journey");
        if fixtures.is_empty() {
            eprintln!("merman's fixtures were not found; skipping");
            return;
        }
        let mut opened = 0;
        let mut refused = Vec::new();
        for (path, source) in &fixtures {
            match parse_journey(source) {
                Ok(document) => {
                    opened += 1;
                    let emitted = emit_journey(&document);
                    let again = parse(&emitted);
                    assert_eq!(again, document, "{path}: {emitted}");
                    assert_eq!(emit_journey(&again), emitted, "{path}");
                    let reading = merman_reading(source);
                    let (title, tasks) = model_reading(&document);
                    assert_eq!(tasks, reading.1, "{path}");
                    // `merman` reads a frontmatter title into the same field.
                    if title.is_some() {
                        assert_eq!(title, reading.0, "{path}");
                    }
                    assert_eq!(merman_reading(&emitted), reading, "{path}: {emitted}");
                    assert_renders(&emitted);
                }
                Err(error) => refused.push(format!("{path}: {error}")),
            }
        }
        eprintln!("opened {opened} of {}, refused:", fixtures.len());
        for reason in &refused {
            eprintln!("  {reason}");
        }
        assert!(opened > 0);
    }
}
