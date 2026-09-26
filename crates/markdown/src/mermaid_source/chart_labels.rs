//! Locates the labels the preview maps as a [`LabelItem`] rather than as a node
//! or an edge: a pie chart's title and legend entries, a gantt chart's title,
//! sections and tasks, and the attribute names of an ER entity.
//!
//! Each scanner follows merman's own parser for its kind line by line, because
//! the thing a label is keyed by — the position of a slice among the ones merman
//! keeps, a task's id, an attribute's position in its entity — only means
//! something if it is counted the way merman counts it. Anything that makes
//! that count uncertain is a refusal: rewriting another element's label would
//! be worse than not rewriting at all.

use std::ops::Range;

use mermaid_render::LabelItem;
use mermaid_render::er_model::is_valid_attribute_word;
use mermaid_render::gantt_model::{
    is_valid_gantt_label, is_valid_gantt_section_name, is_valid_gantt_title,
};
use mermaid_render::mermaid_model::{MermaidDiagramKind, detect_kind};
use mermaid_render::pie_model::{is_valid_pie_label, is_valid_pie_title};

use super::{
    MermaidLabelLookup, MermaidLabelQuoting, MermaidLabelRefusal, MermaidLabelSpan, checked,
    read_er_name, strip_comment,
};

/// The token `item` is drawn with, where `rendered` is the text the diagram
/// shows for it. The rendered text is compared with what the source says, so
/// that a count that has drifted from merman's is caught rather than acted on.
pub(crate) fn find_item_label(
    fence_body: &str,
    item: &LabelItem,
    rendered: &str,
) -> MermaidLabelLookup {
    match (detect_kind(fence_body), item) {
        (Some(MermaidDiagramKind::Pie), LabelItem::Title) => {
            let scan = scan_pie(fence_body).ok_or(MermaidLabelRefusal::NotLocated)?;
            checked(
                fence_body,
                scan.title.ok_or(MermaidLabelRefusal::NotLocated)?,
            )
        }
        (Some(MermaidDiagramKind::Pie), LabelItem::PieSlice { index }) => {
            find_pie_slice(fence_body, *index, rendered)
        }
        (Some(MermaidDiagramKind::Gantt), LabelItem::Title) => {
            let scan = scan_gantt(fence_body).ok_or(MermaidLabelRefusal::NotLocated)?;
            checked(
                fence_body,
                scan.title.ok_or(MermaidLabelRefusal::NotLocated)?,
            )
        }
        (Some(MermaidDiagramKind::Gantt), LabelItem::GanttSection { name }) => {
            find_gantt_section(fence_body, name)
        }
        (Some(MermaidDiagramKind::Gantt), LabelItem::GanttTask { task_id }) => {
            find_gantt_task(fence_body, task_id, rendered)
        }
        (Some(MermaidDiagramKind::Er), LabelItem::ErAttribute { entity, index }) => {
            find_er_attribute(fence_body, entity, *index, rendered)
        }
        _ => Err(MermaidLabelRefusal::NotLocated),
    }
}

/// The labels a new label for `item` must not take, because merman would then
/// draw the two elements as one — it keeps only the first pie slice of a label,
/// and draws gantt sections of the same name under one title.
pub(crate) fn reserved_labels(fence_body: &str, item: &LabelItem) -> Vec<String> {
    match item {
        LabelItem::PieSlice { index } => {
            let Some(scan) = scan_pie(fence_body) else {
                return Vec::new();
            };
            let drawn = drawn_slices(&scan.slices);
            let own = drawn.get(*index).map(|span| span.label.clone());
            let mut reserved: Vec<String> = Vec::new();
            for slice in &scan.slices {
                if Some(&slice.label) != own.as_ref() && !reserved.contains(&slice.label) {
                    reserved.push(slice.label.clone());
                }
            }
            reserved
        }
        LabelItem::GanttSection { name } => {
            let Some(scan) = scan_gantt(fence_body) else {
                return Vec::new();
            };
            let mut reserved: Vec<String> = Vec::new();
            for (drawn_name, span) in &scan.sections {
                if drawn_name != name && !reserved.contains(&span.label) {
                    reserved.push(span.label.clone());
                }
            }
            reserved
        }
        _ => Vec::new(),
    }
}

/// Writes a label into one of the slots this module finds.
pub(super) fn encode(label: &str, quoting: MermaidLabelQuoting) -> String {
    let label = label.trim();
    match quoting {
        MermaidLabelQuoting::PieLabel => {
            format!("\"{}\"", label.replace('\\', "\\\\").replace('"', "\\\""))
        }
        _ => label.to_string(),
    }
}

/// Why `new_label` cannot be written into a slot this module finds, or `None`
/// when it can, or when the slot is not one of this module's.
pub(crate) fn write_refusal(
    quoting: MermaidLabelQuoting,
    new_label: &str,
    reserved: &[String],
) -> Option<&'static str> {
    let flattened: String = new_label
        .chars()
        .map(|character| {
            if character == '\n' || character == '\r' {
                ' '
            } else {
                character
            }
        })
        .collect();
    let label = flattened.trim();
    match quoting {
        MermaidLabelQuoting::PieLabel => {
            if !is_valid_pie_label(label) {
                return Some("円グラフの項目名は空にできず、`%%` も書けません。");
            }
            reserved.iter().any(|taken| taken == label).then_some(
                "同じ名前の項目がすでにあります。円グラフは同じ名前の 2 つ目を描かないため、別の名前にしてください。",
            )
        }
        MermaidLabelQuoting::PieTitle => (!is_valid_pie_title(label))
            .then_some("円グラフのタイトルには `%%` を書けません。"),
        MermaidLabelQuoting::GanttTask => (!is_valid_gantt_label(label)).then_some(
            "タスク名は空にできず、`:` を含めたり `title` や `section` などのキーワードで始めたりできません。",
        ),
        MermaidLabelQuoting::GanttSection => {
            if !is_valid_gantt_section_name(label) {
                return Some("セクション名は空にできません。");
            }
            reserved.iter().any(|taken| taken == label).then_some(
                "同じ名前のセクションがすでにあります。同じ名前のセクションは 1 つにまとめて描かれるため、別の名前にしてください。",
            )
        }
        MermaidLabelQuoting::GanttTitle => (!is_valid_gantt_title(label))
            .then_some("ガントチャートのタイトルは空にできません。"),
        MermaidLabelQuoting::ErAttributeName => (!is_valid_attribute_word(label)).then_some(
            "属性名に使えない文字が含まれています。空白・`,`・`\"`・`{`・`}`・`#`・`%`・`;` と、`PK` などのキー名は使えません。",
        ),
        _ => None,
    }
}

/// The lines of a fence body with their byte offsets, leaving out YAML
/// frontmatter and the continuation lines of a `%%{…}%%` directive, which
/// merman strips before any diagram's parser sees the text.
fn body_lines(fence_body: &str) -> Vec<(usize, &str)> {
    let mut lines = Vec::new();
    let mut offset = 0;
    let mut seen_content = false;
    let mut in_frontmatter = false;
    let mut in_directive = false;
    for raw in fence_body.split_inclusive('\n') {
        let line = raw.trim_end_matches(['\n', '\r']);
        let line_offset = offset;
        offset += raw.len();
        let trimmed = line.trim();
        if in_frontmatter {
            if trimmed == "---" {
                in_frontmatter = false;
            }
            continue;
        }
        if in_directive {
            if trimmed.contains("}%%") {
                in_directive = false;
            }
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        if !seen_content && trimmed == "---" {
            seen_content = true;
            in_frontmatter = true;
            continue;
        }
        seen_content = true;
        if trimmed.starts_with("%%{") && !trimmed.contains("}%%") {
            in_directive = true;
            continue;
        }
        lines.push((line_offset, line));
    }
    lines
}

/// `text` with its surrounding whitespace removed, and the offset of what is
/// left relative to `text`.
fn trimmed_with_offset(text: &str) -> (usize, &str) {
    let start = text.len() - text.trim_start().len();
    (start, text.trim())
}

fn span(
    offset: usize,
    text: &str,
    label: String,
    quoting: MermaidLabelQuoting,
) -> MermaidLabelSpan {
    MermaidLabelSpan {
        range: offset..(offset + text.len()),
        label,
        quoting,
    }
}

struct PieScan {
    title: Option<MermaidLabelSpan>,
    /// Every slice statement in source order, including the ones merman drops
    /// because an earlier slice has the same label.
    slices: Vec<MermaidLabelSpan>,
}

/// merman cuts every pie chart line at its first `%%`, inside a quoted label or
/// not.
fn strip_pie_comment(line: &str) -> &str {
    match line.find("%%") {
        Some(index) => &line[..index],
        None => line,
    }
}

/// Reads a pie chart the way merman does, or `None` for one merman refuses.
fn scan_pie(fence_body: &str) -> Option<PieScan> {
    let lines = body_lines(fence_body);
    let mut scan = PieScan {
        title: None,
        slices: Vec::new(),
    };
    let mut index = 0;
    let mut header = None;
    while let Some((offset, line)) = lines.get(index).copied() {
        index += 1;
        let (start, statement) = trimmed_with_offset(strip_pie_comment(line));
        if !statement.is_empty() {
            header = Some((offset + start, statement));
            break;
        }
    }
    let (header_offset, header) = header?;
    let mut rest = header.strip_prefix("pie")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    loop {
        let trimmed = rest.trim_start();
        let rest_offset = header_offset + header.len() - trimmed.len();
        if trimmed.is_empty() {
            break;
        }
        if let Some(after) = word_tail(trimmed, "showData") {
            rest = after;
            continue;
        }
        if let Some(after) = word_tail(trimmed, "title") {
            let (start, title) = trimmed_with_offset(after);
            let title_offset = rest_offset + (trimmed.len() - after.len()) + start;
            scan.title = Some(span(
                title_offset,
                title,
                title.to_string(),
                MermaidLabelQuoting::PieTitle,
            ));
            break;
        }
        if key_colon(trimmed, "accTitle") || key_colon(trimmed, "accDescr") {
            break;
        }
        if accessibility_block_opens(trimmed) {
            if !trimmed.contains('}') {
                skip_accessibility_block(&lines, &mut index, strip_pie_comment);
            }
            break;
        }
        return None;
    }

    while let Some((offset, line)) = lines.get(index).copied() {
        index += 1;
        let (start, statement) = trimmed_with_offset(strip_pie_comment(line));
        let statement_offset = offset + start;
        if statement.is_empty() {
            continue;
        }
        if let Some(after) = statement.strip_prefix("title")
            && (after.is_empty() || after.starts_with(char::is_whitespace))
        {
            let (title_start, title) = trimmed_with_offset(after);
            scan.title = Some(span(
                statement_offset + "title".len() + title_start,
                title,
                title.to_string(),
                MermaidLabelQuoting::PieTitle,
            ));
            continue;
        }
        if key_colon(statement, "accTitle") || key_colon(statement, "accDescr") {
            continue;
        }
        if accessibility_block_opens(statement) {
            if !statement.contains('}') {
                skip_accessibility_block(&lines, &mut index, strip_pie_comment);
            }
            continue;
        }
        scan.slices
            .push(read_pie_slice(statement_offset, statement)?);
    }
    Some(scan)
}

/// What follows `word` at the start of `text`, when `word` ends there.
fn word_tail<'a>(text: &'a str, word: &str) -> Option<&'a str> {
    let rest = text.strip_prefix(word)?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then_some(rest)
}

fn key_colon(statement: &str, key: &str) -> bool {
    statement
        .strip_prefix(key)
        .is_some_and(|rest| rest.trim_start().starts_with(':'))
}

fn accessibility_block_opens(statement: &str) -> bool {
    statement
        .strip_prefix("accDescr")
        .is_some_and(|rest| rest.trim_start().starts_with('{'))
}

/// Moves `index` past the lines of an `accDescr { … }` block whose first line
/// has already been read.
fn skip_accessibility_block(
    lines: &[(usize, &str)],
    index: &mut usize,
    strip: impl Fn(&str) -> &str,
) {
    while let Some((_, line)) = lines.get(*index).copied() {
        *index += 1;
        if strip(line).contains('}') {
            return;
        }
    }
}

/// `"label" : value`, whose label merman reads with `\` escapes and may quote
/// with `'` as well. The span covers the quotes.
fn read_pie_slice(offset: usize, statement: &str) -> Option<MermaidLabelSpan> {
    let quote = statement.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let mut label = String::new();
    let mut escaped = false;
    let mut end = None;
    for (index, character) in statement.char_indices().skip(1) {
        if escaped {
            label.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if character == quote {
            end = Some(index + character.len_utf8());
            break;
        }
        label.push(character);
    }
    let end = end?;
    let value = statement
        .get(end..)?
        .trim_start()
        .strip_prefix(':')?
        .trim_start();
    let number: String = value
        .chars()
        .take_while(|character| character.is_ascii_digit() || matches!(character, '-' | '.'))
        .collect();
    number.parse::<f64>().ok()?;
    Some(MermaidLabelSpan {
        range: offset..(offset + end),
        label,
        quoting: MermaidLabelQuoting::PieLabel,
    })
}

/// The slices merman draws: the first of each label, in source order.
fn drawn_slices(slices: &[MermaidLabelSpan]) -> Vec<&MermaidLabelSpan> {
    let mut drawn: Vec<&MermaidLabelSpan> = Vec::new();
    for slice in slices {
        if !drawn.iter().any(|kept| kept.label == slice.label) {
            drawn.push(slice);
        }
    }
    drawn
}

fn find_pie_slice(fence_body: &str, index: usize, rendered: &str) -> MermaidLabelLookup {
    let scan = scan_pie(fence_body).ok_or(MermaidLabelRefusal::NotLocated)?;
    let drawn = drawn_slices(&scan.slices);
    let slice = *drawn.get(index).ok_or(MermaidLabelRefusal::NotLocated)?;
    // `showData` draws the value after the label, in brackets.
    let matches_rendered = rendered == slice.label
        || rendered
            .strip_prefix(slice.label.as_str())
            .is_some_and(|rest| rest.starts_with(" ["));
    if !matches_rendered {
        return Err(MermaidLabelRefusal::NotLocated);
    }
    // A later slice with the same label is not drawn now, but would be the one
    // drawn once this one is renamed.
    if scan
        .slices
        .iter()
        .filter(|other| other.label == slice.label)
        .count()
        > 1
    {
        return Err(MermaidLabelRefusal::Duplicated);
    }
    checked(fence_body, slice.clone())
}

struct GanttScan {
    title: Option<MermaidLabelSpan>,
    /// Each `section` statement, with the name as merman draws it.
    sections: Vec<(String, MermaidLabelSpan)>,
    /// Each task, with the id merman gives it.
    tasks: Vec<(String, MermaidLabelSpan)>,
}

fn starts_with_ignoring_case(text: &str, prefix: &str) -> bool {
    text.get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

/// Where the argument of `keyword` starts in `statement`, when the keyword is
/// followed by the whitespace merman requires.
fn keyword_argument(statement: &str, keyword: &str) -> Option<usize> {
    if !starts_with_ignoring_case(statement, keyword) {
        return None;
    }
    let separator = statement.get(keyword.len()..)?.chars().next()?;
    separator
        .is_whitespace()
        .then(|| keyword.len() + separator.len_utf8())
}

enum GanttStatement {
    Title(Range<usize>),
    Section(Range<usize>),
    Task { label: Range<usize>, data: String },
    AccessibilityBlock,
    Other,
}

/// Classifies one gantt statement in the order merman tries its keywords, since
/// a task name that opens like a keyword is read as that keyword instead.
/// Ranges are relative to `statement`, which starts at its first non-blank.
fn classify_gantt_statement(statement: &str) -> Option<GanttStatement> {
    if keyword_argument(statement, "dateFormat").is_some() {
        return Some(GanttStatement::Other);
    }
    if starts_with_ignoring_case(statement, "inclusiveEndDates")
        || starts_with_ignoring_case(statement, "topAxis")
    {
        return Some(GanttStatement::Other);
    }
    for keyword in [
        "axisFormat",
        "tickInterval",
        "includes",
        "excludes",
        "todayMarker",
        "weekday",
        "weekend",
    ] {
        if keyword_argument(statement, keyword).is_some() {
            return Some(GanttStatement::Other);
        }
    }
    for (keyword, is_title) in [("title", true), ("section", false)] {
        if let Some(start) = keyword_argument(statement, keyword) {
            let argument = statement.get(start..)?;
            let (leading, value) = trimmed_with_offset(argument);
            let range = (start + leading)..(start + leading + value.len());
            return Some(if is_title {
                GanttStatement::Title(range)
            } else {
                GanttStatement::Section(range)
            });
        }
    }
    for key in ["accTitle", "accDescr"] {
        if starts_with_ignoring_case(statement, key)
            && statement
                .get(key.len()..)
                .is_some_and(|rest| rest.trim_start().starts_with(':'))
        {
            return Some(GanttStatement::Other);
        }
    }
    if starts_with_ignoring_case(statement, "accDescr")
        && statement
            .get("accDescr".len()..)
            .is_some_and(|rest| rest.trim_start().starts_with('{'))
    {
        return Some(if statement.contains('}') {
            GanttStatement::Other
        } else {
            GanttStatement::AccessibilityBlock
        });
    }
    if starts_with_ignoring_case(statement, "click") {
        return Some(GanttStatement::Other);
    }
    let colon = statement.find(':')?;
    let label = statement.get(..colon)?;
    let data = statement.get(colon + 1..)?;
    let data = match data.find(['#', ';']) {
        Some(end) => data.get(..end)?,
        None => data,
    };
    if label.is_empty() || data.trim().is_empty() {
        return Some(GanttStatement::Other);
    }
    Some(GanttStatement::Task {
        label: 0..label.trim_end().len(),
        data: data.to_string(),
    })
}

/// The id merman gives a task, from the fields after its `:`. Only a task
/// written with all three fields names its own id; every other one is
/// numbered, counting only the numbered ones.
fn gantt_task_id(data: &str, numbered: &mut usize) -> String {
    let mut fields: Vec<&str> = data.split(',').collect();
    const TAGS: [&str; 5] = ["active", "done", "crit", "milestone", "vert"];
    while fields
        .first()
        .is_some_and(|field| TAGS.contains(&field.trim()))
    {
        fields.remove(0);
    }
    if let [id, _, _] = fields[..] {
        return id.trim().to_string();
    }
    *numbered += 1;
    format!("task{numbered}")
}

/// A section name as merman draws it: split into lines at each `<br>`, with
/// trailing empty lines dropped.
fn gantt_drawn_name(name: &str) -> String {
    let mut lines: Vec<String> = vec![String::new()];
    let mut rest = name;
    while !rest.is_empty() {
        if starts_with_ignoring_case(rest, "<br") {
            let after = rest.get(3..).unwrap_or_default().trim_start();
            let after = after.strip_prefix('/').unwrap_or(after).trim_start();
            if let Some(after) = after.strip_prefix('>') {
                lines.push(String::new());
                rest = after;
                continue;
            }
        }
        let Some(character) = rest.chars().next() else {
            break;
        };
        if let Some(line) = lines.last_mut() {
            line.push(character);
        }
        rest = &rest[character.len_utf8()..];
    }
    while lines.len() > 1 && lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// merman only knows full-line comments in a gantt chart: a `%%` inside a
/// task line is part of the task.
fn strip_gantt_comment(line: &str) -> &str {
    if line.trim_start().starts_with("%%") {
        ""
    } else {
        line
    }
}

/// Reads a gantt chart the way merman does, or `None` for one merman refuses.
fn scan_gantt(fence_body: &str) -> Option<GanttScan> {
    let lines = body_lines(fence_body);
    let mut scan = GanttScan {
        title: None,
        sections: Vec::new(),
        tasks: Vec::new(),
    };
    let mut numbered = 0;
    let mut header_seen = false;
    let mut index = 0;
    while let Some((offset, line)) = lines.get(index).copied() {
        index += 1;
        let (start, trimmed) = trimmed_with_offset(strip_gantt_comment(line));
        if trimmed.is_empty() {
            continue;
        }
        let mut statement_offset = offset + start;
        let mut statement = strip_gantt_comment(line).get(start..)?;
        if !header_seen {
            if !starts_with_ignoring_case(trimmed, "gantt") {
                return None;
            }
            header_seen = true;
            let rest = statement.get("gantt".len()..)?;
            let leading = rest.len() - rest.trim_start().len();
            if rest.trim().is_empty() {
                continue;
            }
            statement_offset += "gantt".len() + leading;
            statement = rest.get(leading..)?;
        }
        match classify_gantt_statement(statement)? {
            GanttStatement::Title(range) => {
                let text = statement.get(range.clone())?;
                scan.title = Some(span(
                    statement_offset + range.start,
                    text,
                    text.to_string(),
                    MermaidLabelQuoting::GanttTitle,
                ));
            }
            GanttStatement::Section(range) => {
                let text = statement.get(range.clone())?;
                scan.sections.push((
                    gantt_drawn_name(text),
                    span(
                        statement_offset + range.start,
                        text,
                        text.to_string(),
                        MermaidLabelQuoting::GanttSection,
                    ),
                ));
            }
            GanttStatement::Task { label, data } => {
                let text = statement.get(label.clone())?;
                let id = gantt_task_id(&data, &mut numbered);
                scan.tasks.push((
                    id,
                    span(
                        statement_offset + label.start,
                        text,
                        text.to_string(),
                        MermaidLabelQuoting::GanttTask,
                    ),
                ));
            }
            GanttStatement::AccessibilityBlock => {
                skip_accessibility_block(&lines, &mut index, str::trim_end);
            }
            GanttStatement::Other => {}
        }
    }
    header_seen.then_some(scan)
}

fn find_gantt_section(fence_body: &str, name: &str) -> MermaidLabelLookup {
    let scan = scan_gantt(fence_body).ok_or(MermaidLabelRefusal::NotLocated)?;
    let mut matches = scan
        .sections
        .iter()
        .filter(|(drawn, _)| drawn == name)
        .map(|(_, span)| span);
    let section = matches.next().ok_or(MermaidLabelRefusal::NotLocated)?;
    if matches.next().is_some() {
        return Err(MermaidLabelRefusal::Duplicated);
    }
    checked(fence_body, section.clone())
}

fn find_gantt_task(fence_body: &str, task_id: &str, rendered: &str) -> MermaidLabelLookup {
    let scan = scan_gantt(fence_body).ok_or(MermaidLabelRefusal::NotLocated)?;
    let mut matches = scan
        .tasks
        .iter()
        .filter(|(id, _)| id == task_id)
        .map(|(_, span)| span);
    let task = matches.next().ok_or(MermaidLabelRefusal::NotLocated)?;
    if matches.next().is_some() {
        return Err(MermaidLabelRefusal::Duplicated);
    }
    if rendered.trim() != task.label {
        return Err(MermaidLabelRefusal::NotLocated);
    }
    checked(fence_body, task.clone())
}

/// The name of the `index`-th attribute of `entity`.
///
/// merman reads an attribute block as a stream of words rather than line by
/// line, so the block is read the same way: a line may hold several attributes
/// or close the block after one, and the count has to follow it exactly. A
/// block this reader cannot split into attributes, or a statement that may open
/// a block for the entity in a form it does not know, makes the position of
/// every later attribute uncertain, so the entity is refused as a whole.
fn find_er_attribute(
    fence_body: &str,
    entity: &str,
    index: usize,
    rendered: &str,
) -> MermaidLabelLookup {
    let mut attributes: Vec<MermaidLabelSpan> = Vec::new();
    let mut unreadable = false;
    // Where the last attribute block ended. The lines up to there belong to
    // it, and what follows its `}` on the same line is checked when it closes.
    let mut resume_at = 0;
    for (offset, line) in body_lines(fence_body) {
        if offset < resume_at {
            continue;
        }
        let content = strip_comment(line);
        let (start, trimmed) = trimmed_with_offset(content);
        if !trimmed.contains('{') {
            continue;
        }
        let mut position = 0;
        let name = read_er_name(trimmed, &mut position);
        let brace = name
            .as_ref()
            .and_then(|_| trimmed.get(position..))
            .and_then(block_opening_brace)
            .map(|brace| position + brace);
        let (Some(name), Some(brace)) = (name.clone(), brace) else {
            if name.as_deref() == Some(entity) || name.is_none() {
                // A `{` this reader does not know the meaning of.
                unreadable |= !is_relationship(trimmed);
            }
            continue;
        };
        let Some((names, block_end)) = read_attribute_block(fence_body, offset + start + brace + 1)
        else {
            return Err(MermaidLabelRefusal::NotLocated);
        };
        resume_at = block_end;
        let rest_of_line = fence_body.get(block_end..).unwrap_or_default();
        let rest_of_line = rest_of_line
            .split_once('\n')
            .map_or(rest_of_line, |(line, _)| line);
        if strip_comment(rest_of_line).contains('{') {
            // Another block opened after this one closed, on the same line.
            unreadable = true;
        }
        if name != entity {
            continue;
        }
        match names {
            Some(names) => attributes.extend(names.into_iter().map(|range| {
                MermaidLabelSpan {
                    label: fence_body
                        .get(range.clone())
                        .unwrap_or_default()
                        .to_string(),
                    range,
                    quoting: MermaidLabelQuoting::ErAttributeName,
                }
            })),
            None => unreadable = true,
        }
    }
    if unreadable {
        return Err(MermaidLabelRefusal::NotLocated);
    }
    let attribute = attributes
        .get(index)
        .ok_or(MermaidLabelRefusal::NotLocated)?;
    // The name is written bare, so a name the diagram draws differently was
    // spelled with an entity (`#…;`) this slot cannot write back.
    if attribute.label != rendered {
        return Err(MermaidLabelRefusal::NotLocated);
    }
    checked(fence_body, attribute.clone())
}

/// Whether a line holding a `{` is a relationship, whose cardinality may be
/// spelled with one (`||--o{`).
fn is_relationship(statement: &str) -> bool {
    statement.contains("--") || statement.contains("..")
}

/// Where the `{` of an attribute block is in `rest`, the part of a statement
/// after the entity's name: an optional `[alias]`, optional `:::classes`, then
/// the brace. `None` for anything else, a relationship included.
fn block_opening_brace(rest: &str) -> Option<usize> {
    let mut position = rest.len() - rest.trim_start().len();
    if rest.get(position..)?.starts_with('[') {
        let body_start = position + 1;
        let body = rest.get(body_start..)?;
        let leading = body.len() - body.trim_start().len();
        // A quoted alias may hold a `]` of its own.
        let search_from = match body.get(leading..)?.strip_prefix('"') {
            Some(quoted) => leading + 1 + quoted.find('"')? + 1,
            None => 0,
        };
        let close = search_from + body.get(search_from..)?.find(']')?;
        position = body_start + close + 1;
        let after = rest.get(position..)?;
        position += after.len() - after.trim_start().len();
    }
    if let Some(classes) = rest.get(position..)?.strip_prefix(":::") {
        let length = classes
            .find(|character: char| character.is_whitespace() || character == '{')
            .unwrap_or(classes.len());
        position += ":::".len() + length;
        let after = rest.get(position..)?;
        position += after.len() - after.trim_start().len();
    }
    rest.get(position..)?.starts_with('{').then_some(position)
}

enum AttributeToken {
    Word(Range<usize>),
    Comma,
    Comment,
}

fn is_attribute_key(word: &str) -> bool {
    ["PK", "FK", "UK"]
        .iter()
        .any(|key| word.eq_ignore_ascii_case(key))
}

/// Reads an attribute block the way merman's lexer does, from just after its
/// `{` to just after its `}`. Returns the ranges of the attribute names, or
/// `None` in their place when the block holds anything this reader cannot
/// count, along with where the block ends. `None` overall for a block that
/// never closes.
fn read_attribute_block(
    fence_body: &str,
    block_start: usize,
) -> Option<(Option<Vec<Range<usize>>>, usize)> {
    let mut tokens: Vec<AttributeToken> = Vec::new();
    let mut countable = true;
    let mut position = block_start;
    loop {
        let rest = fence_body.get(position..)?;
        position += rest.len() - rest.trim_start_matches([' ', '\t', '\r', '\n']).len();
        let rest = fence_body.get(position..)?;
        if rest.is_empty() {
            return None;
        }
        if rest.starts_with("%%") {
            position += rest.find('\n').unwrap_or(rest.len());
            continue;
        }
        if rest.starts_with('}') {
            position += 1;
            break;
        }
        if rest.starts_with(',') {
            tokens.push(AttributeToken::Comma);
            position += 1;
            continue;
        }
        if let Some(body) = rest.strip_prefix('"') {
            position += 1 + body.find('"')? + 1;
            tokens.push(AttributeToken::Comment);
            continue;
        }
        let length = rest
            .find(|character: char| {
                character.is_whitespace() || matches!(character, ',' | '"' | '}')
            })
            .unwrap_or(rest.len());
        if length == 0 {
            // A blank merman does not skip, which it cannot read either.
            countable = false;
            position += rest.chars().next().map_or(1, char::len_utf8);
            continue;
        }
        tokens.push(AttributeToken::Word(position..(position + length)));
        position += length;
    }
    if !countable {
        return Some((None, position));
    }

    let word = |token: Option<&AttributeToken>| match token {
        Some(AttributeToken::Word(range)) => fence_body
            .get(range.clone())
            .map(|text| (range.clone(), text)),
        _ => None,
    };
    let is_key = |token: Option<&AttributeToken>| {
        word(token).is_some_and(|(_, text)| is_attribute_key(text))
    };
    let mut names = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let (Some((_, type_text)), Some((name, name_text))) =
            (word(tokens.get(index)), word(tokens.get(index + 1)))
        else {
            return Some((None, position));
        };
        if is_attribute_key(type_text) || is_attribute_key(name_text) {
            return Some((None, position));
        }
        names.push(name);
        index += 2;
        if is_key(tokens.get(index)) {
            index += 1;
            while matches!(tokens.get(index), Some(AttributeToken::Comma))
                && is_key(tokens.get(index + 1))
            {
                index += 2;
            }
        }
        if matches!(tokens.get(index), Some(AttributeToken::Comment)) {
            index += 1;
        }
    }
    Some((Some(names), position))
}

#[cfg(test)]
#[path = "chart_labels_tests.rs"]
mod tests;
