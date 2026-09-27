//! A round-trippable editing model for Mermaid radar charts (`radar-beta`).
//!
//! The shape of this module mirrors [`crate::pie_model`]: [`parse_radar`]
//! reads the body of a ```` ```mermaid ```` fence into [`RadarDocument`], and
//! [`emit_radar`] writes it back. See [`crate::mermaid_model`] for the
//! invariants every diagram kind keeps.
//!
//! `merman` reads a radar chart line by line, and this module reads it the
//! same way, with these decisions of its own:
//!
//! - A curve's values are held in the order of the axes, whichever way the
//!   source wrote them. A curve written with keys (`{b: 2, a: 1}`) is written
//!   back with keys, so that renaming an axis renames the key with it; a key
//!   that names no axis, which Mermaid leaves out, is refused instead.
//! - A curve written as a plain list may hold fewer values than there are
//!   axes (Mermaid draws the missing ones at the minimum) or more (Mermaid
//!   leaves them out). The model keeps it as it is.
//! - `merman` cuts every line at the first `%%`, even inside a label, so a
//!   trailing comment is refused rather than lost on the next emit.
//! - Comment lines right before a `curve` travel with that curve; the others
//!   stay before the first statement or move below the last one.
//!
//! What the parser rewrites, all of it stable from the first emit: the
//! indentation, the `:` after `radar-beta`, a label's quotes (always `"`),
//! several `axis` lines (joined into one), several curves on one line or one
//! curve over several lines (one curve per line), and the order of the
//! statements (title, axes, curves, then the options).

use anyhow::{Result, anyhow, bail};
use merman::diagram::RenderSemanticModel;

use crate::flowchart_model::{PreservedLine, PreservedPosition};
use crate::mermaid_model::{
    INDENT, first_statement_line, join_lines, keyword_tail, scan_preamble, statement_keyword,
};
use crate::xychart_model::{directive_statement_error, statement_after_directives};

const HEADER: &str = "radar-beta";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadarAxis {
    pub id: String,
    /// The label drawn at the end of the axis, or `None` to draw the id.
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadarCurve {
    pub id: String,
    /// The name in the legend, or `None` to show the id.
    pub label: Option<String>,
    /// One value per axis, in the order of the axes, as the source spells
    /// them. An empty string is a gap the user left in the form.
    pub values: Vec<String>,
    /// Whether the values are written with the axis they belong to,
    /// `{a: 1, b: 2}`, rather than in axis order, `{1, 2}`.
    pub keyed: bool,
    /// Comment and accessibility lines written right before the curve, kept
    /// verbatim so that they stay with it.
    pub leading_lines: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadarGraticule {
    Circle,
    Polygon,
}

impl RadarGraticule {
    pub const ALL: [RadarGraticule; 2] = [RadarGraticule::Circle, RadarGraticule::Polygon];

    pub fn keyword(self) -> &'static str {
        match self {
            Self::Circle => "circle",
            Self::Polygon => "polygon",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Circle => "円 (circle)",
            Self::Polygon => "多角形 (polygon)",
        }
    }
}

/// The option statements. `None` leaves an option to Mermaid's default, which
/// is also what the configuration in front of the diagram can change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RadarOptions {
    pub show_legend: Option<bool>,
    pub graticule: Option<RadarGraticule>,
    pub ticks: Option<String>,
    pub max: Option<String>,
    pub min: Option<String>,
}

/// An editable radar chart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadarDocument {
    /// The title, without the `title` keyword. Mermaid draws no title for a
    /// `title` statement with nothing after it, so an empty title is `None`.
    pub title: Option<String>,
    pub axes: Vec<RadarAxis>,
    pub curves: Vec<RadarCurve>,
    pub options: RadarOptions,
    pub preserved_lines: Vec<PreservedLine>,
}

impl RadarDocument {
    /// A blank radar chart, used when the editor is opened on an empty fence.
    pub fn empty() -> Self {
        Self {
            title: None,
            axes: Vec::new(),
            curves: Vec::new(),
            options: RadarOptions::default(),
            preserved_lines: Vec::new(),
        }
    }

    pub fn axis_index(&self, id: &str) -> Option<usize> {
        self.axes.iter().position(|axis| axis.id == id)
    }

    /// Why the document cannot be written back as a diagram Mermaid accepts, in
    /// Japanese, or `None` when it can.
    pub fn structure_error(&self) -> Option<String> {
        if let Some(title) = &self.title
            && !is_valid_radar_title(title)
        {
            return Some(
                "タイトルに `%%` と改行は使えません (前後の空白だけにもできません)。".to_string(),
            );
        }

        let mut axis_ids: Vec<&str> = Vec::new();
        for axis in &self.axes {
            if !is_valid_radar_id(&axis.id) {
                return Some(format!("軸の ID `{}` は使えません。{ID_RULE}", axis.id));
            }
            if axis_ids.contains(&axis.id.as_str()) {
                return Some(format!(
                    "軸の ID `{}` が 2 つあります。値をどちらの軸に付けるか決まらないので、ID を変えてください。",
                    axis.id
                ));
            }
            axis_ids.push(&axis.id);
            if let Some(label) = &axis.label
                && !is_valid_radar_label(label)
            {
                return Some(format!(
                    "軸 `{}` のラベル `{label}` は書けません。{LABEL_RULE}",
                    axis.id
                ));
            }
        }

        let mut curve_ids: Vec<&str> = Vec::new();
        for curve in &self.curves {
            if !is_valid_radar_id(&curve.id) {
                return Some(format!("系列の ID `{}` は使えません。{ID_RULE}", curve.id));
            }
            if curve_ids.contains(&curve.id.as_str()) {
                return Some(format!(
                    "系列の ID `{}` が 2 つあります。ID を変えてください。",
                    curve.id
                ));
            }
            curve_ids.push(&curve.id);
            if let Some(label) = &curve.label
                && !is_valid_radar_label(label)
            {
                return Some(format!(
                    "系列 `{}` のラベル `{label}` は書けません。{LABEL_RULE}",
                    curve.id
                ));
            }
            if curve.keyed {
                if self.axes.is_empty() {
                    return Some(format!(
                        "系列 `{}` は軸の名前付きで値を書いていますが、軸がありません。",
                        curve.id
                    ));
                }
                if curve.values.len() != self.axes.len()
                    || curve.values.iter().any(String::is_empty)
                {
                    return Some(format!(
                        "系列 `{}` は軸の名前付きで値を書いているので、すべての軸に値が必要です。",
                        curve.id
                    ));
                }
            }
            for (position, value) in curve.values.iter().enumerate() {
                if value.is_empty() {
                    return Some(format!(
                        "系列 `{}` の {} 番目の値が空です。途中の値は空にできません (末尾の空欄は値なしとして扱います)。",
                        curve.id,
                        position + 1
                    ));
                }
                if !is_valid_radar_number(value) {
                    return Some(format!(
                        "系列 `{}` の値 `{value}` は書けません。{NUMBER_RULE}",
                        curve.id
                    ));
                }
            }
        }

        for (name, value) in [
            ("目盛りの数 (ticks)", &self.options.ticks),
            ("最大 (max)", &self.options.max),
            ("最小 (min)", &self.options.min),
        ] {
            if let Some(value) = value
                && !is_valid_radar_number(value)
            {
                return Some(format!("{name} の `{value}` は書けません。{NUMBER_RULE}"));
            }
        }
        // Mermaid draws a ring per tick, so a mistyped count would draw
        // millions of them and never finish.
        if let Some(ticks) = &self.options.ticks
            && ticks
                .parse::<f64>()
                .is_ok_and(|ticks| ticks > MAX_TICKS as f64)
        {
            return Some(format!(
                "目盛りの数 (ticks) `{ticks}` が多すぎます。{MAX_TICKS} 以下にしてください。"
            ));
        }
        None
    }
}

const MAX_TICKS: u32 = 100;

const ID_RULE: &str =
    "半角英字か `_` で始め、半角英数字・`_`・`-` だけにしてください (末尾の `-` は不可)。";
const LABEL_RULE: &str = "`%%` と改行は使えず、空にもできません。";
const NUMBER_RULE: &str =
    "0 以上の数を `12`・`0.5`・`.5` の形で書いてください (先頭の 0、末尾の `.`、符号は不可)。";

/// Whether `source` is a radar chart: its first statement is `radar-beta`.
///
/// `merman` also takes the keyword with a colon after it, `radar-beta:`.
pub fn is_radar_source(source: &str) -> bool {
    first_statement_line(source).is_some_and(|line| {
        keyword_tail(line, HEADER).is_some()
            || line
                .strip_prefix(HEADER)
                .is_some_and(|rest| rest.trim_start().starts_with(':'))
    })
}

/// Reads `source` — the body of a ```` ```mermaid ```` fence — into an editable
/// document.
pub fn parse_radar(source: &str) -> Result<RadarDocument> {
    let document = scan_radar(source)?;

    // `merman` is asked last, so that a construct this module knows about is
    // reported with the reason that says what to do.
    if !source.trim().is_empty() {
        let parsed = crate::parse_for_render_model(source)
            .map_err(|error| {
                anyhow!("Mermaid の構文として読めませんでした ({error})。テキストのまま編集してください。")
            })?
            .ok_or_else(|| anyhow!("Mermaid の図として解釈できませんでした。"))?;
        if !matches!(parsed.model, RenderSemanticModel::Radar(_)) {
            bail!("この図はレーダーチャートではありません。");
        }
    }

    Ok(document)
}

/// A curve as the source writes it, before its values are put in axis order.
struct CurveSource {
    id: String,
    label: Option<String>,
    entries: Vec<(Option<String>, String)>,
    leading_lines: Vec<String>,
    statement: String,
}

#[derive(Default)]
struct Seen {
    any: bool,
    title: bool,
}

fn scan_radar(source: &str) -> Result<RadarDocument> {
    let lines = source.lines().collect::<Vec<_>>();
    let preamble = scan_preamble(&lines);
    let mut document = RadarDocument::empty();
    document.preserved_lines = preamble.preserved;

    let Some(header_index) = preamble.header else {
        return Ok(document);
    };

    let header = lines.get(header_index).copied().unwrap_or_default().trim();
    let Some(tail) = header.strip_prefix(HEADER) else {
        bail!("レーダーチャートではありません (`radar-beta` で始まる図だけを構造化編集できます)。");
    };
    let tail = tail.trim();
    if !tail.is_empty() && tail != ":" {
        bail!(
            "`radar-beta` の行の `{tail}` は構造化編集に対応していません。テキストのまま編集してください。"
        );
    }

    let mut seen = Seen::default();
    let mut curves: Vec<CurveSource> = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    let mut index = header_index + 1;
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
            let block = join_lines(lines.get(start..index).unwrap_or_default());
            if let Some(statement) = statement_after_directives(&block) {
                bail!("{}", directive_statement_error(statement));
            }
            pending.push(block);
            continue;
        }
        if trimmed.contains("%%") {
            bail!(
                "文の末尾のコメントは構造化編集で失われます ({trimmed})。コメントを独立した行に移すか、テキストのまま編集してください。"
            );
        }
        if is_accessibility_statement(trimmed) {
            if statement_keyword(trimmed, "accDescr").is_some_and(|rest| rest.starts_with('{'))
                && !trimmed.contains('}')
            {
                while lines.get(index).is_some_and(|line| !line.contains('}')) {
                    index += 1;
                }
                if index < lines.len() {
                    index += 1;
                }
            }
            pending.push(join_lines(lines.get(start..index).unwrap_or_default()));
            continue;
        }

        if let Some(rest) = word_rest(trimmed, "title") {
            if seen.title {
                bail!("`title` が 2 回書かれています。1 つにまとめてからもう一度開いてください。");
            }
            seen.title = true;
            let title = rest.trim();
            document.title = (!title.is_empty()).then(|| title.to_string());
            flush_pending(&mut document, &mut pending, seen.any);
            seen.any = true;
            continue;
        }
        if let Some(rest) = word_rest(trimmed, "axis") {
            document.axes.extend(parse_axes(rest.trim(), trimmed)?);
            flush_pending(&mut document, &mut pending, seen.any);
            seen.any = true;
            continue;
        }
        if let Some(rest) = word_rest(trimmed, "curve") {
            // A curve's braces may close on a later line; Mermaid joins the
            // lines until they do.
            let mut statement = rest.to_string();
            while !braces_closed(&statement) {
                let Some(next) = lines.get(index).copied() else {
                    break;
                };
                if next.contains("%%") {
                    bail!(
                        "複数行にわたる `curve` の途中のコメントは構造化編集で失われます ({trimmed})。コメントを外へ移すか、テキストのまま編集してください。"
                    );
                }
                statement.push('\n');
                statement.push_str(next);
                index += 1;
            }
            let mut parsed = parse_curves(&statement, trimmed)?;
            if let Some(first) = parsed.first_mut() {
                first.leading_lines = std::mem::take(&mut pending);
            }
            curves.extend(parsed);
            seen.any = true;
            continue;
        }
        if parse_options(trimmed, &mut document.options)? {
            flush_pending(&mut document, &mut pending, seen.any);
            seen.any = true;
            continue;
        }
        bail!(
            "レーダーチャートの文として読めませんでした ({trimmed})。テキストのまま編集してください。"
        );
    }
    flush_pending(&mut document, &mut pending, seen.any);

    for curve in curves {
        let values = order_entries(&document.axes, &curve)?;
        let keyed = curve
            .entries
            .first()
            .is_some_and(|(axis, _)| axis.is_some());
        document.curves.push(RadarCurve {
            id: curve.id,
            label: curve.label,
            values,
            keyed,
            leading_lines: curve.leading_lines,
        });
    }

    Ok(document)
}

fn flush_pending(document: &mut RadarDocument, pending: &mut Vec<String>, after_statement: bool) {
    let position = if after_statement {
        PreservedPosition::Bottom
    } else {
        PreservedPosition::Top
    };
    for text in pending.drain(..) {
        document
            .preserved_lines
            .push(PreservedLine { position, text });
    }
}

fn is_accessibility_statement(statement: &str) -> bool {
    statement_keyword(statement, "accTitle").is_some()
        || statement_keyword(statement, "accDescr").is_some()
}

/// What follows `keyword` at the start of a line. Mermaid matches the word
/// without asking where it ends, so `titles` is a title of `s`; this module
/// only takes the word when whitespace or the end of the line follows it, and
/// refuses the rest.
fn word_rest<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(keyword)?;
    if rest.is_empty() || rest.starts_with(|character: char| character.is_whitespace()) {
        return Some(rest);
    }
    None
}

fn parse_axes(list: &str, line: &str) -> Result<Vec<RadarAxis>> {
    if list.is_empty() {
        bail!("`axis` に軸がありません ({line})。");
    }
    let mut axes = Vec::new();
    for part in split_top_level(list, ',') {
        let mut scanner = Scanner::new(part.trim());
        let Some(id) = scanner.id() else {
            bail!("軸の ID として読めませんでした ({line})。");
        };
        let label = scanner.bracketed_label(line)?;
        if !scanner.at_end() {
            bail!(
                "軸 `{id}` の後の `{}` は読めません ({line})。",
                scanner.rest()
            );
        }
        axes.push(RadarAxis { id, label });
    }
    Ok(axes)
}

fn parse_curves(statement: &str, line: &str) -> Result<Vec<CurveSource>> {
    if statement.trim().is_empty() {
        bail!("`curve` に系列がありません ({line})。");
    }
    let mut curves = Vec::new();
    for part in split_top_level(statement, ',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let mut scanner = Scanner::new(part);
        let Some(id) = scanner.id() else {
            bail!("系列の ID として読めませんでした ({line})。");
        };
        let label = scanner.bracketed_label(line)?;
        let Some(body) = scanner.rest().strip_prefix('{') else {
            bail!("系列 `{id}` の値が `{{1, 2, 3}}` の形で書かれていません ({line})。");
        };
        let Some(inner) = body.trim_end().strip_suffix('}') else {
            bail!("系列 `{id}` の `}}` の後に読めない文字があります ({line})。");
        };
        let mut entries = Vec::new();
        for entry in split_top_level(inner, ',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            entries.push(parse_entry(entry, &id, line)?);
        }
        let keyed = entries.iter().any(|(axis, _)| axis.is_some());
        let plain = entries.iter().any(|(axis, _)| axis.is_none());
        if keyed && plain {
            bail!("系列 `{id}` で軸の名前付きの値と名前なしの値が混ざっています ({line})。");
        }
        curves.push(CurveSource {
            id,
            label,
            entries,
            leading_lines: Vec::new(),
            statement: line.to_string(),
        });
    }
    Ok(curves)
}

/// One entry of a curve: `3`, or `axis: 3` / `axis 3`.
fn parse_entry(entry: &str, curve: &str, line: &str) -> Result<(Option<String>, String)> {
    if is_valid_radar_number(entry) {
        return Ok((None, entry.to_string()));
    }
    let mut scanner = Scanner::new(entry);
    if let Some(axis) = scanner.id() {
        let rest = scanner.rest().trim_start();
        let value = rest.strip_prefix(':').unwrap_or(rest).trim();
        if is_valid_radar_number(value) {
            return Ok((Some(axis), value.to_string()));
        }
    }
    bail!("系列 `{curve}` の値 `{entry}` は読めません ({line})。{NUMBER_RULE}")
}

/// Puts a curve's values in the order of the axes, the way Mermaid draws them.
fn order_entries(axes: &[RadarAxis], curve: &CurveSource) -> Result<Vec<String>> {
    if curve.entries.first().is_none_or(|(axis, _)| axis.is_none()) {
        return Ok(curve
            .entries
            .iter()
            .map(|(_, value)| value.clone())
            .collect());
    }
    let line = &curve.statement;
    let mut used = Vec::new();
    for (axis, _) in &curve.entries {
        let Some(axis) = axis else {
            continue;
        };
        if !axes.iter().any(|candidate| &candidate.id == axis) {
            bail!(
                "系列 `{}` の `{axis}` という軸はありません ({line})。Mermaid はこの値を描かないので、消すか軸を足してから開いてください。",
                curve.id
            );
        }
        if used.contains(&axis) {
            bail!(
                "系列 `{}` に軸 `{axis}` の値が 2 つあります ({line})。Mermaid は 1 つ目だけを描くので、1 つにしてから開いてください。",
                curve.id
            );
        }
        used.push(axis);
    }
    axes.iter()
        .map(|axis| {
            curve
                .entries
                .iter()
                .find(|(key, _)| key.as_deref() == Some(axis.id.as_str()))
                .map(|(_, value)| value.clone())
                .ok_or_else(|| {
                    anyhow!(
                        "系列 `{}` に軸 `{}` の値がありません ({line})。",
                        curve.id,
                        axis.id
                    )
                })
        })
        .collect()
}

/// Reads an option line, `max 5` or `max 5, min 1`, and says whether it was
/// one.
fn parse_options(line: &str, options: &mut RadarOptions) -> Result<bool> {
    let parts = split_top_level(line, ',');
    let mut parsed = Vec::new();
    for part in &parts {
        let part = part.trim();
        if part.is_empty() && parts.len() > 1 {
            continue;
        }
        let mut scanner = Scanner::new(part);
        let Some(name) = scanner.id() else {
            return Ok(false);
        };
        if !matches!(
            name.as_str(),
            "showLegend" | "ticks" | "max" | "min" | "graticule"
        ) {
            return Ok(false);
        }
        parsed.push((name, scanner.rest().trim().to_string()));
    }
    if parsed.is_empty() {
        return Ok(false);
    }
    for (name, value) in parsed {
        let twice = || {
            anyhow!(
                "`{name}` が 2 回書かれています ({line})。1 つにまとめてからもう一度開いてください。"
            )
        };
        match name.as_str() {
            "showLegend" => {
                let shown = match value.as_str() {
                    "true" => true,
                    "false" => false,
                    _ => bail!("`showLegend` は `true` か `false` で書いてください ({line})。"),
                };
                if options.show_legend.replace(shown).is_some() {
                    return Err(twice());
                }
            }
            "graticule" => {
                let Some(graticule) = RadarGraticule::ALL
                    .into_iter()
                    .find(|graticule| graticule.keyword() == value)
                else {
                    bail!("`graticule` は `circle` か `polygon` で書いてください ({line})。");
                };
                if options.graticule.replace(graticule).is_some() {
                    return Err(twice());
                }
            }
            _ => {
                if !is_valid_radar_number(&value) {
                    bail!("`{name}` の `{value}` は読めません ({line})。{NUMBER_RULE}");
                }
                let slot = match name.as_str() {
                    "ticks" => &mut options.ticks,
                    "max" => &mut options.max,
                    _ => &mut options.min,
                };
                if slot.replace(value).is_some() {
                    return Err(twice());
                }
            }
        }
    }
    Ok(true)
}

/// Splits `text` at `delimiter` where it is outside quotes, braces and
/// brackets, the way `merman`'s radar parser does.
fn split_top_level(text: &str, delimiter: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut depth = 0i32;
    let mut start = 0;
    for (offset, character) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if let Some(open) = quote {
            if character == '\\' {
                escaped = true;
            } else if character == open {
                quote = None;
            }
            continue;
        }
        match character {
            '"' | '\'' => quote = Some(character),
            '{' | '[' => depth += 1,
            '}' | ']' => depth -= 1,
            _ if character == delimiter && depth == 0 => {
                parts.push(text.get(start..offset).unwrap_or_default());
                start = offset + character.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(text.get(start..).unwrap_or_default());
    parts
}

/// Whether every `{` outside quotes has its `}`.
fn braces_closed(text: &str) -> bool {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut depth = 0i32;
    for character in text.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if let Some(open) = quote {
            if character == '\\' {
                escaped = true;
            } else if character == open {
                quote = None;
            }
            continue;
        }
        match character {
            '"' | '\'' => quote = Some(character),
            '{' => depth += 1,
            '}' => depth -= 1,
            _ => {}
        }
    }
    depth <= 0
}

/// Reads the tokens `merman`'s radar parser reads: ids, `["labels"]`.
struct Scanner<'a> {
    text: &'a str,
}

impl<'a> Scanner<'a> {
    fn new(text: &'a str) -> Self {
        Self { text }
    }

    fn rest(&self) -> &'a str {
        self.text
    }

    fn at_end(&self) -> bool {
        self.text.trim().is_empty()
    }

    fn id(&mut self) -> Option<String> {
        let text = self.text.trim_start();
        let length = text
            .char_indices()
            .take_while(|(offset, character)| {
                if *offset == 0 {
                    character.is_ascii_alphabetic() || *character == '_'
                } else {
                    character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
                }
            })
            .count();
        let id = text.get(..length)?;
        if !is_valid_radar_id(id) {
            return None;
        }
        self.text = text.get(length..)?.trim_start();
        Some(id.to_string())
    }

    /// An optional `["label"]` (or `['label']`), with `\` escapes.
    fn bracketed_label(&mut self, line: &str) -> Result<Option<String>> {
        let Some(after) = self.text.trim_start().strip_prefix('[') else {
            return Ok(None);
        };
        let after = after.trim_start();
        let mut characters = after.char_indices();
        let Some((_, quote)) = characters.next() else {
            bail!("ラベルが `[\"…\"]` の形で書かれていません ({line})。");
        };
        if quote != '"' && quote != '\'' {
            bail!("ラベルが `[\"…\"]` の形で書かれていません ({line})。");
        }
        let mut label = String::new();
        let mut escaped = false;
        for (offset, character) in characters {
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
                let rest = after
                    .get(offset + character.len_utf8()..)
                    .unwrap_or_default();
                let Some(rest) = rest.trim_start().strip_prefix(']') else {
                    bail!("ラベルの後に `]` がありません ({line})。");
                };
                self.text = rest.trim_start();
                if label.is_empty() {
                    bail!(
                        "空のラベル `[\"\"]` は構造化編集に対応していません ({line})。ラベルを書くか `[\"\"]` を消してから開いてください。"
                    );
                }
                return Ok(Some(label));
            }
            label.push(character);
        }
        bail!("閉じていないラベルがあります ({line})。")
    }
}

/// Writes the document back as Mermaid source, without a trailing newline.
pub fn emit_radar(document: &RadarDocument) -> String {
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
        out.push_str(&format!("{INDENT}title {title}\n"));
    }
    if !document.axes.is_empty() {
        let axes = document
            .axes
            .iter()
            .map(|axis| with_label(&axis.id, axis.label.as_deref()))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("{INDENT}axis {axes}\n"));
    }
    for curve in &document.curves {
        for line in &curve.leading_lines {
            out.push_str(line);
            out.push('\n');
        }
        let values = if curve.keyed {
            document
                .axes
                .iter()
                .zip(&curve.values)
                .map(|(axis, value)| format!("{}: {value}", axis.id))
                .collect::<Vec<_>>()
        } else {
            curve.values.clone()
        };
        out.push_str(&format!(
            "{INDENT}curve {}{{{}}}\n",
            with_label(&curve.id, curve.label.as_deref()),
            values.join(", ")
        ));
    }

    let options = &document.options;
    if let Some(shown) = options.show_legend {
        out.push_str(&format!("{INDENT}showLegend {shown}\n"));
    }
    if let Some(graticule) = options.graticule {
        out.push_str(&format!("{INDENT}graticule {}\n", graticule.keyword()));
    }
    for (name, value) in [
        ("ticks", &options.ticks),
        ("max", &options.max),
        ("min", &options.min),
    ] {
        if let Some(value) = value {
            out.push_str(&format!("{INDENT}{name} {value}\n"));
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

fn with_label(id: &str, label: Option<&str>) -> String {
    match label {
        Some(label) => format!(
            "{id}[\"{}\"]",
            label.replace('\\', "\\\\").replace('"', "\\\"")
        ),
        None => id.to_string(),
    }
}

fn preserved_at(
    document: &RadarDocument,
    position: PreservedPosition,
) -> impl Iterator<Item = &str> {
    document
        .preserved_lines
        .iter()
        .filter(move |line| line.position == position)
        .map(|line| line.text.as_str())
}

/// Whether `id` can name an axis or a curve: a letter or `_`, then letters,
/// digits, `_` and `-`, not ending in `-`.
pub fn is_valid_radar_id(id: &str) -> bool {
    let mut characters = id.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && characters
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
        && !id.ends_with('-')
}

/// Whether `label` can be written inside `["…"]`. The quote and the
/// backslash are escaped, but `merman` cuts the line at `%%` wherever it is.
pub fn is_valid_radar_label(label: &str) -> bool {
    !label.is_empty() && !label.contains("%%") && !label.contains(['\n', '\r'])
}

/// Whether `title` can be written after `title`, which runs to the end of the
/// line.
pub fn is_valid_radar_title(title: &str) -> bool {
    !title.trim().is_empty()
        && title.trim() == title
        && !title.contains("%%")
        && !title.contains(['\n', '\r'])
}

/// Whether `text` is a number the way `merman`'s radar parser reads one:
/// digits without a leading zero, or with one dot that is not the last
/// character. There is no sign.
pub fn is_valid_radar_number(text: &str) -> bool {
    let Some(first) = text.chars().next() else {
        return false;
    };
    if !text
        .chars()
        .all(|character| character.is_ascii_digit() || character == '.')
        || text.matches('.').count() > 1
    {
        return false;
    }
    if text.contains('.') {
        return !text.ends_with('.') && text != ".";
    }
    // `merman` reads a number without a dot as a 64-bit integer.
    !(first == '0' && text.len() > 1) && text.parse::<i64>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MermaidTheme, render_to_svg};
    use merman::{Engine, ParseOptions};

    /// The template `markdown::InsertMermaidDiagram` writes, which has to come
    /// back byte for byte. The editor's tests read it from the action itself.
    const TEMPLATE: &str = "radar-beta\n    %% axis で軸、curve で系列。値は軸の順に並べる\n    title スキルの比較\n    axis design[\"設計\"], build[\"実装\"], test[\"テスト\"], ops[\"運用\"]\n    curve a[\"Aさん\"]{4, 3, 5, 2}\n    curve b[\"Bさん\"]{2, 5, 3, 4}\n    max 5";

    fn parse(source: &str) -> RadarDocument {
        parse_radar(source).unwrap_or_else(|error| panic!("{source} should parse: {error}"))
    }

    fn assert_stable(source: &str) -> String {
        let first = parse(source);
        let first_text = emit_radar(&first);
        let second = parse_radar(&first_text)
            .unwrap_or_else(|error| panic!("emitted source should parse: {error}\n{first_text}"));
        assert_eq!(first, second, "the document changed through:\n{first_text}");
        assert_eq!(emit_radar(&second), first_text);
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

    fn merman_model(source: &str) -> Option<merman::diagrams::radar::RadarDiagramRenderModel> {
        let parsed = Engine::new()
            .parse_diagram_for_render_model_sync(source, ParseOptions::strict())
            .ok()??;
        match parsed.model {
            RenderSemanticModel::Radar(model) => Some(model),
            _ => None,
        }
    }

    fn number(text: &str) -> f64 {
        text.parse::<f64>()
            .unwrap_or_else(|error| panic!("`{text}` should be a number: {error}"))
    }

    fn json_number(value: &serde_json::Value) -> f64 {
        value
            .as_f64()
            .unwrap_or_else(|| panic!("{value} should be a number"))
    }

    /// What Mermaid draws from `source` is what the document says: the same
    /// axes, curves (value for value, in axis order) and options.
    fn assert_matches_merman(source: &str, document: &RadarDocument) {
        let model = merman_model(source).unwrap_or_else(|| panic!("merman should read {source}"));
        assert_eq!(
            model.title.as_deref().filter(|title| !title.is_empty()),
            document.title.as_deref(),
            "title of {source}"
        );
        assert_eq!(
            model
                .axes
                .iter()
                .map(|axis| (axis.name.clone(), axis.label.clone()))
                .collect::<Vec<_>>(),
            document
                .axes
                .iter()
                .map(|axis| (
                    axis.id.clone(),
                    axis.label.clone().unwrap_or_else(|| axis.id.clone())
                ))
                .collect::<Vec<_>>(),
            "{source}"
        );
        assert_eq!(model.curves.len(), document.curves.len(), "{source}");
        for (theirs, ours) in model.curves.iter().zip(&document.curves) {
            assert_eq!(theirs.name, ours.id, "{source}");
            assert_eq!(
                theirs.label,
                ours.label.clone().unwrap_or_else(|| ours.id.clone()),
                "{source}"
            );
            assert_eq!(
                theirs.entries.iter().map(json_number).collect::<Vec<_>>(),
                ours.values
                    .iter()
                    .map(|value| number(value))
                    .collect::<Vec<_>>(),
                "{source}"
            );
        }
        let options = &document.options;
        assert_eq!(
            model.options.show_legend,
            options.show_legend.unwrap_or(true),
            "{source}"
        );
        assert_eq!(
            model.options.graticule,
            options
                .graticule
                .unwrap_or(RadarGraticule::Circle)
                .keyword(),
            "{source}"
        );
        assert_eq!(
            json_number(&model.options.ticks),
            options.ticks.as_deref().map_or(5.0, number),
            "{source}"
        );
        assert_eq!(
            json_number(&model.options.min),
            options.min.as_deref().map_or(0.0, number),
            "{source}"
        );
        assert_eq!(
            model.options.max.as_ref().map(json_number),
            options.max.as_deref().map(number),
            "{source}"
        );
    }

    /// Every radar chart `merman` ships: each one the editor opens reads as
    /// what Mermaid draws, is written back stably as the same chart, and still
    /// draws. The ones it refuses are listed with their reasons.
    #[test]
    fn merman_examples_round_trip() {
        let fixtures = crate::merman_fixtures("radar");
        if fixtures.is_empty() {
            return;
        }
        assert!(
            fixtures.len() > 30,
            "found only {} examples",
            fixtures.len()
        );

        let mut opened = 0;
        let mut merman_refused = 0;
        let mut refused = Vec::new();
        for (path, source) in &fixtures {
            if merman_model(source).is_none() {
                merman_refused += 1;
                assert!(parse_radar(source).is_err(), "{path:?}");
                continue;
            }
            let document = match parse_radar(source) {
                Ok(document) => document,
                Err(error) => {
                    refused.push(format!("{}: {error}", path.display()));
                    continue;
                }
            };
            opened += 1;
            assert_matches_merman(source, &document);
            let emitted = assert_stable(source);
            assert_matches_merman(&emitted, &document);
            assert_renders(&emitted);
        }
        eprintln!(
            "radar examples: {opened} opened, {merman_refused} refused by merman, {} refused by the editor:\n{}",
            refused.len(),
            refused.join("\n")
        );
        assert!(
            refused.len() * 10 <= opened,
            "too many examples refused:\n{}",
            refused.join("\n")
        );
    }

    #[test]
    fn the_insert_template_round_trips_byte_for_byte() {
        let document = parse(TEMPLATE);
        assert_eq!(emit_radar(&document), TEMPLATE);
        assert_eq!(document.title.as_deref(), Some("スキルの比較"));
        assert_eq!(
            document
                .axes
                .iter()
                .map(|axis| (axis.id.as_str(), axis.label.as_deref()))
                .collect::<Vec<_>>(),
            vec![
                ("design", Some("設計")),
                ("build", Some("実装")),
                ("test", Some("テスト")),
                ("ops", Some("運用")),
            ]
        );
        assert_eq!(document.options.max.as_deref(), Some("5"));
        assert_matches_merman(TEMPLATE, &document);
        assert_renders(TEMPLATE);
    }

    #[test]
    fn keyed_values_are_put_in_axis_order_and_follow_a_rename() {
        let source = "radar-beta\n  axis A, B, C\n  curve c{ C: 3, A: 1, B: 2 }";
        let mut document = parse(source);
        let curve = document.curves.first().cloned().expect("one curve");
        assert!(curve.keyed);
        assert_eq!(curve.values, vec!["1", "2", "3"]);
        assert_eq!(
            assert_stable(source),
            "radar-beta\n    axis A, B, C\n    curve c{A: 1, B: 2, C: 3}"
        );

        if let Some(axis) = document.axes.get_mut(1) {
            axis.id = "speed".to_string();
        }
        let emitted = emit_radar(&document);
        assert_eq!(
            emitted,
            "radar-beta\n    axis A, speed, C\n    curve c{A: 1, speed: 2, C: 3}"
        );
        assert_matches_merman(&emitted, &parse(&emitted));
    }

    #[test]
    fn several_statements_on_a_line_are_written_one_per_line() {
        let source = "radar-beta:\n  axis a[\"A\"]\n  axis b['B \\' q'], c\n  curve x{1, 2, 3}, y[\"Y\"]{\n    3, 2, 1\n  }\n  ticks 3\n  max 8";
        let document = parse(source);
        assert_eq!(document.axes.len(), 3);
        assert_eq!(
            document.axes.get(1).and_then(|axis| axis.label.as_deref()),
            Some("B ' q")
        );
        assert_eq!(document.curves.len(), 2);
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "radar-beta\n    axis a[\"A\"], b[\"B ' q\"], c\n    curve x{1, 2, 3}\n    curve y[\"Y\"]{3, 2, 1}\n    ticks 3\n    max 8"
        );
        assert_matches_merman(&emitted, &document);
        assert_renders(&emitted);
    }

    #[test]
    fn preserved_lines_keep_their_place() {
        let source = "%%{init: {'theme': 'forest'} }%%\nradar-beta\n  %% 先頭\n  title 図\n  %% 途中\n  axis a, b\n  %% 系列のメモ\n  curve x{1, 2}\n  accTitle: 概要\n  max 3\n  %% 末尾";
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
                    "%%{init: {'theme': 'forest'} }%%".to_string()
                ),
                (PreservedPosition::Top, "%% 先頭".to_string()),
                (PreservedPosition::Bottom, "%% 途中".to_string()),
                (PreservedPosition::Bottom, "accTitle: 概要".to_string()),
                (PreservedPosition::Bottom, "%% 末尾".to_string()),
            ]
        );
        assert_eq!(
            document
                .curves
                .first()
                .map(|curve| curve.leading_lines.clone()),
            Some(vec!["  %% 系列のメモ".to_string()])
        );
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "%%{init: {'theme': 'forest'} }%%\nradar-beta\n  %% 先頭\n    title 図\n    axis a, b\n  %% 系列のメモ\n    curve x{1, 2}\n    max 3\n  %% 途中\n  accTitle: 概要\n  %% 末尾"
        );
        assert_renders(&emitted);
    }

    #[test]
    fn a_value_count_that_differs_from_the_axes_is_kept() {
        let source =
            "radar-beta\n  axis a, b, c\n  curve x{1}\n  curve y{1, 2, 3, 4}\n  curve z{\n  }";
        let document = parse(source);
        assert_eq!(document.structure_error(), None);
        let emitted = assert_stable(source);
        assert_eq!(
            emitted,
            "radar-beta\n    axis a, b, c\n    curve x{1}\n    curve y{1, 2, 3, 4}\n    curve z{}"
        );
        assert_matches_merman(&emitted, &document);
    }

    #[test]
    fn constructs_that_would_change_are_refused() {
        for (source, reason) in [
            (
                "radar-beta\n  axis a, b\n  curve x{a: 1, b: 2, c: 3}",
                "`c` という軸はありません",
            ),
            (
                "radar-beta\n  axis a, b\n  curve x{a: 1, a: 2, b: 3}",
                "2 つあります",
            ),
            ("radar-beta\n  axis a\n  curve x{1} %% メモ", "コメント"),
            ("radar-beta\n  title a\n  title b", "2 回"),
            ("radar-beta\n  max 5\n  max 6", "2 回"),
            ("radar-beta\n  axis a[\"\"]", "空のラベル"),
            (
                "radar-beta\n  axis a\n  curve x{\n  %% メモ\n  1}",
                "コメント",
            ),
            ("radar-beta\n  titles a", "読めませんでした"),
        ] {
            let error = parse_radar(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            let message = format!("{error}");
            assert!(
                message.contains(reason),
                "the reason for `{source}` should say `{reason}`: {message}"
            );
        }
        for source in [
            "radar-beta\n  axis a, b,",
            "radar-beta\n  axis a\n  curve x{1, a: 2}",
            "radar-beta\n  axis a, b\n  curve x{a: 1}",
            "radar-beta\n  showLegend yes",
            "radar-beta\n  curve x{-1}",
            // `merman` reads an option line as one option before it tries a
            // list, so a list of options never reaches the list reader.
            "radar-beta\n  ticks 3, max 8",
        ] {
            assert!(parse_radar(source).is_err(), "{source}");
        }
    }

    #[test]
    fn structure_errors_are_reported() {
        let base = parse(TEMPLATE);
        let cases: Vec<(Box<dyn Fn(&mut RadarDocument)>, &str)> = vec![
            (
                Box::new(|document| {
                    if let Some(axis) = document.axes.get_mut(0) {
                        axis.id = "1st".to_string();
                    }
                }),
                "軸の ID `1st`",
            ),
            (
                Box::new(|document| {
                    if let Some(axis) = document.axes.get_mut(1) {
                        axis.id = "design".to_string();
                    }
                }),
                "2 つあります",
            ),
            (
                Box::new(|document| {
                    if let Some(curve) = document.curves.get_mut(1) {
                        curve.id = "a".to_string();
                    }
                }),
                "系列の ID `a` が 2 つ",
            ),
            (
                Box::new(|document| {
                    if let Some(curve) = document.curves.get_mut(0) {
                        curve.values = vec!["4".to_string(), String::new(), "5".to_string()];
                    }
                }),
                "2 番目の値が空",
            ),
            (
                Box::new(|document| {
                    if let Some(curve) = document.curves.get_mut(0) {
                        curve.keyed = true;
                        curve.values.pop();
                    }
                }),
                "すべての軸に値が必要",
            ),
            (
                Box::new(|document| {
                    document.options.max = Some("-1".to_string());
                }),
                "最大 (max)",
            ),
            (
                Box::new(|document| {
                    if let Some(axis) = document.axes.get_mut(0) {
                        axis.label = Some("50%% 以上".to_string());
                    }
                }),
                "ラベル",
            ),
        ];
        for (change, reason) in cases {
            let mut document = base.clone();
            change(&mut document);
            let error = document
                .structure_error()
                .unwrap_or_else(|| panic!("`{reason}` should be reported"));
            assert!(error.contains(reason), "{error} should say {reason}");
        }
        assert_eq!(base.structure_error(), None);
    }

    #[test]
    fn labels_with_quotes_and_multibyte_text_survive() {
        for label in [
            "引用 \" あり",
            "バックスラッシュ \\ あり",
            "カンマ, あり",
            "波かっこ { } あり",
            "日本語のラベル",
        ] {
            let document = RadarDocument {
                title: None,
                axes: vec![
                    RadarAxis {
                        id: "a".to_string(),
                        label: Some(label.to_string()),
                    },
                    RadarAxis {
                        id: "b".to_string(),
                        label: None,
                    },
                ],
                curves: vec![RadarCurve {
                    id: "c".to_string(),
                    label: Some(label.to_string()),
                    values: vec!["1".to_string(), "2".to_string()],
                    keyed: false,
                    leading_lines: Vec::new(),
                }],
                options: RadarOptions::default(),
                preserved_lines: Vec::new(),
            };
            let emitted = emit_radar(&document);
            let parsed = parse_radar(&emitted)
                .unwrap_or_else(|error| panic!("`{label}` should parse: {error}\n{emitted}"));
            assert_eq!(parsed, document, "{emitted}");
            assert_matches_merman(&emitted, &parsed);
            assert_renders(&emitted);
        }
    }

    #[test]
    fn numbers_and_ids_are_validated() {
        for value in ["0", "5", "12", "0.5", ".5", "10.25"] {
            assert!(is_valid_radar_number(value), "{value}");
        }
        for value in ["", "05", "5.", "-1", "+1", "1e3", "1.2.3", ".", "５"] {
            assert!(!is_valid_radar_number(value), "{value}");
        }
        for id in ["a", "_a", "a-b", "a1", "A_B"] {
            assert!(is_valid_radar_id(id), "{id}");
        }
        for id in ["", "1a", "a-", "a b", "軸", "-a"] {
            assert!(!is_valid_radar_id(id), "{id}");
        }
    }

    #[test]
    fn an_empty_fence_opens_as_a_blank_chart() {
        let document = parse("");
        assert_eq!(document, RadarDocument::empty());
        assert_eq!(emit_radar(&document), "radar-beta");
        assert_renders("radar-beta");
    }

    #[test]
    fn the_keyword_is_recognized_the_way_merman_recognizes_it() {
        for source in [
            "radar-beta\n    axis A, B",
            "radar-beta:\n    axis A, B",
            "radar-beta :\n    axis A",
        ] {
            assert!(is_radar_source(source), "{source} should be a radar chart");
        }
        for source in [
            "radar\n  axis A",
            "radar-betas",
            "Radar-beta",
            "",
            "%% radar",
            "flowchart TD\n    A --> B",
        ] {
            assert!(
                !is_radar_source(source),
                "{source} should not be a radar chart"
            );
        }
    }

    /// Mermaid cuts a `%%{init}%%` directive out of its line and reads what
    /// follows, which a preserved comment would hide from the form and move
    /// below the other statements.
    #[test]
    fn a_statement_after_a_directive_on_the_same_line_is_refused() {
        let source = "radar-beta\n  axis a, b\n%%{init: {}}%% axis c\n  curve x{1, 2, 3}";
        assert_eq!(merman_model(source).map(|model| model.axes.len()), Some(3));
        let message = format!("{}", parse_radar(source).err().expect("refused"));
        assert!(message.contains("指示子"), "{message}");

        let source = "radar-beta\n  axis a\n%%{init: {}}%%\n  curve x{1}";
        assert_eq!(parse(source).curves.len(), 1);
        assert_stable(source);
    }

    /// Mermaid draws a ring per tick, so a count typed with a few zeros too
    /// many would never finish drawing.
    #[test]
    fn too_many_ticks_are_reported() {
        let mut document = parse(TEMPLATE);
        document.options.ticks = Some("100".to_string());
        assert_eq!(document.structure_error(), None);
        document.options.ticks = Some("5000000".to_string());
        assert!(
            document
                .structure_error()
                .is_some_and(|error| error.contains("多すぎます"))
        );
    }

    /// A number without a dot is read as a 64-bit integer, and one too long
    /// for that makes `merman` refuse the chart.
    #[test]
    fn a_number_merman_cannot_hold_is_reported() {
        let mut document = parse(TEMPLATE);
        if let Some(value) = document
            .curves
            .first_mut()
            .and_then(|curve| curve.values.first_mut())
        {
            *value = "99999999999999999999".to_string();
        }
        assert!(merman_model(&emit_radar(&document)).is_none());
        assert!(document.structure_error().is_some());
        assert!(is_valid_radar_number("9223372036854775807"));
        assert!(!is_valid_radar_number("9223372036854775808"));
    }
}
