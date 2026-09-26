//! A round-trippable editing model for Mermaid timelines.
//!
//! The shape of this module mirrors [`crate::gantt_model`]: [`parse_timeline`]
//! reads the body of a ```` ```mermaid ```` fence into [`TimelineDocument`], and
//! [`emit_timeline`] writes it back. See [`crate::mermaid_model`] for the
//! invariants every diagram kind keeps.
//!
//! `merman` reads a timeline line by line with a handful of rules that decide
//! what text can go where, and this module reads it the same way:
//!
//! - A period runs to the first `:` or `#`. What follows a `#` is dropped as a
//!   comment, so a period line with one in it is refused rather than written
//!   back without the events after it.
//! - Events are separated by a `:` *followed by whitespace*, so `10:30` is one
//!   event while `a: b` is two. An event that ends in `:` or holds `: ` cannot
//!   be written back as one event and is reported instead.
//! - A line that starts with `:` adds its events to the last period written
//!   before it, whichever section that is in. The model carries the events on
//!   that period and writes them on its own line.
//! - A section name runs to the first `:`; a name with one in it is refused.
//! - Periods written before the first `section` belong to no section, and
//!   Mermaid only draws them while there is no section at all.
//!
//! What the parser rewrites, all of it stable from the first emit: the
//! indentation, the whitespace around a value, the case of a keyword,
//! continuation lines (joined onto their period's line), and comment lines in
//! the middle of the body, which move below it as they do in a gantt chart.

use anyhow::{Result, anyhow, bail};
use merman::diagram::RenderSemanticModel;
use merman::{Engine, ParseOptions};

use crate::flowchart_model::{PreservedLine, PreservedPosition};
use crate::mermaid_model::{INDENT, first_statement_line, join_lines, keyword_tail, scan_preamble};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelinePeriod {
    /// The text before the first `:`, e.g. `2024`.
    pub label: String,
    /// What happened in the period, each drawn in a box of its own below it.
    pub events: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineSection {
    /// `None` for the periods written before the first `section` line. Only the
    /// first section can be nameless: a later one would run into the section
    /// above it.
    pub name: Option<String>,
    pub periods: Vec<TimelinePeriod>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineDocument {
    /// The title, without the `title` keyword. Mermaid draws no title for a
    /// `title` statement with nothing after it, so an empty title is `None`.
    pub title: Option<String>,
    pub sections: Vec<TimelineSection>,
    pub preserved_lines: Vec<PreservedLine>,
}

impl TimelineDocument {
    /// A blank timeline, used when the editor is opened on an empty fence.
    pub fn empty() -> Self {
        Self {
            title: None,
            sections: Vec::new(),
            preserved_lines: Vec::new(),
        }
    }

    /// Every period, in the order the fence writes them.
    pub fn periods(&self) -> impl Iterator<Item = &TimelinePeriod> {
        self.sections.iter().flat_map(|section| &section.periods)
    }

    /// Why the timeline cannot be written back as a diagram Mermaid draws the
    /// way the form shows it, in Japanese, or `None` when it can.
    pub fn structure_error(&self) -> Option<String> {
        if let Some(title) = &self.title
            && !is_valid_timeline_title(title)
        {
            return Some("タイトルに改行は使えません (前後の空白だけにもできません)。".to_string());
        }

        let has_named_section = self.sections.iter().any(|section| section.name.is_some());
        let mut names: Vec<&str> = Vec::new();
        for (index, section) in self.sections.iter().enumerate() {
            match &section.name {
                Some(name) if !is_valid_timeline_section_name(name) => {
                    return Some(format!(
                        "セクション名 `{name}` は使えません (`:` と改行は不可、空にもできません)。"
                    ));
                }
                Some(name) => {
                    // Mermaid draws a section's periods by looking them up by
                    // name, so two sections with one name both get every period
                    // of either.
                    if names.contains(&name.as_str()) {
                        return Some(format!(
                            "セクション名 `{name}` が 2 つあります。Mermaid は同じ名前のセクションにどちらの期間も描くので、名前を変えてください。"
                        ));
                    }
                    names.push(name);
                }
                None if index > 0 => {
                    return Some(
                        "2 つ目以降のセクションには名前が必要です (名前が無いと 1 つ上のセクションと 1 つになります)。"
                            .to_string(),
                    );
                }
                None if has_named_section && !section.periods.is_empty() => {
                    return Some(
                        "最初のセクションより前の期間は、セクションがあると描かれません。名前を付けてセクションにするか、期間を下のセクションへ移してください。"
                            .to_string(),
                    );
                }
                None => {}
            }
        }

        for period in self.periods() {
            if !is_valid_timeline_period(&period.label) {
                return Some(format!(
                    "期間 `{}` は書けません (`:` `#` と改行は不可、空にもできません。`title` `section` `accTitle` `accDescr` `%%` で始めることもできません)。",
                    period.label
                ));
            }
            for event in &period.events {
                if !is_valid_timeline_event(event) {
                    return Some(format!(
                        "期間 `{}` の出来事 `{event}` は書けません (`:` のすぐ後に空白を続けることと末尾の `:`、改行は不可、空にもできません)。",
                        period.label
                    ));
                }
            }
        }
        None
    }
}

/// Whether `source` is a timeline. Cheap enough to call before parsing.
pub fn is_timeline_source(source: &str) -> bool {
    first_statement_line(source).is_some_and(|line| keyword_tail(line, "timeline").is_some())
}

/// Reads `source` — the body of a ```` ```mermaid ```` fence — into an editable
/// document.
pub fn parse_timeline(source: &str) -> Result<TimelineDocument> {
    let document = scan_timeline(source)?;

    // `merman` is asked last, so that a construct this module knows about is
    // reported with the reason that says what to do.
    if !source.trim().is_empty() {
        let engine = Engine::new();
        let parsed = engine
            .parse_diagram_for_render_model_sync(source, ParseOptions::strict())
            .map_err(|error| {
                anyhow!("Mermaid の構文として読めませんでした ({error})。テキストのまま編集してください。")
            })?
            .ok_or_else(|| anyhow!("Mermaid の図として解釈できませんでした。"))?;
        if !matches!(parsed.model, RenderSemanticModel::Timeline(_)) {
            bail!("この図はタイムラインではありません。");
        }
    }

    Ok(document)
}

fn scan_timeline(source: &str) -> Result<TimelineDocument> {
    let lines = source.lines().collect::<Vec<_>>();
    let preamble = scan_preamble(&lines);
    let mut document = TimelineDocument::empty();
    document.preserved_lines = preamble.preserved;

    let Some(header_index) = preamble.header else {
        return Ok(document);
    };

    let header = lines.get(header_index).copied().unwrap_or_default().trim();
    let Some(tail) = keyword_tail(header, "timeline") else {
        bail!("タイムラインではありません (`timeline` で始まる図だけを構造化編集できます)。");
    };
    if !tail.is_empty() {
        bail!(
            "`timeline` の行の `{tail}` は構造化編集に対応していません。テキストのまま編集してください。"
        );
    }

    let mut index = header_index + 1;
    let mut position = PreservedPosition::Top;
    while let Some(line) = lines.get(index).copied() {
        let start = index;
        index += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Mermaid's comment pass only removes a `%%` with something after
        // it, so a bare one reaches the timeline parser as a period.
        if line.trim_start() == "%%" {
            bail!(
                "中身の無い `%%` の行は Mermaid が期間として描くので、構造化編集に対応していません。消すかテキストのまま編集してください。"
            );
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
            document.preserved_lines.push(PreservedLine {
                position,
                text: join_lines(lines.get(start..index).unwrap_or_default()),
            });
            continue;
        }

        // Mermaid reads a statement from its first non-blank column and keeps
        // the whitespace at its end, which matters to where an event ends.
        let statement = line.trim_start();
        match classify(statement) {
            Statement::Comment => {
                // Mermaid turns `#name;` into an entity before it reads the
                // line, so such a line is not the comment it looks like.
                if starts_with_entity(statement) {
                    bail!(
                        "`{trimmed}` は Mermaid が文字参照として読むので、コメントとして残せません。テキストのまま編集してください。"
                    );
                }
                document.preserved_lines.push(PreservedLine {
                    position,
                    text: line.trim_end().to_string(),
                });
            }
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
                let value = value.trim();
                // Mermaid keeps the last one, so a second statement — even an
                // empty one, which clears the title — would make writing the
                // timeline back drop a line the user can see.
                if document.title.is_some() {
                    bail!(
                        "`title` が 2 回書かれています。1 つにまとめてからもう一度開いてください。"
                    );
                }
                if !value.is_empty() {
                    document.title = Some(value.to_string());
                }
            }
            Statement::Section(value) => {
                if value.contains(':') {
                    bail!(
                        "セクション名の `:` から先は Mermaid が読み飛ばします ({trimmed})。`:` を消すか、テキストのまま編集してください。"
                    );
                }
                let name = value.trim();
                if name.is_empty() {
                    bail!(
                        "名前の無い `section` は構造化編集に対応していません ({trimmed})。名前を付けるか、テキストのまま編集してください。"
                    );
                }
                document.sections.push(TimelineSection {
                    name: Some(name.to_string()),
                    periods: Vec::new(),
                });
                position = PreservedPosition::Bottom;
            }
            Statement::Events(text) => {
                let events = split_events(text, trimmed)?;
                let Some(period) = document
                    .sections
                    .iter_mut()
                    .rev()
                    .find_map(|section| section.periods.last_mut())
                else {
                    bail!("出来事 `{trimmed}` の前に期間がありません。");
                };
                period.events.extend(events);
            }
            Statement::Period => {
                let period = parse_period(statement, trimmed)?;
                if document.sections.is_empty() {
                    document.sections.push(TimelineSection {
                        name: None,
                        periods: Vec::new(),
                    });
                }
                if let Some(section) = document.sections.last_mut() {
                    section.periods.push(period);
                }
                position = PreservedPosition::Bottom;
            }
        }
    }

    Ok(document)
}

/// What one line of a timeline body is, decided in the order `merman`'s
/// timeline parser tries the possibilities.
#[derive(Debug, PartialEq, Eq)]
enum Statement<'a> {
    Comment,
    Title(&'a str),
    Accessibility { block: bool },
    Section(&'a str),
    Events(&'a str),
    Period,
}

fn classify(statement: &str) -> Statement<'_> {
    if statement.starts_with('#') {
        return Statement::Comment;
    }
    if let Some(value) = keyword_argument(statement, "title") {
        return Statement::Title(value);
    }
    if let Some(block) = accessibility_kind(statement) {
        return Statement::Accessibility { block };
    }
    if let Some(value) = keyword_argument(statement, "section") {
        return Statement::Section(value);
    }
    if statement.starts_with(':') {
        return Statement::Events(statement);
    }
    Statement::Period
}

/// What follows `keyword` at the start of `line`, the way `merman`'s timeline
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

/// `get` rather than slicing, because a line that opens with a multibyte label
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

/// Whether `text` opens with `#` + word characters + `;`, which Mermaid
/// replaces with a placeholder before the timeline parser sees it.
fn starts_with_entity(text: &str) -> bool {
    let Some(rest) = text.strip_prefix('#') else {
        return false;
    };
    let word = rest
        .bytes()
        .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        .count();
    word > 0 && rest.as_bytes().get(word) == Some(&b';')
}

fn parse_period(statement: &str, trimmed: &str) -> Result<TimelinePeriod> {
    let end = statement.find([':', '#']).unwrap_or(statement.len());
    let label = statement.get(..end).unwrap_or_default().trim();
    let rest = statement.get(end..).unwrap_or_default();
    if rest.starts_with('#') {
        bail!(
            "期間の `#` から先は Mermaid がコメントとして読み飛ばします ({trimmed})。`#` を消すか、テキストのまま編集してください。"
        );
    }
    let events = if rest.is_empty() {
        Vec::new()
    } else {
        split_events(rest, trimmed)?
    };
    Ok(TimelinePeriod {
        label: label.to_string(),
        events,
    })
}

/// Splits `: a : b` into its events the way `merman` does: each event starts
/// after a `:` and one whitespace character, and runs to the next `:` that is
/// followed by whitespace.
fn split_events(input: &str, trimmed: &str) -> Result<Vec<String>> {
    let mut rest = input;
    let mut events = Vec::new();
    while !rest.is_empty() {
        let Some(after_colon) = rest.strip_prefix(':') else {
            bail!("出来事として読めませんでした ({trimmed})。");
        };
        let Some(space) = after_colon
            .chars()
            .next()
            .filter(|character| character.is_whitespace())
        else {
            bail!("`:` のあとに空白が必要です ({trimmed})。");
        };
        let body = after_colon.get(space.len_utf8()..).unwrap_or_default();
        let boundary = body.char_indices().find_map(|(offset, character)| {
            (character == ':'
                && body
                    .get(offset + 1..)
                    .and_then(|after| after.chars().next())
                    .is_some_and(char::is_whitespace))
            .then_some(offset)
        });
        let (event, next) = match boundary {
            Some(offset) => (
                body.get(..offset).unwrap_or_default(),
                body.get(offset..).unwrap_or_default(),
            ),
            None => (body, ""),
        };
        let event = event.trim();
        if event.is_empty() {
            bail!("空の出来事があります ({trimmed})。消すか、テキストのまま編集してください。");
        }
        events.push(event.to_string());
        rest = next;
    }
    Ok(events)
}

/// Writes `document` back as Mermaid source, without a trailing newline.
pub fn emit_timeline(document: &TimelineDocument) -> String {
    let mut out = String::new();

    for line in preserved_at(document, PreservedPosition::Preamble) {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("timeline\n");
    if let Some(title) = &document.title {
        out.push_str(&format!("{INDENT}title {title}\n"));
    }
    for line in preserved_at(document, PreservedPosition::Top) {
        out.push_str(line);
        out.push('\n');
    }

    for section in &document.sections {
        if let Some(name) = &section.name {
            out.push_str(&format!("{INDENT}section {name}\n"));
        }
        for period in &section.periods {
            out.push_str(INDENT);
            out.push_str(&period.label);
            for event in &period.events {
                out.push_str(" : ");
                out.push_str(event);
            }
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

fn preserved_at(
    document: &TimelineDocument,
    position: PreservedPosition,
) -> impl Iterator<Item = &str> {
    document
        .preserved_lines
        .iter()
        .filter(move |line| line.position == position)
        .map(|line| line.text.as_str())
}

/// Whether `title` can be written after `title`. It runs to the end of the
/// line, so only a line break is in the way.
pub fn is_valid_timeline_title(title: &str) -> bool {
    !title.trim().is_empty() && title.trim() == title && !title.contains(['\n', '\r'])
}

/// Whether `name` can be written after `section`, which Mermaid cuts at the
/// first `:`.
pub fn is_valid_timeline_section_name(name: &str) -> bool {
    !name.is_empty() && name.trim() == name && !name.contains([':', '\n', '\r'])
}

/// Whether `label` reads back as a period rather than as a statement of some
/// other kind or as a period cut short.
///
/// The line is also classified with an event after it, because `title` and
/// `section` only become keywords once whitespace follows them, and `accTitle`
/// once a `:` does.
pub fn is_valid_timeline_period(label: &str) -> bool {
    !label.is_empty()
        && label.trim() == label
        && !label.contains([':', '#', '\n', '\r'])
        && !label.starts_with("%%")
        && classify(label) == Statement::Period
        && classify(&format!("{label} : x")) == Statement::Period
}

/// Whether `event` reads back as the one event it is.
pub fn is_valid_timeline_event(event: &str) -> bool {
    !event.is_empty()
        && event.trim() == event
        && !event.contains(['\n', '\r'])
        && !event.ends_with(':')
        && !event.char_indices().any(|(offset, character)| {
            character == ':'
                && event
                    .get(offset + 1..)
                    .and_then(|after| after.chars().next())
                    .is_some_and(char::is_whitespace)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MermaidTheme, render_to_svg};

    const TEMPLATE: &str =
        "timeline\n    title 沿革\n    2024 : 企画\n    2025 : 開発 : 公開\n    2026 : 運用";

    fn parse(source: &str) -> TimelineDocument {
        parse_timeline(source).unwrap_or_else(|error| panic!("{source} should parse: {error}"))
    }

    /// Emitting must reach a fixed point after one pass, and the text is what
    /// is compared, so a line that vanished from both documents still fails.
    fn assert_stable(source: &str) -> String {
        let first = parse(source);
        let first_text = emit_timeline(&first);
        let second = parse(&first_text);
        let second_text = emit_timeline(&second);
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

    fn period(label: &str, events: &[&str]) -> TimelinePeriod {
        TimelinePeriod {
            label: label.to_string(),
            events: events.iter().map(|event| event.to_string()).collect(),
        }
    }

    #[test]
    fn the_insert_template_round_trips_byte_for_byte() {
        let document = parse(TEMPLATE);
        assert_eq!(document.title.as_deref(), Some("沿革"));
        assert_eq!(
            document.sections,
            vec![TimelineSection {
                name: None,
                periods: vec![
                    period("2024", &["企画"]),
                    period("2025", &["開発", "公開"]),
                    period("2026", &["運用"]),
                ],
            }]
        );
        assert_eq!(emit_timeline(&document), TEMPLATE);
        assert_eq!(document.structure_error(), None);
        assert_renders(TEMPLATE);
    }

    #[test]
    fn sections_continuations_and_colons_inside_events_survive() {
        let source = "timeline\n    title 歴史\n    section 古代\n    紀元前 : 文字 : 暦\n          : 車輪\n    section 近代\n    1800年代 : 蒸気機関\n    1900年代\n    : 10:30 の出来事\n    2000年代 : a:b";
        let document = parse(source);
        assert_eq!(
            document.sections,
            vec![
                TimelineSection {
                    name: Some("古代".to_string()),
                    periods: vec![period("紀元前", &["文字", "暦", "車輪"])],
                },
                TimelineSection {
                    name: Some("近代".to_string()),
                    periods: vec![
                        period("1800年代", &["蒸気機関"]),
                        period("1900年代", &["10:30 の出来事"]),
                        period("2000年代", &["a:b"]),
                    ],
                },
            ]
        );
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "timeline\n    title 歴史\n    section 古代\n    紀元前 : 文字 : 暦 : 車輪\n    section 近代\n    1800年代 : 蒸気機関\n    1900年代 : 10:30 の出来事\n    2000年代 : a:b"
        );
        assert_renders(&emitted);
    }

    #[test]
    fn a_continuation_after_a_section_line_joins_the_period_before_it() {
        // `merman` adds the events to the last period it read, which is in the
        // section above; writing them on that period's line keeps them there.
        let source = "timeline\n    section A\n    2020 : a\n    section B\n    : b\n    2021 : c";
        let document = parse(source);
        assert_eq!(
            document
                .sections
                .first()
                .and_then(|section| section.periods.first()),
            Some(&period("2020", &["a", "b"]))
        );
        assert_stable(source);
    }

    #[test]
    fn comments_accessibility_and_the_preamble_keep_their_place() {
        let source = "---\ntitle: 図\n---\n%%{init: {\"theme\": \"dark\"}}%%\ntimeline\n    %% 先頭のメモ\n    accTitle: 概要\n    title 沿革\n    # シャープのコメント\n    2024 : 企画\n    accDescr {\n      説明の 1 行目\n      2 行目\n    }\n    %% 途中のメモ\n    2025 : 開発";
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
            "---\ntitle: 図\n---\n%%{init: {\"theme\": \"dark\"}}%%\ntimeline\n    title 沿革\n    %% 先頭のメモ\n    accTitle: 概要\n    # シャープのコメント\n    2024 : 企画\n    2025 : 開発\n    accDescr {\n      説明の 1 行目\n      2 行目\n    }\n    %% 途中のメモ"
        );
        assert_renders(&emitted);
    }

    #[test]
    fn constructs_that_would_be_dropped_are_refused() {
        for (source, expected) in [
            ("timeline\n    title a\n    title b", "2 回"),
            ("timeline\n    title a\n    title ", "2 回"),
            (
                "timeline\n    section 前半: 補足\n    2024 : a",
                "`:` から先",
            ),
            ("timeline\n    section \n    2024 : a", "名前の無い"),
            ("timeline\n    2024 # メモ : a", "`#` から先"),
            ("timeline\n    2024 : a : \n", "空の出来事"),
            ("timeline\n    : a", "前に期間"),
            ("timeline\n    #x; 何か", "文字参照"),
            ("timeline LR\n    2024 : a", "`timeline` の行"),
        ] {
            let error = parse_timeline(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            let message = format!("{error}");
            assert!(
                message.contains(expected),
                "the reason for `{source}` should mention {expected}: {message}"
            );
        }
        // What `merman` itself cannot read is refused with its reason.
        assert!(parse_timeline("timeline\n    2024 :a").is_err());
    }

    #[test]
    fn periods_the_drawing_would_leave_out_are_reported() {
        let document = parse("timeline\n    2023 : 前\n    section A\n    2024 : a");
        let error = document.structure_error().expect("the first band is lost");
        assert!(error.contains("描かれません"), "{error}");

        let document = parse("timeline\n    section A\n    2024 : a\n    section A\n    2025 : b");
        let error = document.structure_error().expect("the name is repeated");
        assert!(error.contains("2 つ"), "{error}");

        let mut document = parse(TEMPLATE);
        if let Some(section) = document.sections.get_mut(0) {
            section.name = Some("名前".to_string());
        }
        assert_eq!(document.structure_error(), None);
        document.sections.push(TimelineSection {
            name: None,
            periods: Vec::new(),
        });
        assert!(document.structure_error().is_some());
    }

    #[test]
    fn values_are_validated() {
        for label in ["2024", "紀元前", "Q1 (1-3月)", "Titles", "sections"] {
            assert!(is_valid_timeline_period(label), "{label}");
        }
        for label in [
            "",
            " 2024",
            "a:b",
            "C#",
            "title x",
            "title",
            "Section",
            "accTitle",
            "section x",
            "accTitle: x",
            "accDescr {",
            "%% x",
            "改行\nあり",
        ] {
            assert!(!is_valid_timeline_period(label), "{label}");
        }
        for event in ["企画", "10:30", "a:b", "C# の登場", "50%"] {
            assert!(is_valid_timeline_event(event), "{event}");
        }
        for event in ["", " a", "a: b", "a:", "改行\nあり"] {
            assert!(!is_valid_timeline_event(event), "{event}");
        }
        assert!(is_valid_timeline_section_name("古代"));
        assert!(!is_valid_timeline_section_name("a:b"));
        assert!(!is_valid_timeline_section_name(""));
        assert!(is_valid_timeline_title("沿革: 概要"));
        assert!(!is_valid_timeline_title(" "));

        // Everything the predicates accept has to come back as itself.
        let document = TimelineDocument {
            title: Some("沿革: 概要".to_string()),
            sections: vec![TimelineSection {
                name: Some("古代".to_string()),
                periods: vec![
                    period("Q1 (1-3月)", &["10:30", "C# の登場", "50%"]),
                    period("sections", &[]),
                    period("Titles", &["a:b"]),
                ],
            }],
            preserved_lines: Vec::new(),
        };
        assert_eq!(document.structure_error(), None);
        let emitted = emit_timeline(&document);
        assert_eq!(parse(&emitted), document, "{emitted}");
        assert_renders(&emitted);
    }

    #[test]
    fn an_empty_fence_opens_as_a_blank_timeline() {
        assert_eq!(parse(""), TimelineDocument::empty());
        assert_eq!(parse("timeline"), TimelineDocument::empty());
        assert_eq!(emit_timeline(&TimelineDocument::empty()), "timeline");
    }

    #[test]
    fn other_diagram_types_are_not_timelines() {
        assert!(is_timeline_source("timeline\n    2024 : a"));
        assert!(!is_timeline_source("timelines\n    2024 : a"));
        assert!(!is_timeline_source("gantt\n    A :a, 2026-01-01, 1d"));
        assert!(parse_timeline("gantt\n    A :a, 2026-01-01, 1d").is_err());
    }

    #[test]
    fn crlf_and_trailing_whitespace_are_tolerated() {
        let source = "timeline\r\n    title 沿革  \r\n    2024 : 企画  \r\n";
        let document = parse(source);
        assert_eq!(document.title.as_deref(), Some("沿革"));
        assert_eq!(
            emit_timeline(&document),
            "timeline\n    title 沿革\n    2024 : 企画"
        );
    }

    /// Mermaid drops `%%` lines only when something follows the marker, so a
    /// bare `%%` is a period named `%%`: preserving it as a comment would move
    /// it below the body and hand the events after it to the period above.
    #[test]
    fn a_bare_comment_marker_is_refused() {
        for source in [
            "timeline\n    2024 : a\n    %%\n    : b",
            "timeline\n    section A\n    %%\n    2024 : a",
        ] {
            let error = parse_timeline(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            assert!(format!("{error}").contains("`%%`"), "{error}");
        }
        let with_text = "timeline\n    2024 : a\n    %% メモ\n    2025 : b";
        assert_eq!(
            emit_timeline(&parse(with_text)),
            "timeline\n    2024 : a\n    2025 : b\n    %% メモ"
        );
    }
}
