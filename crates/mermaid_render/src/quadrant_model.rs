//! A round-trippable editing model for Mermaid quadrant charts.
//!
//! The shape of this module mirrors [`crate::timeline_model`]:
//! [`parse_quadrant`] reads the body of a ```` ```mermaid ```` fence into
//! [`QuadrantDocument`], and [`emit_quadrant`] writes it back. See
//! [`crate::mermaid_model`] for the invariants every diagram kind keeps.
//!
//! `merman` reads a quadrant chart line by line, and this module reads it the
//! same way:
//!
//! - A `%%` outside double quotes ends the line, and a `;` outside them ends a
//!   statement. A line with a comment after a statement, or with two
//!   statements, is refused rather than written back as something else.
//! - The keywords (`title`, `x-axis`, `quadrant-1`, …) are recognised by their
//!   first letters alone, so `titles` is a title reading `s`. A point whose
//!   name would read as a keyword is written in double quotes.
//! - Axis labels, quadrant labels and point names may be written in double
//!   quotes, which Mermaid drops; the emitter quotes a text only when it would
//!   not read back as itself without them. The title keeps its quotes.
//! - A point is `name:::class: [x, y] style, style`, with `x` and `y` written
//!   as `0`, `1` or `0.` and digits. The coordinates are kept as written.
//! - `classDef` lines are kept verbatim; a point names a class the way
//!   Mermaid does, and a class that is not defined simply adds no style.
//!
//! What the parser rewrites, all of it stable from the first emit: the
//! indentation, the whitespace around a value, the case of a keyword, quotes a
//! text does not need, the spacing of a point's styles, the order of the
//! statements, and comment lines in the middle of the body, which move below it.

use std::ops::Range;

use anyhow::{Result, anyhow, bail};
use merman::diagram::RenderSemanticModel;

use crate::flowchart_model::{PreservedLine, PreservedPosition};
use crate::journey_model::text_after_directive;
use crate::mermaid_model::{INDENT, first_statement_line, join_lines, keyword_tail, scan_preamble};

const HEADER: &str = "quadrantChart";

const QUADRANT_KEYWORDS: [&str; 4] = ["quadrant-1", "quadrant-2", "quadrant-3", "quadrant-4"];

/// The style keys Mermaid accepts on a point or in a `classDef`.
const STYLE_KEYS: [&str; 4] = ["radius", "color", "stroke-color", "stroke-width"];

/// The labels at the two ends of an axis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuadrantAxis {
    /// The left end of the x axis, the bottom end of the y axis.
    pub low: String,
    /// The other end. `None` when the statement has no `-->`; `Some("")` for
    /// a `-->` with nothing after it, which Mermaid draws as an arrow after
    /// the low end's label.
    pub high: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuadrantPoint {
    pub name: String,
    /// The class after `:::`, whose `classDef` styles the point.
    pub class: Option<String>,
    /// The coordinates as written, e.g. `0.30`, so that they come back
    /// unchanged.
    pub x: String,
    pub y: String,
    /// The point's own styles, e.g. `radius: 10, color: #ff0000`.
    pub styles: String,
}

/// An editable quadrant chart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuadrantDocument {
    /// The title, without the `title` keyword and with any quotes it was
    /// written with. An empty title draws nothing and is `None`.
    pub title: Option<String>,
    pub x_axis: Option<QuadrantAxis>,
    pub y_axis: Option<QuadrantAxis>,
    /// The labels of `quadrant-1` (top right), `quadrant-2` (top left),
    /// `quadrant-3` (bottom left) and `quadrant-4` (bottom right).
    pub quadrants: [Option<String>; 4],
    pub points: Vec<QuadrantPoint>,
    pub preserved_lines: Vec<PreservedLine>,
}

impl QuadrantDocument {
    /// A blank quadrant chart, used when the editor is opened on an empty fence.
    pub fn empty() -> Self {
        Self {
            title: None,
            x_axis: None,
            y_axis: None,
            quadrants: [None, None, None, None],
            points: Vec::new(),
            preserved_lines: Vec::new(),
        }
    }

    /// Every text the form edits: the title, the axis ends, the quadrant labels
    /// and the point names.
    fn texts(&self) -> impl Iterator<Item = &str> {
        let axes = [&self.x_axis, &self.y_axis]
            .into_iter()
            .flatten()
            .flat_map(|axis| [Some(axis.low.as_str()), axis.high.as_deref()])
            .flatten();
        self.title
            .as_deref()
            .into_iter()
            .chain(axes)
            .chain(self.quadrants.iter().flatten().map(String::as_str))
            .chain(self.points.iter().map(|point| point.name.as_str()))
    }

    /// Why the document cannot be written back as a diagram Mermaid accepts, in
    /// Japanese, or `None` when it can.
    pub fn structure_error(&self) -> Option<String> {
        if let Some(text) = self.texts().find(|text| has_rewritten_html_attribute(text)) {
            return Some(format!(
                "`{text}` の HTML の属性は `'` で囲んでください (`\"` は Mermaid が `'` に書き換えるので、区切りの読み方が変わります)。"
            ));
        }
        if let Some(title) = &self.title
            && !is_valid_quadrant_title(title)
        {
            return Some(format!(
                "タイトル `{title}` は書けません (`%%` と `;` と改行は不可、前後の空白だけにもできません)。"
            ));
        }
        for (name, axis) in [("横軸", &self.x_axis), ("縦軸", &self.y_axis)] {
            if let Some(axis) = axis
                && !is_valid_quadrant_axis(axis)
            {
                return Some(format!(
                    "{name}のラベルは書けません (`\"` と改行は不可、前後の空白も不可)。"
                ));
            }
        }
        for (index, label) in self.quadrants.iter().enumerate() {
            if let Some(label) = label
                && !is_valid_quadrant_label(label)
            {
                return Some(format!(
                    "象限 {} のラベル `{label}` は書けません (`\"` と改行は不可、前後の空白も不可)。",
                    index + 1
                ));
            }
        }
        for point in &self.points {
            if point_head(&point.name, point.class.as_deref()).is_none() {
                if point
                    .class
                    .as_deref()
                    .is_some_and(|class| !is_valid_quadrant_class(class))
                {
                    return Some(format!(
                        "点 `{}` のクラス名は英数字と `_` だけにしてください。",
                        point.name
                    ));
                }
                return Some(format!(
                    "点の名前 `{}` は書けません (`\"` と改行は不可、空と前後の空白も不可)。",
                    point.name
                ));
            }
            for (axis, value) in [("x", &point.x), ("y", &point.y)] {
                if !is_valid_quadrant_coordinate(value) {
                    return Some(format!(
                        "点 `{}` の {axis} `{value}` は使えません (0 から 1 まで。`0`、`1`、`0.` で始まる小数だけ)。",
                        point.name
                    ));
                }
            }
            if !is_valid_quadrant_styles(&point.styles) {
                return Some(format!(
                    "点 `{}` のスタイル `{}` は使えません (`radius: 10`、`color: #ff0000`、`stroke-color: #000`、`stroke-width: 2px` をカンマで区切って)。",
                    point.name, point.styles
                ));
            }
        }
        None
    }
}

/// Whether `source` is a quadrant chart: its first statement is `quadrantChart`.
pub fn is_quadrant_source(source: &str) -> bool {
    first_statement_line(source).is_some_and(|line| keyword_tail(line, HEADER).is_some())
}

/// Reads `source` — the body of a ```` ```mermaid ```` fence — into an editable
/// document.
pub fn parse_quadrant(source: &str) -> Result<QuadrantDocument> {
    let document = scan_quadrant(source)?;

    // `merman` is asked last, so that a construct this module knows about is
    // reported with the reason that says what to do.
    if !source.trim().is_empty() {
        let parsed = crate::parse_for_render_model(source)
            .map_err(|error| {
                anyhow!("Mermaid の構文として読めませんでした ({error})。テキストのまま編集してください。")
            })?
            .ok_or_else(|| anyhow!("Mermaid の図として解釈できませんでした。"))?;
        if !matches!(parsed.model, RenderSemanticModel::QuadrantChart(_)) {
            bail!("この図は 4 象限チャートではありません。");
        }
    }

    Ok(document)
}

fn scan_quadrant(source: &str) -> Result<QuadrantDocument> {
    let lines = source.lines().collect::<Vec<_>>();
    let preamble = scan_preamble(&lines);
    let mut document = QuadrantDocument::empty();
    document.preserved_lines = preamble.preserved;

    let Some(header_index) = preamble.header else {
        return Ok(document);
    };

    let header = lines.get(header_index).copied().unwrap_or_default().trim();
    let Some(tail) = keyword_tail(header, HEADER) else {
        bail!(
            "4 象限チャートではありません (`quadrantChart` で始まる図だけを構造化編集できます)。"
        );
    };
    if !tail.is_empty() {
        bail!(
            "`quadrantChart` の行の `{tail}` は構造化編集に対応していません。テキストのまま編集してください。"
        );
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
                bail!(
                    "指示子 `%%{{…}}%%` の後ろの `{rest}` は Mermaid が文として読みますが、構造化編集ではコメントと一緒に動いてしまいます。指示子を独立した行にしてください。"
                );
            }
            document
                .preserved_lines
                .push(PreservedLine { position, text });
            continue;
        }

        let Some(statement) = read_line(line, trimmed)? else {
            continue;
        };
        let preserved = |document: &mut QuadrantDocument, end: usize| {
            document.preserved_lines.push(PreservedLine {
                position,
                text: join_lines(lines.get(start..end).unwrap_or_default()),
            });
        };
        match classify(statement) {
            Statement::Accessibility { block } => {
                if block {
                    // Mermaid reads everything up to the next `}` on a later
                    // line as the description, even when the opening line
                    // closes the block itself.
                    if statement.contains('}') {
                        bail!(
                            "1 行で閉じた `accDescr {{ … }}` は、Mermaid が次の `}}` の行までを説明として読みます ({trimmed})。複数行に分けるか、テキストのまま編集してください。"
                        );
                    }
                    while lines.get(index).is_some_and(|line| !line.contains('}')) {
                        index += 1;
                    }
                    if index < lines.len() {
                        index += 1;
                    }
                }
                preserved(&mut document, index);
            }
            Statement::ClassDef => preserved(&mut document, index),
            Statement::Title(value) => {
                // Mermaid keeps the last one, so a second statement would make
                // writing the chart back drop a line the user can see.
                if seen_title {
                    bail!(
                        "`title` が 2 回書かれています。1 つにまとめてからもう一度開いてください。"
                    );
                }
                seen_title = true;
                if !value.is_empty() {
                    document.title = Some(value.to_string());
                }
            }
            Statement::XAxis(rest) => {
                set_axis(&mut document.x_axis, "x-axis", rest, trimmed)?;
                position = PreservedPosition::Bottom;
            }
            Statement::YAxis(rest) => {
                set_axis(&mut document.y_axis, "y-axis", rest, trimmed)?;
                position = PreservedPosition::Bottom;
            }
            Statement::Quadrant(quadrant, rest) => {
                let Some(slot) = document.quadrants.get_mut(quadrant) else {
                    continue;
                };
                if slot.is_some() {
                    bail!(
                        "`quadrant-{}` が 2 回書かれています。1 つにまとめてからもう一度開いてください。",
                        quadrant + 1
                    );
                }
                let label =
                    parse_text_value(rest).map_err(|reason| anyhow!("{reason} ({trimmed})"))?;
                *slot = Some(label);
                position = PreservedPosition::Bottom;
            }
            Statement::Point => {
                let point = parse_point(statement)
                    .map_err(|reason| anyhow!("{reason} ({trimmed})"))?
                    .ok_or_else(|| anyhow!("`{trimmed}` は点として読めません。"))?;
                document.points.push(point);
                position = PreservedPosition::Bottom;
            }
            Statement::Unknown => {
                bail!(
                    "`{trimmed}` は点 (`名前: [x, y]`) としても他の文としても読めません。テキストのまま編集してください。"
                );
            }
        }
    }

    // An axis or quadrant written with nothing in it was held as empty so that
    // a second statement is still reported; it draws nothing, so the model
    // does not carry it.
    for axis in [&mut document.x_axis, &mut document.y_axis] {
        if axis
            .as_ref()
            .is_some_and(|axis| axis.low.is_empty() && axis.high.is_none())
        {
            *axis = None;
        }
    }
    for label in &mut document.quadrants {
        if label.as_deref() == Some("") {
            *label = None;
        }
    }

    Ok(document)
}

/// Mermaid lets a later axis statement overwrite an earlier one end by end, so
/// a second one is refused rather than merged.
fn set_axis(
    slot: &mut Option<QuadrantAxis>,
    keyword: &str,
    rest: &str,
    trimmed: &str,
) -> Result<()> {
    if slot.is_some() {
        bail!("`{keyword}` が 2 回書かれています。1 つにまとめてからもう一度開いてください。");
    }
    *slot = Some(parse_axis(rest).map_err(|reason| anyhow!("{reason} ({trimmed})"))?);
    Ok(())
}

/// The one statement `line` holds, trimmed, or `None` for a line with nothing
/// but separators in it.
fn read_line<'a>(line: &'a str, trimmed: &str) -> Result<Option<&'a str>> {
    let (text, comment) = match comment_start(line) {
        Some(start) => (line.get(..start).unwrap_or_default(), true),
        None => (line, false),
    };
    let statements = statement_ranges(text)
        .into_iter()
        .filter_map(|range| text.get(range))
        .map(str::trim)
        .filter(|statement| !statement.is_empty())
        .collect::<Vec<_>>();
    if comment && !statements.is_empty() {
        bail!(
            "行の途中の `%%` から先は Mermaid が読み飛ばします ({trimmed})。コメントを別の行に移すか、テキストのまま編集してください。"
        );
    }
    match statements.as_slice() {
        [] => Ok(None),
        [statement] => Ok(Some(statement)),
        _ => bail!(
            "`;` で区切った複数の文は構造化編集に対応していません ({trimmed})。1 行に 1 つずつ書いてください。"
        ),
    }
}

/// The one statement `line` would be read as, or `None` when it would be read
/// as something else: a comment, several statements or nothing.
fn single_statement(line: &str) -> Option<&str> {
    read_line(line, line).ok().flatten()
}

/// Where `merman` cuts a line as a comment: the first `%%` outside double
/// quotes. Bytes rather than characters, since the markers are ASCII and never
/// occur inside a multibyte character.
fn comment_start(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut in_quotes = false;
    let mut index = 0;
    while let (Some(&byte), Some(&next)) = (bytes.get(index), bytes.get(index + 1)) {
        match byte {
            b'"' => in_quotes = !in_quotes,
            b'%' if !in_quotes && next == b'%' => return Some(index),
            _ => {}
        }
        index += 1;
    }
    None
}

/// The statements `text` splits into at each `;` outside double quotes. A
/// `#name;` entity is replaced by Mermaid before the line is split, so its `;`
/// separates nothing.
fn statement_ranges(text: &str) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut in_quotes = false;
    let mut start = 0;
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'"' => in_quotes = !in_quotes,
            b'#' => {
                if let Some(word) = entity_length(bytes.get(index..).unwrap_or_default()) {
                    index += word + 2;
                    continue;
                }
            }
            b';' if !in_quotes => {
                ranges.push(start..index);
                start = index + 1;
            }
            _ => {}
        }
        index += 1;
    }
    ranges.push(start..bytes.len());
    ranges
}

/// The length of the word of a `#word;` entity at the start of `text`.
fn entity_length(text: &[u8]) -> Option<usize> {
    let rest = text.strip_prefix(b"#")?;
    let word = rest
        .iter()
        .take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_')
        .count();
    (word > 0 && rest.get(word) == Some(&b';')).then_some(word)
}

/// What one statement of a quadrant chart is, decided in the order `merman`'s
/// parser tries the possibilities.
#[derive(Debug, PartialEq, Eq)]
enum Statement<'a> {
    Accessibility { block: bool },
    Title(&'a str),
    XAxis(&'a str),
    YAxis(&'a str),
    Quadrant(usize, &'a str),
    ClassDef,
    Point,
    Unknown,
}

fn classify(statement: &str) -> Statement<'_> {
    if keyword_rest(statement, "accTitle").is_some_and(|rest| rest.starts_with(':')) {
        return Statement::Accessibility { block: false };
    }
    if let Some(rest) = keyword_rest(statement, "accDescr") {
        if rest.starts_with('{') {
            return Statement::Accessibility { block: true };
        }
        if rest.starts_with(':') {
            return Statement::Accessibility { block: false };
        }
    }
    if let Some(rest) = keyword_rest(statement, "title") {
        return Statement::Title(rest.trim());
    }
    if let Some(rest) = keyword_rest(statement, "x-axis") {
        return Statement::XAxis(rest);
    }
    if let Some(rest) = keyword_rest(statement, "y-axis") {
        return Statement::YAxis(rest);
    }
    for (index, keyword) in QUADRANT_KEYWORDS.iter().enumerate() {
        if let Some(rest) = keyword_rest(statement, keyword) {
            return Statement::Quadrant(index, rest);
        }
    }
    if keyword_rest(statement, "classDef").is_some() {
        return Statement::ClassDef;
    }
    if find_point_colon(statement).is_some() {
        return Statement::Point;
    }
    Statement::Unknown
}

/// What follows `keyword` at the start of `statement`, with its leading
/// whitespace removed. `merman` matches the word without regard to case and
/// without asking for anything after it.
fn keyword_rest<'a>(statement: &'a str, keyword: &str) -> Option<&'a str> {
    let head = statement.get(..keyword.len())?;
    if !head.eq_ignore_ascii_case(keyword) {
        return None;
    }
    Some(statement.get(keyword.len()..)?.trim_start())
}

/// A label as Mermaid reads it: the text inside double quotes (or inside the
/// backticks of a `"` `` ` `` markdown string), or the trimmed text.
fn parse_text_value(raw: &str) -> Result<String, String> {
    let text = raw.trim();
    if text.starts_with("\"`") {
        return text
            .strip_prefix("\"`")
            .and_then(|inner| inner.strip_suffix("`\""))
            .map(str::to_string)
            .ok_or_else(|| "閉じていない \"` … `\" があります。".to_string());
    }
    if text.starts_with('"') {
        return text
            .strip_prefix('"')
            .and_then(|inner| inner.strip_suffix('"'))
            .map(str::to_string)
            .ok_or_else(|| "閉じていない `\"` があります。".to_string());
    }
    Ok(text.to_string())
}

/// Where an axis's `-->` (two or more dashes and `>`) starts and ends, outside
/// double quotes.
fn axis_delimiter(text: &str) -> Option<Range<usize>> {
    let bytes = text.as_bytes();
    let mut in_quotes = false;
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        if byte == b'"' {
            in_quotes = !in_quotes;
        } else if byte == b'-' && !in_quotes {
            let dashes = bytes
                .get(index..)
                .unwrap_or_default()
                .iter()
                .take_while(|byte| **byte == b'-')
                .count();
            if dashes >= 2 && bytes.get(index + dashes) == Some(&b'>') {
                return Some(index..index + dashes + 1);
            }
        }
        index += 1;
    }
    None
}

fn parse_axis(rest: &str) -> Result<QuadrantAxis, String> {
    let rest = rest.trim_start();
    match axis_delimiter(rest) {
        Some(range) => {
            let low = parse_text_value(rest.get(..range.start).unwrap_or_default())?;
            let high_raw = rest.get(range.end..).unwrap_or_default().trim();
            let high = parse_text_value(high_raw)?;
            // Mermaid adds an arrow to the low label only when nothing at all
            // follows `-->`, so an empty `""` there is not the same as nothing.
            if high.is_empty() && !high_raw.is_empty() {
                return Err(
                    "`-->` の後の空の `\"\"` は構造化編集に対応していません。消すか、テキストのまま編集してください。"
                        .to_string(),
                );
            }
            Ok(QuadrantAxis {
                low,
                high: Some(high),
            })
        }
        None => Ok(QuadrantAxis {
            low: parse_text_value(rest)?,
            high: None,
        }),
    }
}

/// The byte offset of the `:` that ends a point's name: the first one outside
/// double quotes that is followed, after any whitespace, by `[`.
fn find_point_colon(statement: &str) -> Option<usize> {
    let bytes = statement.as_bytes();
    let mut in_quotes = false;
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        if byte == b'"' {
            in_quotes = !in_quotes;
        } else if byte == b':' && !in_quotes {
            let after = statement.get(index + 1..).unwrap_or_default().trim_start();
            if after.starts_with('[') {
                return Some(index);
            }
        }
        index += 1;
    }
    None
}

fn parse_point(statement: &str) -> Result<Option<QuadrantPoint>, String> {
    let Some(colon) = find_point_colon(statement) else {
        return Ok(None);
    };
    let head = statement.get(..colon).unwrap_or_default().trim_end();
    let tail = statement.get(colon + 1..).unwrap_or_default();

    let (class, label) = match head.rfind(":::") {
        Some(position) => {
            let class = head
                .get(position..)
                .unwrap_or_default()
                .trim_start_matches(":::")
                .trim();
            if is_valid_quadrant_class(class) {
                (
                    Some(class.to_string()),
                    head.get(..position).unwrap_or_default(),
                )
            } else {
                (None, head)
            }
        }
        None => (None, head),
    };
    let name = parse_text_value(label)?;

    let Some(inside) = tail.trim_start().strip_prefix('[') else {
        return Err("点の `:` の後に `[` がありません。".to_string());
    };
    let Some((coordinates, after)) = inside.split_once(']') else {
        return Err("点の座標の `]` がありません。".to_string());
    };
    let coordinates = coordinates.split(',').map(str::trim).collect::<Vec<_>>();
    let [x, y] = coordinates.as_slice() else {
        return Err(
            "点の座標は `[x, y]` の 2 つにしてください (3 つ目以降は Mermaid が読み飛ばします)。"
                .to_string(),
        );
    };
    for value in [x, y] {
        if !is_valid_quadrant_coordinate(value) {
            return Err(format!(
                "座標 `{value}` は Mermaid が読めません (0 から 1 まで。`0`、`1`、`0.` で始まる小数だけ)。"
            ));
        }
    }
    let styles = normalize_styles(after)?;
    Ok(Some(QuadrantPoint {
        name,
        class,
        x: x.to_string(),
        y: y.to_string(),
        styles,
    }))
}

/// The styles after a point's coordinates, each written `key: value` and
/// joined with `, `, or why Mermaid would refuse them.
fn normalize_styles(text: &str) -> Result<String, String> {
    let mut styles = Vec::new();
    for style in text
        .split(',')
        .map(str::trim)
        .filter(|style| !style.is_empty())
    {
        let Some((key, value)) = style.split_once(':') else {
            return Err(format!(
                "スタイル `{style}` は `名前: 値` の形にしてください。"
            ));
        };
        let (key, value) = (key.trim(), value.trim());
        let is_valid = match key {
            "radius" => !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()),
            "color" | "stroke-color" => is_hex_color(value),
            "stroke-width" => value.strip_suffix("px").is_some_and(|number| {
                !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
            }),
            _ => {
                return Err(format!(
                    "スタイル `{key}` は使えません ({} のどれか)。",
                    STYLE_KEYS.join("、")
                ));
            }
        };
        if !is_valid || value.contains(['\n', '\r', ';', '"']) {
            return Err(format!("スタイル `{key}` の値 `{value}` は使えません。"));
        }
        styles.push(format!("{key}: {value}"));
    }
    Ok(styles.join(", "))
}

fn is_hex_color(value: &str) -> bool {
    let digits = value.strip_prefix('#').unwrap_or(value);
    matches!(digits.len(), 3 | 6) && digits.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Writes the document back as Mermaid source, without a trailing newline.
pub fn emit_quadrant(document: &QuadrantDocument) -> String {
    let mut out = String::new();

    for line in preserved_at(document, PreservedPosition::Preamble) {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(HEADER);
    out.push('\n');
    // The lines kept from before the first axis, quadrant or point go above
    // the title, which is where the insert template writes its comment.
    for line in preserved_at(document, PreservedPosition::Top) {
        out.push_str(line);
        out.push('\n');
    }
    if let Some(title) = &document.title {
        out.push_str(&format!("{INDENT}title {title}\n"));
    }
    for (keyword, axis) in [("x-axis", &document.x_axis), ("y-axis", &document.y_axis)] {
        if let Some(axis) = axis {
            let line = axis_line(keyword, axis).unwrap_or_else(|| plain_axis_line(keyword, axis));
            out.push_str(&format!("{INDENT}{line}\n"));
        }
    }
    for (keyword, label) in QUADRANT_KEYWORDS.iter().zip(&document.quadrants) {
        if let Some(label) = label {
            let line =
                quadrant_line(keyword, label).unwrap_or_else(|| format!("{keyword} {label}"));
            out.push_str(&format!("{INDENT}{line}\n"));
        }
    }
    for point in &document.points {
        out.push_str(INDENT);
        out.push_str(&point_line(point));
        out.push('\n');
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
    document: &QuadrantDocument,
    position: PreservedPosition,
) -> impl Iterator<Item = &str> {
    document
        .preserved_lines
        .iter()
        .filter(move |line| line.position == position)
        .map(|line| line.text.as_str())
}

/// The ways a label can be written: as it is, and in double quotes.
fn text_tokens(text: &str) -> Vec<String> {
    vec![text.to_string(), format!("\"{text}\"")]
}

fn plain_axis_line(keyword: &str, axis: &QuadrantAxis) -> String {
    let mut line = keyword.to_string();
    if !axis.low.is_empty() {
        line.push(' ');
        line.push_str(&axis.low);
    }
    if let Some(high) = &axis.high {
        line.push_str(" -->");
        if !high.is_empty() {
            line.push(' ');
            line.push_str(high);
        }
    }
    line
}

/// The first spelling of an axis statement that reads back as `axis`, or
/// `None` when there is none.
fn axis_line(keyword: &str, axis: &QuadrantAxis) -> Option<String> {
    if axis.low.contains(['\n', '\r'])
        || axis
            .high
            .as_deref()
            .is_some_and(|high| high.contains(['\n', '\r']))
    {
        return None;
    }
    let lows = if axis.low.is_empty() {
        vec![String::new()]
    } else {
        text_tokens(&axis.low)
    };
    let highs = match &axis.high {
        None => vec![None],
        Some(high) if high.is_empty() => vec![Some(String::new())],
        Some(high) => text_tokens(high).into_iter().map(Some).collect(),
    };
    for low in &lows {
        for high in &highs {
            let line = plain_axis_line(
                keyword,
                &QuadrantAxis {
                    low: low.clone(),
                    high: high.clone(),
                },
            );
            let reads_back = single_statement(&line).is_some_and(|statement| {
                let rest = match classify(statement) {
                    Statement::XAxis(rest) if keyword == "x-axis" => rest,
                    Statement::YAxis(rest) if keyword == "y-axis" => rest,
                    _ => return false,
                };
                parse_axis(rest).is_ok_and(|parsed| &parsed == axis)
            });
            if reads_back {
                return Some(line);
            }
        }
    }
    None
}

fn quadrant_line(keyword: &str, label: &str) -> Option<String> {
    if label.contains(['\n', '\r']) {
        return None;
    }
    text_tokens(label)
        .into_iter()
        .map(|token| format!("{keyword} {token}"))
        .find(|line| {
            single_statement(line).is_some_and(|statement| match classify(statement) {
                Statement::Quadrant(index, rest) => {
                    QUADRANT_KEYWORDS.get(index) == Some(&keyword)
                        && parse_text_value(rest).is_ok_and(|parsed| parsed == label)
                }
                _ => false,
            })
        })
}

/// The part of a point's line before its coordinates, quoting the name when
/// it would not read back as itself otherwise.
fn point_head(name: &str, class: Option<&str>) -> Option<String> {
    if name.is_empty()
        || name.trim() != name
        || name.contains(['\n', '\r'])
        || class.is_some_and(|class| !is_valid_quadrant_class(class))
    {
        return None;
    }
    let suffix = class.map(|class| format!(":::{class}")).unwrap_or_default();
    text_tokens(name)
        .into_iter()
        .map(|token| format!("{token}{suffix}"))
        .find(|head| {
            let line = format!("{head}: [0, 0]");
            single_statement(&line).is_some_and(|statement| {
                classify(statement) == Statement::Point
                    && parse_point(statement).is_ok_and(|point| {
                        point.is_some_and(|point| {
                            point.name == name && point.class.as_deref() == class
                        })
                    })
            })
        })
}

fn point_line(point: &QuadrantPoint) -> String {
    let head = point_head(&point.name, point.class.as_deref()).unwrap_or_else(|| {
        let suffix = point
            .class
            .as_ref()
            .map(|class| format!(":::{class}"))
            .unwrap_or_default();
        format!("{}{suffix}", point.name)
    });
    let mut line = format!("{head}: [{}, {}]", point.x, point.y);
    if !point.styles.is_empty() {
        line.push(' ');
        line.push_str(&point.styles);
    }
    line
}

/// Whether Mermaid rewrites a `="…"` inside an HTML tag in `text` to `='…'`
/// before the chart is read, the way `merman`'s preprocessing does. That takes
/// the text between the quotes out of a quoted label, so a `;`, `%%`, `-->` or
/// `:` there is read where this module does not read it.
fn has_rewritten_html_attribute(text: &str) -> bool {
    let is_word_byte = |byte: &u8| byte.is_ascii_alphanumeric() || *byte == b'_';
    let bytes = text.as_bytes();
    let mut probe = 0;
    while let Some(relative) = text.get(probe..).and_then(|rest| rest.find('<')) {
        let tag_start = probe + relative + 1;
        if !bytes.get(tag_start).is_some_and(is_word_byte) {
            probe = tag_start;
            continue;
        }
        let tag_end = tag_start
            + bytes
                .get(tag_start..)
                .unwrap_or_default()
                .iter()
                .take_while(|byte| is_word_byte(byte))
                .count();
        let Some(end) = text
            .get(tag_end..)
            .and_then(|rest| rest.find('>'))
            .map(|relative| tag_end + relative)
        else {
            probe = tag_start;
            continue;
        };
        let attributes = text.get(tag_end..end).unwrap_or_default();
        if let Some(value_start) = attributes.find("=\"")
            && attributes
                .get(value_start + 2..)
                .is_some_and(|rest| rest.contains('"'))
        {
            return true;
        }
        probe = end + 1;
    }
    false
}

/// Whether `title` can be written after `title`. Mermaid keeps quotes in a
/// title, so it is written exactly as it is.
pub fn is_valid_quadrant_title(title: &str) -> bool {
    !title.is_empty()
        && !has_rewritten_html_attribute(title)
        && title.trim() == title
        && !title.contains(['\n', '\r'])
        && single_statement(&format!("title {title}"))
            .is_some_and(|statement| classify(statement) == Statement::Title(title))
}

/// Whether an axis can be written so that it reads back as itself.
pub fn is_valid_quadrant_axis(axis: &QuadrantAxis) -> bool {
    axis.low.trim() == axis.low
        && !has_rewritten_html_attribute(&axis.low)
        && axis
            .high
            .as_deref()
            .is_none_or(|high| !has_rewritten_html_attribute(high))
        && axis.high.as_deref().is_none_or(|high| high.trim() == high)
        && axis_line("x-axis", axis).is_some()
}

/// Whether a quadrant's label can be written so that it reads back as itself.
pub fn is_valid_quadrant_label(label: &str) -> bool {
    !label.is_empty()
        && label.trim() == label
        && !has_rewritten_html_attribute(label)
        && quadrant_line("quadrant-1", label).is_some()
}

/// Whether `name` can be written as a point's name.
pub fn is_valid_quadrant_point_name(name: &str) -> bool {
    !has_rewritten_html_attribute(name) && point_head(name, None).is_some()
}

/// Whether `class` can follow `:::` on a point.
pub fn is_valid_quadrant_class(class: &str) -> bool {
    !class.is_empty()
        && class
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Whether `value` is a coordinate Mermaid reads: `0`, `1`, or `0.` followed
/// by digits.
pub fn is_valid_quadrant_coordinate(value: &str) -> bool {
    value == "0"
        || value == "1"
        || value.strip_prefix("0.").is_some_and(|digits| {
            !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
        })
}

/// Whether `styles` is a list of point styles Mermaid accepts, written the way
/// the emitter writes it.
pub fn is_valid_quadrant_styles(styles: &str) -> bool {
    normalize_styles(styles).is_ok_and(|normalized| normalized == styles)
}

/// `styles` written the way the emitter writes it, or `None` when Mermaid would
/// refuse it.
pub fn normalized_quadrant_styles(styles: &str) -> Option<String> {
    normalize_styles(styles).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MermaidTheme, render_to_svg};
    use merman::{Engine, ParseOptions};

    /// The template `markdown::InsertMermaidDiagram` inserts, copied from
    /// `editor::actions::MermaidDiagramKind::QuadrantChart`; the editor's own
    /// tests read it from the action.
    const TEMPLATE: &str = "quadrantChart\n    %% 点は 名前: [横, 縦] で、どちらも 0〜1\n    title 施策の優先度\n    x-axis 労力が小さい --> 労力が大きい\n    y-axis 効果が小さい --> 効果が大きい\n    quadrant-1 計画して進める\n    quadrant-2 すぐやる\n    quadrant-3 後回し\n    quadrant-4 見直す\n    施策A: [0.3, 0.8]\n    施策B: [0.7, 0.6]\n    施策C: [0.2, 0.3]";

    fn parse(source: &str) -> QuadrantDocument {
        parse_quadrant(source).unwrap_or_else(|error| panic!("{source} should parse: {error}"))
    }

    /// Emitting must reach a fixed point after one pass, and the text is what
    /// is compared, so a line that vanished from both documents still fails.
    fn assert_stable(source: &str) -> String {
        let first = parse(source);
        let first_text = emit_quadrant(&first);
        let second = parse(&first_text);
        let second_text = emit_quadrant(&second);
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

    fn point(name: &str, x: &str, y: &str) -> QuadrantPoint {
        QuadrantPoint {
            name: name.to_string(),
            class: None,
            x: x.to_string(),
            y: y.to_string(),
            styles: String::new(),
        }
    }

    /// What `merman` itself reads from `source`, as JSON so that the float
    /// coordinates and the styles compare as a whole. The points are put back
    /// in the order they are written, which `merman` reverses.
    fn merman_reading(source: &str) -> serde_json::Value {
        let parsed = Engine::new()
            .parse_diagram_for_render_model_sync(source, ParseOptions::strict())
            .unwrap_or_else(|error| panic!("merman should read {source}: {error}"))
            .unwrap_or_else(|| panic!("merman should find a diagram in {source}"));
        let RenderSemanticModel::QuadrantChart(mut model) = parsed.model else {
            panic!("merman should read {source} as a quadrant chart");
        };
        model.points.reverse();
        let mut value = serde_json::to_value(&model)
            .unwrap_or_else(|error| panic!("the model should serialize: {error}"));
        // The accessibility statements are kept verbatim, and `merman` reads
        // the frontmatter title into the same field as `title`.
        if let Some(object) = value.as_object_mut() {
            object.remove("title");
        }
        value
    }

    fn merman_title(source: &str) -> Option<String> {
        let parsed = Engine::new()
            .parse_diagram_for_render_model_sync(source, ParseOptions::strict())
            .ok()
            .flatten()?;
        match parsed.model {
            RenderSemanticModel::QuadrantChart(model) => model.title,
            _ => None,
        }
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
        assert_eq!(document.title.as_deref(), Some("施策の優先度"));
        assert_eq!(
            document.x_axis,
            Some(QuadrantAxis {
                low: "労力が小さい".to_string(),
                high: Some("労力が大きい".to_string()),
            })
        );
        assert_eq!(
            document.quadrants,
            [
                Some("計画して進める".to_string()),
                Some("すぐやる".to_string()),
                Some("後回し".to_string()),
                Some("見直す".to_string()),
            ]
        );
        assert_eq!(
            document.points,
            vec![
                point("施策A", "0.3", "0.8"),
                point("施策B", "0.7", "0.6"),
                point("施策C", "0.2", "0.3"),
            ]
        );
        assert_eq!(emit_quadrant(&document), TEMPLATE);
        assert_eq!(document.structure_error(), None);
        assert_renders(TEMPLATE);
    }

    #[test]
    fn quotes_classes_styles_and_open_axes_survive() {
        let source = "quadrantChart\n  title \"引用付きの題\"\n  x-AxIs \"Urgent(* +=[?\"  -->\n  y-axis Low\n  quadrant-2 \"Do: now\"\n  classDef hot color: #ff3300, radius: 12\n  Point A:::hot: [0.9, 0.0]\n  \"Title B\": [1, 0.25]   radius:  10  ,  color: #ff3300\n  quadrant-1: [0.5, 0.5]";
        let document = parse(source);
        assert_eq!(document.title.as_deref(), Some("\"引用付きの題\""));
        assert_eq!(
            document.x_axis,
            Some(QuadrantAxis {
                low: "Urgent(* +=[?".to_string(),
                high: Some(String::new()),
            })
        );
        assert_eq!(
            document.y_axis,
            Some(QuadrantAxis {
                low: "Low".to_string(),
                high: None,
            })
        );
        assert_eq!(document.quadrants[0].as_deref(), Some(": [0.5, 0.5]"));
        assert_eq!(document.quadrants[1].as_deref(), Some("Do: now"));
        assert_eq!(
            document.points,
            vec![
                QuadrantPoint {
                    class: Some("hot".to_string()),
                    ..point("Point A", "0.9", "0.0")
                },
                QuadrantPoint {
                    styles: "radius: 10, color: #ff3300".to_string(),
                    ..point("Title B", "1", "0.25")
                },
            ]
        );
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "quadrantChart\n    title \"引用付きの題\"\n    x-axis Urgent(* +=[? -->\n    y-axis Low\n    quadrant-1 : [0.5, 0.5]\n    quadrant-2 Do: now\n    Point A:::hot: [0.9, 0.0]\n    \"Title B\": [1, 0.25] radius: 10, color: #ff3300\n  classDef hot color: #ff3300, radius: 12"
        );
        assert_eq!(merman_reading(&emitted), merman_reading(source));
        assert_renders(&emitted);
    }

    #[test]
    fn comments_accessibility_and_the_preamble_keep_their_place() {
        let source = "---\ntitle: 図\n---\n%%{init: {\"theme\": \"dark\"}}%%\nquadrantChart\n    %% 先頭のメモ\n    accTitle: 概要\n    title 優先度\n    x-axis 低 --> 高\n    accDescr {\n      説明の 1 行目\n      2 行目\n    }\n    %% 途中のメモ\n    A: [0.1, 0.2]";
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
            "---\ntitle: 図\n---\n%%{init: {\"theme\": \"dark\"}}%%\nquadrantChart\n    %% 先頭のメモ\n    accTitle: 概要\n    title 優先度\n    x-axis 低 --> 高\n    A: [0.1, 0.2]\n    accDescr {\n      説明の 1 行目\n      2 行目\n    }\n    %% 途中のメモ"
        );
        assert_eq!(merman_reading(&emitted), merman_reading(source));
        assert_renders(&emitted);
    }

    #[test]
    fn constructs_that_would_be_dropped_are_refused() {
        for (source, expected) in [
            ("quadrantChart\n    title a\n    title b", "2 回"),
            ("quadrantChart\n    x-axis a\n    x-axis b --> c", "2 回"),
            ("quadrantChart\n    quadrant-1 a\n    quadrant-1 b", "2 回"),
            ("quadrantChart\n    A: [0.1, 0.2] %% メモ", "`%%`"),
            ("quadrantChart\n    A: [0.1, 0.2]; B: [0.3, 0.4]", "`;`"),
            ("quadrantChart\n    A: [0.1, 0.2, 0.3]", "2 つ"),
            ("quadrantChart\n    A: [1.0, 0.2]", "`1.0`"),
            ("quadrantChart\n    A: [0.1, .2]", "`.2`"),
            ("quadrantChart\n    A: [0.1, 0.2] size: 3", "`size`"),
            ("quadrantChart\n    A: [0.1, 0.2] color: red", "`red`"),
            ("quadrantChart\n    x-axis a --> \"\"", "空の"),
            ("quadrantChart\n    accDescr { 1 行 }", "1 行で閉じた"),
            ("quadrantChart\n    点だけ", "読めません"),
            (
                "quadrantChart\n    title 甲\n    %%{init: {}}%% title 乙",
                "指示子",
            ),
            (
                "quadrantChart LR\n    A: [0.1, 0.2]",
                "`quadrantChart` の行",
            ),
        ] {
            let error = parse_quadrant(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            let message = format!("{error}");
            assert!(
                message.contains(expected),
                "the reason for `{source}` should mention {expected}: {message}"
            );
        }
    }

    #[test]
    fn a_trailing_separator_and_entities_are_kept_apart_from_statements() {
        let source = "quadrantChart\n    title 題 #9829;\n    A#35;: [0.1, 0.2];";
        let document = parse(source);
        assert_eq!(document.title.as_deref(), Some("題 #9829;"));
        assert_eq!(document.points, vec![point("A#35;", "0.1", "0.2")]);
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "quadrantChart\n    title 題 #9829;\n    A#35;: [0.1, 0.2]"
        );
        assert_renders(&emitted);
    }

    #[test]
    fn empty_statements_draw_nothing_and_are_left_out() {
        let document = parse("quadrantChart\n    title\n    x-axis\n    quadrant-3\n    A: [0, 1]");
        assert_eq!(document.title, None);
        assert_eq!(document.x_axis, None);
        assert_eq!(document.quadrants, [None, None, None, None]);
        assert_eq!(emit_quadrant(&document), "quadrantChart\n    A: [0, 1]");
    }

    #[test]
    fn texts_that_need_quotes_get_them_and_the_rest_are_refused() {
        let mut document = QuadrantDocument::empty();
        document.title = Some("題: 副題".to_string());
        document.x_axis = Some(QuadrantAxis {
            low: "a --> b".to_string(),
            high: Some("c; d".to_string()),
        });
        document.y_axis = Some(QuadrantAxis {
            low: String::new(),
            high: Some("上だけ".to_string()),
        });
        document.quadrants = [
            Some("%% 注意".to_string()),
            Some("\"引用\"で始まらない".to_string()),
            None,
            Some("quadrant-4".to_string()),
        ];
        document.points = vec![
            point("title", "0.5", "0.5"),
            point("x-axis 風", "0", "0"),
            point("a: [b]", "1", "1"),
            QuadrantPoint {
                class: Some("c_1".to_string()),
                ..point("名前:::x", "0.25", "0.75")
            },
            point("%%", "0.1", "0.9"),
        ];
        assert_eq!(document.structure_error(), None);
        let emitted = emit_quadrant(&document);
        assert_eq!(parse(&emitted), document, "{emitted}");
        assert_eq!(emit_quadrant(&parse(&emitted)), emitted);
        assert_renders(&emitted);

        for name in ["", " a", "改行\nあり"] {
            assert!(!is_valid_quadrant_point_name(name), "{name}");
        }
        for title in ["a; b", "a %% b", " a"] {
            assert!(!is_valid_quadrant_title(title), "{title}");
        }
        assert!(is_valid_quadrant_title("\"a; b\""));
        for value in ["0", "1", "0.5", "0.30"] {
            assert!(is_valid_quadrant_coordinate(value), "{value}");
        }
        for value in ["", "1.0", "0.", ".5", "-0.1", "2", "0.5a"] {
            assert!(!is_valid_quadrant_coordinate(value), "{value}");
        }
        assert!(is_valid_quadrant_styles(""));
        assert!(is_valid_quadrant_styles("radius: 10, stroke-width: 2px"));
        assert!(!is_valid_quadrant_styles("radius:10"));
        assert!(!is_valid_quadrant_styles("radius: ten"));
        assert_eq!(
            normalized_quadrant_styles("radius:10 ,color:#fff").as_deref(),
            Some("radius: 10, color: #fff")
        );
        assert!(!is_valid_quadrant_class("a-b"));
    }

    #[test]
    fn structure_errors_catch_what_the_form_can_type() {
        let document = parse(TEMPLATE);
        let edits: [(&str, fn(&mut QuadrantDocument)); 6] = [
            ("タイトル", |document| {
                document.title = Some("a %% b".to_string())
            }),
            ("横軸", |document| {
                document.x_axis = Some(QuadrantAxis {
                    low: "a\"b".to_string(),
                    high: Some("\"".to_string()),
                })
            }),
            ("クラス名", |document| {
                if let Some(point) = document.points.get_mut(0) {
                    point.class = Some("a-b".to_string());
                }
            }),
            ("点の名前", |document| {
                if let Some(point) = document.points.get_mut(0) {
                    point.name = " a".to_string();
                }
            }),
            ("の x", |document| {
                if let Some(point) = document.points.get_mut(0) {
                    point.x = "1.5".to_string();
                }
            }),
            ("スタイル", |document| {
                if let Some(point) = document.points.get_mut(0) {
                    point.styles = "radius: big".to_string();
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
    }

    /// Mermaid rewrites `="…"` inside an HTML tag to `='…'` before it reads
    /// the chart, which takes a `;` in the attribute out of the quotes that
    /// kept it from ending the statement.
    #[test]
    fn html_attributes_in_double_quotes_are_refused() {
        let label = "<span style=\"color:red;\">重要</span>";
        assert!(!is_valid_quadrant_label(label));
        assert!(!is_valid_quadrant_point_name(label));
        assert!(!is_valid_quadrant_title(label));
        assert!(!is_valid_quadrant_axis(&QuadrantAxis {
            low: "低".to_string(),
            high: Some(label.to_string()),
        }));
        let mut document = parse(TEMPLATE);
        document.quadrants[0] = Some(label.to_string());
        let error = document
            .structure_error()
            .unwrap_or_else(|| panic!("a label merman cannot read should be refused"));
        assert!(error.contains("HTML"), "{error}");
        let emitted = emit_quadrant(&document);
        assert!(
            Engine::new()
                .parse_diagram_for_render_model_sync(&emitted, ParseOptions::strict())
                .is_err(),
            "the refused label should be one merman cannot read:\n{emitted}"
        );

        assert!(!is_valid_quadrant_label("<b class=\"x\">重要</b>"));
        let label = "<b class='x'>重要</b>";
        assert!(is_valid_quadrant_label(label));
        document.quadrants[0] = Some(label.to_string());
        assert_eq!(document.structure_error(), None);
        let emitted = assert_stable(&emit_quadrant(&document));
        assert_eq!(parse(&emitted), document, "{emitted}");
        assert_renders(&emitted);

        // A `-->` inside an attribute that is never closed before the `>` is
        // not rewritten, and reads the same way in both.
        assert!(is_valid_quadrant_label("<b c=\"a-->b\">L</b>"));
    }

    #[test]
    fn an_empty_fence_opens_as_a_blank_chart() {
        assert_eq!(parse(""), QuadrantDocument::empty());
        assert_eq!(parse("quadrantChart"), QuadrantDocument::empty());
        assert_eq!(emit_quadrant(&QuadrantDocument::empty()), "quadrantChart");
        assert_renders("quadrantChart");
    }

    #[test]
    fn crlf_and_trailing_whitespace_are_tolerated() {
        let source = "quadrantChart\r\n    title 題  \r\n    A: [0.1, 0.2]  \r\n";
        let document = parse(source);
        assert_eq!(document.title.as_deref(), Some("題"));
        assert_eq!(
            emit_quadrant(&document),
            "quadrantChart\n    title 題\n    A: [0.1, 0.2]"
        );
    }

    #[test]
    fn the_keyword_is_recognized_the_way_merman_recognizes_it() {
        let source = "quadrantChart\n    title x";
        assert!(
            is_quadrant_source(source),
            "{source} should be a quadrant chart"
        );
        for source in [
            "quadrant\n  title x",
            "quadrantchart\n  title x",
            "quadrantCharts",
            "",
            "%% quadrant",
            "flowchart TD\n    A --> B",
        ] {
            assert!(
                !is_quadrant_source(source),
                "{source} should not be a quadrant chart"
            );
        }
        assert!(parse_quadrant("gantt\n    A :a, 2026-01-01, 1d").is_err());
    }

    /// Every quadrant chart in `merman`'s own fixtures either opens, reads the
    /// way `merman` reads it and comes back drawing the same chart, or is
    /// refused with a reason.
    #[test]
    fn merman_fixtures_open_and_round_trip() {
        let fixtures = merman_fixtures("quadrantchart");
        if fixtures.is_empty() {
            eprintln!("merman's fixtures were not found; skipping");
            return;
        }
        let mut opened = 0;
        let mut refused = Vec::new();
        for (path, source) in &fixtures {
            match parse_quadrant(source) {
                Ok(document) => {
                    opened += 1;
                    let emitted = emit_quadrant(&document);
                    let again = parse(&emitted);
                    assert_eq!(again, document, "{path}: {emitted}");
                    assert_eq!(emit_quadrant(&again), emitted, "{path}");
                    assert_eq!(
                        merman_reading(&emitted),
                        merman_reading(source),
                        "{path}: {emitted}"
                    );
                    if document.title.is_some() {
                        assert_eq!(merman_title(source), document.title, "{path}");
                    }
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
