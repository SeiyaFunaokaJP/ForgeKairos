//! A round-trippable editing model for Mermaid XY charts (`xychart-beta`).
//!
//! The shape of this module mirrors [`crate::pie_model`]: [`parse_xychart`]
//! reads the body of a ```` ```mermaid ```` fence into [`XyChartDocument`],
//! and [`emit_xychart`] writes it back. See [`crate::mermaid_model`] for the
//! invariants every diagram kind keeps.
//!
//! `merman` reads an XY chart as statements separated by line breaks or `;`,
//! and this module reads it the same way, with these decisions of its own:
//!
//! - A text without quotes has its whitespace squeezed out by Mermaid
//!   (`title Sales Revenue` is drawn as `SalesRevenue`), so such a text is
//!   refused rather than opened as something the fence does not say.
//! - A text keeps the way it was written — bare, `"quoted"` or a
//!   ``"`markdown`"`` string — so that opening the editor on the insert
//!   template does not rewrite it. A bare text the user edits into something
//!   that needs quotes is quoted when it is written back.
//! - A series may hold more or fewer values than the x axis has categories.
//!   Mermaid draws the missing ones as gaps and leaves the extra ones out,
//!   which the model keeps as it is rather than refusing a chart Mermaid draws.
//! - Comment lines right before a `bar` or `line` travel with that series;
//!   the others stay before the first statement or move below the last one.
//!
//! What the parser rewrites, all of it stable from the first emit: the
//! indentation, the case of a keyword, the whitespace inside a list, the
//! spaces inside a quoted text's ends (Mermaid trims them), statements joined
//! with `;` (written one per line) and the order of the statements (title,
//! axes, then the series in their own order).

use anyhow::{Result, anyhow, bail};
use merman::diagram::RenderSemanticModel;

use crate::flowchart_model::{PreservedLine, PreservedPosition};
use crate::mermaid_model::{
    INDENT, first_statement_line, join_lines, keyword_tail, scan_preamble, statement_keyword,
};

const HEADERS: &[&str] = &["xychart-beta", "xychart"];

/// Which of the two keywords the chart was opened with. `merman` takes both
/// and draws them the same; the spelling is kept so the fence is not rewritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XyChartKeyword {
    Beta,
    Plain,
}

impl XyChartKeyword {
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Beta => "xychart-beta",
            Self::Plain => "xychart",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XyOrientation {
    Vertical,
    Horizontal,
}

impl XyOrientation {
    pub const ALL: [XyOrientation; 2] = [XyOrientation::Vertical, XyOrientation::Horizontal];

    pub fn keyword(self) -> &'static str {
        match self {
            Self::Vertical => "vertical",
            Self::Horizontal => "horizontal",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Vertical => i18n::tc!("xychart orientation", "Vertical"),
            Self::Horizontal => i18n::tc!("xychart orientation", "Horizontal"),
        }
    }
}

/// How a text is written in the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XyTextStyle {
    /// Without quotes. Only a text without whitespace or punctuation Mermaid
    /// reads as syntax can stay like this; see [`is_bare_xychart_text`].
    Bare,
    Quoted,
    /// A ``"`markdown`"`` string.
    Markdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XyText {
    pub text: String,
    pub style: XyTextStyle,
}

impl XyText {
    pub fn new(text: impl Into<String>, style: XyTextStyle) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

/// What the x axis is divided into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XyXAxisData {
    /// No data on the `x-axis` statement: Mermaid numbers the values `1..n`.
    None,
    /// `[a, b, c]`: one category per value.
    Categories(Vec<XyText>),
    /// `min --> max`, each as the source spells it.
    Range { min: String, max: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XyXAxis {
    pub title: Option<XyText>,
    pub data: XyXAxisData,
}

impl XyXAxis {
    /// Whether there is anything to write: an `x-axis` statement needs a title
    /// or data, so an axis with neither is no statement at all.
    pub fn is_written(&self) -> bool {
        self.title.is_some() || self.data != XyXAxisData::None
    }

    pub fn categories(&self) -> &[XyText] {
        match &self.data {
            XyXAxisData::Categories(categories) => categories,
            _ => &[],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XyRange {
    pub min: String,
    pub max: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XyYAxis {
    pub title: Option<XyText>,
    /// `None` lets Mermaid fit the axis to the values.
    pub range: Option<XyRange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XySeriesKind {
    Bar,
    Line,
}

impl XySeriesKind {
    pub const ALL: [XySeriesKind; 2] = [XySeriesKind::Bar, XySeriesKind::Line];

    pub fn keyword(self) -> &'static str {
        match self {
            Self::Bar => "bar",
            Self::Line => "line",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Bar => i18n::tc!("xychart series", "Bar"),
            Self::Line => i18n::tc!("xychart series", "Line"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XySeries {
    pub kind: XySeriesKind,
    /// The name written between the keyword and the values. Mermaid does not
    /// draw it, but it is what tells the series apart in the source.
    pub name: Option<XyText>,
    /// The values as the source spells them, e.g. `+1.3` or `.6`. An empty
    /// string is a gap the user left in the form, which
    /// [`XyChartDocument::structure_error`] reports.
    pub values: Vec<String>,
    /// Comment and accessibility lines written right before the series, kept
    /// verbatim so that they stay with it.
    pub leading_lines: Vec<String>,
}

/// An editable XY chart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XyChartDocument {
    pub keyword: XyChartKeyword,
    /// `None` when the header names no orientation, which leaves it to the
    /// configuration (vertical unless `xyChart.chartOrientation` says so).
    pub orientation: Option<XyOrientation>,
    pub title: Option<XyText>,
    pub x_axis: XyXAxis,
    pub y_axis: XyYAxis,
    pub series: Vec<XySeries>,
    pub preserved_lines: Vec<PreservedLine>,
}

impl XyChartDocument {
    /// A blank XY chart, used when the editor is opened on an empty fence.
    pub fn empty() -> Self {
        Self {
            keyword: XyChartKeyword::Beta,
            orientation: None,
            title: None,
            x_axis: XyXAxis {
                title: None,
                data: XyXAxisData::None,
            },
            y_axis: XyYAxis {
                title: None,
                range: None,
            },
            series: Vec::new(),
            preserved_lines: Vec::new(),
        }
    }

    /// Why the document cannot be written back as a diagram Mermaid accepts, in
    /// the display language, or `None` when it can.
    pub fn structure_error(&self) -> Option<String> {
        let text_rule = text_rule();
        if let Some(title) = &self.title
            && !is_valid_xychart_text(title)
        {
            return Some(i18n::t!(
                "The title `{}` cannot be written. {text_rule}",
                title.text
            ));
        }
        if let Some(title) = &self.x_axis.title
            && !is_valid_xychart_text(title)
        {
            return Some(i18n::t!(
                "The x-axis name `{}` cannot be written. {text_rule}",
                title.text
            ));
        }
        match &self.x_axis.data {
            XyXAxisData::None => {}
            XyXAxisData::Categories(categories) => {
                if categories.is_empty() {
                    return Some(
                        i18n::t!("The x axis has no categories. Add a category, or set the x-axis “Scale” to “None (1, 2, 3…)”.")
                            .to_string(),
                    );
                }
                for category in categories {
                    if !is_valid_xychart_text(category) {
                        return Some(i18n::t!(
                            "The x-axis category `{}` cannot be written. {text_rule}",
                            category.text
                        ));
                    }
                }
            }
            XyXAxisData::Range { min, max } => {
                if !is_valid_xychart_number(min) || !is_valid_xychart_number(max) {
                    return Some(i18n::t!(
                        "Write the x-axis range `{min}` → `{max}` as numbers."
                    ));
                }
            }
        }
        if let Some(title) = &self.y_axis.title
            && !is_valid_xychart_text(title)
        {
            return Some(i18n::t!(
                "The y-axis name `{}` cannot be written. {text_rule}",
                title.text
            ));
        }
        if let Some(range) = &self.y_axis.range
            && (!is_valid_xychart_number(&range.min) || !is_valid_xychart_number(&range.max))
        {
            return Some(i18n::t!(
                "Write the y-axis range `{}` → `{}` as numbers (leave both empty to set it automatically).",
                range.min,
                range.max
            ));
        }

        for (index, series) in self.series.iter().enumerate() {
            let number = index + 1;
            if let Some(name) = &series.name
                && !is_valid_xychart_text(name)
            {
                return Some(i18n::t!(
                    "The name `{}` of series {number} cannot be written. {text_rule}",
                    name.text
                ));
            }
            if series.values.is_empty() {
                return Some(i18n::t!(
                    "Series {number} has no values. Mermaid cannot draw a series without values."
                ));
            }
            for (position, value) in series.values.iter().enumerate() {
                if value.is_empty() {
                    return Some(i18n::t!(
                        "Series {number}: value {} is empty. Values in the middle cannot be empty (empty cells at the end count as no value).",
                        position + 1
                    ));
                }
                if !is_valid_xychart_number(value) {
                    return Some(i18n::t!(
                        "The value `{value}` of series {number} is not a number (for example 12, -3.5, .5)."
                    ));
                }
            }
        }
        self.endless_x_axis()
    }

    /// Why `merman` would never finish reading the chart, or `None`. For an x
    /// axis without categories it places a series' values by stepping from
    /// the axis' minimum to its maximum by `(max - min) / (values - 1)`, and a
    /// step that is zero, or too small to change the number it is added to,
    /// never reaches the end: parsing and drawing hang and use up the memory.
    /// Without an x-axis range the axis is `1 --> n` for the first series' `n`
    /// values, so a first series of one value does this to every longer one.
    fn endless_x_axis(&self) -> Option<String> {
        let (min, max) = match &self.x_axis.data {
            XyXAxisData::Categories(_) => return None,
            XyXAxisData::Range { min, max } => (min.parse::<f64>().ok()?, max.parse::<f64>().ok()?),
            XyXAxisData::None => (1.0, self.series.first()?.values.len() as f64),
        };
        if min > max {
            return None;
        }
        let (index, _) = self.series.iter().enumerate().find(|(_, series)| {
            let count = series.values.len();
            if count < 2 {
                return false;
            }
            let step = (max - min) / (count - 1) as f64;
            let advances = min + step > min && max + step > max;
            !advances
        })?;
        let number = index + 1;
        Some(match &self.x_axis.data {
            XyXAxisData::Range { min, max } => i18n::t!(
                "With the x-axis range `{min}` → `{max}`, Mermaid cannot place the values of series {number} and never finishes drawing. Move the ends of the range apart."
            ),
            _ => i18n::t!(
                "Without categories or a range on the x axis, Mermaid builds the x axis from the number of values in the first series. With only one value in the first series, it cannot place the values of series {number} and never finishes drawing. Add values to the first series, or give the x axis categories or a range."
            ),
        })
    }
}

fn text_rule() -> &'static str {
    i18n::t!(
        "`\"`, `%%` and line breaks are not allowed, and it cannot start or end with whitespace."
    )
}

/// Whether `source` is a XY chart: its first statement is `xychart-beta` or `xychart`.
pub fn is_xychart_source(source: &str) -> bool {
    first_statement_line(source).is_some_and(|line| {
        HEADERS
            .iter()
            .any(|header| keyword_tail(line, header).is_some())
    })
}

/// Reads `source` — the body of a ```` ```mermaid ```` fence — into an editable
/// document.
pub fn parse_xychart(source: &str) -> Result<XyChartDocument> {
    let document = scan_xychart(source)?;
    if let Some(reason) = document.endless_x_axis() {
        bail!("{reason}");
    }

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
        if !matches!(parsed.model(), RenderSemanticModel::XyChart(_)) {
            bail!(i18n::t!("This diagram is not an XY chart."));
        }
    }

    Ok(document)
}

fn scan_xychart(source: &str) -> Result<XyChartDocument> {
    let lines = source.lines().collect::<Vec<_>>();
    let preamble = scan_preamble(&lines);
    let mut document = XyChartDocument::empty();
    document.preserved_lines = preamble.preserved;

    let Some(header_index) = preamble.header else {
        return Ok(document);
    };

    let header = lines.get(header_index).copied().unwrap_or_default().trim();
    parse_header(header, &mut document)?;

    let mut seen = Seen::default();
    // Comment lines waiting for the statement after them, which decides where
    // they are written back.
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
        if is_accessibility_statement(trimmed) {
            if trimmed.contains(';') {
                bail!(i18n::t!(
                    "accTitle / accDescr on a line with `;` is not supported by the structured editor ({trimmed}). Write one per line."
                ));
            }
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
            let block = join_lines(lines.get(start..index).unwrap_or_default());
            // Mermaid's statement splitter runs an open quote or bracket on
            // into the lines below, which the form would show as statements
            // of their own.
            if is_open(&block) {
                bail!(i18n::t!(
                    "accTitle / accDescr with an unclosed `\"` or `[` is not supported by the structured editor ({trimmed}). Mermaid reads the lines after it as part of the description."
                ));
            }
            pending.push(block);
            continue;
        }

        // A list or a quoted text left open runs on to the next line, the way
        // Mermaid's statement splitter reads it.
        let mut text = line.trim_end().to_string();
        while is_open(&text) {
            let Some(next) = lines.get(index).copied() else {
                break;
            };
            if next.trim_start().starts_with("%%") {
                bail!(i18n::t!(
                    "A comment inside a statement that spans several lines is lost in the structured editor ({trimmed}). Move the comment outside the statement, or edit it as text."
                ));
            }
            text.push('\n');
            text.push_str(next.trim_end());
            index += 1;
        }
        if text.contains("%%") {
            bail!(i18n::t!(
                "A comment in the middle or at the end of a statement is lost in the structured editor ({trimmed}). Move the comment to a line of its own, or edit it as text."
            ));
        }

        for statement in split_top_level(&text, ';') {
            let statement = statement.trim();
            if statement.is_empty() {
                continue;
            }
            let is_series = parse_statement(statement, &mut document, &mut seen)?;
            if is_series {
                if let Some(series) = document.series.last_mut() {
                    series.leading_lines = std::mem::take(&mut pending);
                }
            } else {
                flush_pending(&mut document, &mut pending, seen.any);
            }
            seen.any = true;
        }
    }
    flush_pending(&mut document, &mut pending, seen.any);

    Ok(document)
}

#[derive(Default)]
struct Seen {
    any: bool,
    title: bool,
    x_axis: bool,
    y_axis: bool,
}

fn flush_pending(document: &mut XyChartDocument, pending: &mut Vec<String>, after_statement: bool) {
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

fn parse_header(header: &str, document: &mut XyChartDocument) -> Result<()> {
    let (keyword, tail) = if let Some(tail) = keyword_tail(header, "xychart-beta") {
        (XyChartKeyword::Beta, tail)
    } else if let Some(tail) = keyword_tail(header, "xychart") {
        (XyChartKeyword::Plain, tail)
    } else {
        bail!(i18n::t!(
            "This is not an XY chart (only diagrams that start with `xychart-beta` can be edited in the structured editor)."
        ));
    };
    document.keyword = keyword;
    let tail = tail.trim();
    if tail.is_empty() {
        return Ok(());
    }
    if tail.eq_ignore_ascii_case("vertical") {
        document.orientation = Some(XyOrientation::Vertical);
    } else if tail.eq_ignore_ascii_case("horizontal") {
        document.orientation = Some(XyOrientation::Horizontal);
    } else {
        bail!(i18n::t!(
            "`{tail}` on the `{}` line is not supported by the structured editor. Edit it as text.",
            keyword.keyword()
        ));
    }
    Ok(())
}

fn is_accessibility_statement(statement: &str) -> bool {
    statement_keyword(statement, "accTitle").is_some()
        || statement_keyword(statement, "accDescr").is_some()
}

/// Reads one statement into the document, and says whether it was a series.
fn parse_statement(
    statement: &str,
    document: &mut XyChartDocument,
    seen: &mut Seen,
) -> Result<bool> {
    if let Some(rest) = keyword_rest(statement, "title") {
        if seen.title {
            bail!(i18n::t!(
                "`title` is written twice. Merge them into one, then open it again."
            ));
        }
        seen.title = true;
        document.title = Some(read_whole_text(rest, statement)?);
        return Ok(false);
    }
    if let Some(rest) = keyword_rest(statement, "x-axis") {
        if seen.x_axis {
            bail!(i18n::t!(
                "`x-axis` is written twice. Merge them into one, then open it again."
            ));
        }
        seen.x_axis = true;
        document.x_axis = parse_x_axis(rest, statement)?;
        // Mermaid pairs a series' values with the x axis as it stands when the
        // series is read, and the form writes the axes before the series.
        if !document.series.is_empty() && document.x_axis.data != XyXAxisData::None {
            bail!(i18n::t!(
                "The `x-axis` ticks are written after a series (bar / line). Mermaid places the values of the series written before them at 1, 2, 3…, so reordering them in the structured editor would change the chart. Move `x-axis` before the series, then open it again."
            ));
        }
        return Ok(false);
    }
    if let Some(rest) = keyword_rest(statement, "y-axis") {
        if seen.y_axis {
            bail!(i18n::t!(
                "`y-axis` is written twice. Merge them into one, then open it again."
            ));
        }
        seen.y_axis = true;
        document.y_axis = parse_y_axis(rest, statement)?;
        return Ok(false);
    }
    for kind in XySeriesKind::ALL {
        if let Some(rest) = keyword_rest(statement, kind.keyword()) {
            let (name, values) = parse_series(rest, statement)?;
            document.series.push(XySeries {
                kind,
                name,
                values,
                leading_lines: Vec::new(),
            });
            return Ok(true);
        }
    }
    bail!(i18n::t!(
        "Could not read this as an XY chart statement ({statement}). Edit it as text."
    ))
}

/// What follows `keyword` at the start of a statement. Mermaid matches the
/// word without regard to case and without asking where it ends, so
/// `barTitle [1]` is a bar named `Title`; this module only takes the word when
/// something that cannot be part of a word follows it, and refuses the rest.
fn keyword_rest<'a>(statement: &'a str, keyword: &str) -> Option<&'a str> {
    let head = statement.get(..keyword.len())?;
    if !head.eq_ignore_ascii_case(keyword) {
        return None;
    }
    let rest = statement.get(keyword.len()..)?;
    if rest.is_empty()
        || rest.starts_with(|character: char| character.is_whitespace())
        || rest.starts_with(['[', '"'])
    {
        return Some(rest);
    }
    None
}

fn parse_x_axis(rest: &str, statement: &str) -> Result<XyXAxis> {
    let text = rest.trim();
    if text.is_empty() {
        bail!(i18n::t!(
            "`x-axis` has neither a name nor ticks ({statement})."
        ));
    }
    if text.starts_with('[') {
        return Ok(XyXAxis {
            title: None,
            data: XyXAxisData::Categories(read_categories(text, statement)?),
        });
    }
    if let Some((min, max)) = read_range(text, statement)? {
        return Ok(XyXAxis {
            title: None,
            data: XyXAxisData::Range { min, max },
        });
    }
    let (title, tail) = read_text_prefix(text, statement)?;
    let tail = tail.trim();
    let data = if tail.is_empty() {
        XyXAxisData::None
    } else if tail.starts_with('[') {
        XyXAxisData::Categories(read_categories(tail, statement)?)
    } else if let Some((min, max)) = read_range(tail, statement)? {
        XyXAxisData::Range { min, max }
    } else {
        bail!(i18n::t!("Could not read the x-axis ticks ({statement})."));
    };
    Ok(XyXAxis {
        title: Some(title),
        data,
    })
}

fn parse_y_axis(rest: &str, statement: &str) -> Result<XyYAxis> {
    let text = rest.trim();
    if text.is_empty() {
        bail!(i18n::t!(
            "`y-axis` has neither a name nor a range ({statement})."
        ));
    }
    if let Some((min, max)) = read_range(text, statement)? {
        return Ok(XyYAxis {
            title: None,
            range: Some(XyRange { min, max }),
        });
    }
    if text.starts_with('[') {
        bail!(i18n::t!("The y axis cannot have categories ({statement})."));
    }
    let (title, tail) = read_text_prefix(text, statement)?;
    let tail = tail.trim();
    let range = if tail.is_empty() {
        None
    } else if let Some((min, max)) = read_range(tail, statement)? {
        Some(XyRange { min, max })
    } else {
        bail!(i18n::t!(
            "Could not read the y-axis range ({statement}). Write it as `0 --> 100`."
        ));
    };
    Ok(XyYAxis {
        title: Some(title),
        range,
    })
}

fn parse_series(rest: &str, statement: &str) -> Result<(Option<XyText>, Vec<String>)> {
    let text = rest.trim();
    let (name, list) = if text.starts_with('[') {
        (None, text)
    } else {
        if text.is_empty() {
            bail!(i18n::t!("The series has no values ({statement})."));
        }
        let (name, tail) = read_text_prefix(text, statement)?;
        (Some(name), tail.trim())
    };
    if !list.starts_with('[') {
        bail!(i18n::t!(
            "The series values are not written as `[1, 2, 3]` ({statement})."
        ));
    }
    let inner = bracket_inner(list, statement)?;
    let mut values = Vec::new();
    for part in split_top_level(inner, ',') {
        let value = part.trim();
        if !is_valid_xychart_number(value) {
            bail!(i18n::t!(
                "The series value `{value}` cannot be read as a number ({statement})."
            ));
        }
        values.push(value.to_string());
    }
    Ok((name, values))
}

fn read_categories(text: &str, statement: &str) -> Result<Vec<XyText>> {
    let inner = bracket_inner(text, statement)?;
    let mut categories = Vec::new();
    for part in split_top_level(inner, ',') {
        let part = part.trim();
        if part.is_empty() {
            bail!(i18n::t!("The x axis has an empty category ({statement})."));
        }
        let category = read_whole_text(part, statement)?;
        if category.text.is_empty() {
            bail!(i18n::t!("The x axis has an empty category ({statement})."));
        }
        categories.push(category);
    }
    Ok(categories)
}

/// Reads a text that has to fill what it is given — a title or a category —
/// the way `merman`'s `parse_text` does.
fn read_whole_text(input: &str, statement: &str) -> Result<XyText> {
    let text = input.trim();
    if text.starts_with('"') {
        let (value, rest) = read_text_prefix(text, statement)?;
        if !rest.trim().is_empty() {
            bail!(i18n::t!(
                "Could not read `{}` after the quotes ({statement}).",
                rest.trim()
            ));
        }
        return Ok(value);
    }
    if text.is_empty() {
        bail!(i18n::t!("The text is empty ({statement})."));
    }
    if text.contains(char::is_whitespace) {
        bail!(i18n::t!(
            "Mermaid collapses the spaces in the unquoted `{text}` when drawing it ({statement}). Enclose it in `\"`, like `\"{text}\"`, then open it again."
        ));
    }
    Ok(unquoted_text(text))
}

/// A text read without quotes, held as quoted when Mermaid would not read it
/// bare (see [`is_bare_xychart_text`]), so that the first write already
/// quotes it and the round trip stays stable.
fn unquoted_text(text: &str) -> XyText {
    let style = if is_bare_xychart_text(text) {
        XyTextStyle::Bare
    } else {
        XyTextStyle::Quoted
    };
    XyText::new(text, style)
}

/// Reads a text at the start of `input` and returns it with what follows, the
/// way `merman`'s `parse_text_prefix` does: a quoted or markdown string, or a
/// bare word that runs to whitespace or `[`.
fn read_text_prefix<'a>(input: &'a str, statement: &str) -> Result<(XyText, &'a str)> {
    let text = input.trim_start();
    if let Some(body) = text.strip_prefix("\"`") {
        let Some(end) = body.find("`\"") else {
            bail!(i18n::t!("There is an unclosed \"` ({statement})."));
        };
        let value = body.get(..end).unwrap_or_default();
        let rest = body.get(end + 2..).unwrap_or_default();
        return Ok((XyText::new(value.trim(), XyTextStyle::Markdown), rest));
    }
    if let Some(body) = text.strip_prefix('"') {
        let Some(end) = body.find('"') else {
            bail!(i18n::t!("There is an unclosed `\"` ({statement})."));
        };
        let value = body.get(..end).unwrap_or_default();
        let rest = body.get(end + 1..).unwrap_or_default();
        return Ok((XyText::new(value.trim(), XyTextStyle::Quoted), rest));
    }
    let end = text
        .find(|character: char| character.is_whitespace() || character == '[')
        .unwrap_or(text.len());
    let value = text.get(..end).unwrap_or_default();
    if value.is_empty() {
        bail!(i18n::t!("The name is empty ({statement})."));
    }
    Ok((unquoted_text(value), text.get(end..).unwrap_or_default()))
}

/// Reads `min --> max`, or `None` when `input` does not start with a number
/// followed by `-->`.
fn read_range(input: &str, statement: &str) -> Result<Option<(String, String)>> {
    let text = input.trim_start();
    let Some((min, tail)) = number_token(text) else {
        return Ok(None);
    };
    let Some(after_arrow) = tail.trim_start().strip_prefix("-->") else {
        return Ok(None);
    };
    let Some((max, tail)) = number_token(after_arrow.trim_start()) else {
        bail!(i18n::t!("There is no number after `-->` ({statement})."));
    };
    if !tail.trim().is_empty() {
        bail!(i18n::t!(
            "Could not read `{}` after the range ({statement}).",
            tail.trim()
        ));
    }
    if !is_valid_xychart_number(min) || !is_valid_xychart_number(max) {
        bail!(i18n::t!(
            "Could not read the range `{min} --> {max}` as numbers ({statement})."
        ));
    }
    Ok(Some((min.to_string(), max.to_string())))
}

/// A sign followed by digits and dots, the characters `merman` takes into a
/// number token.
fn number_token(text: &str) -> Option<(&str, &str)> {
    let mut end = 0;
    for (offset, character) in text.char_indices() {
        let is_sign = offset == 0 && matches!(character, '+' | '-');
        if is_sign || character.is_ascii_digit() || character == '.' {
            end = offset + character.len_utf8();
            continue;
        }
        break;
    }
    if end == 0 {
        return None;
    }
    Some((text.get(..end)?, text.get(end..)?))
}

/// Tracks whether a scan is inside a quoted or a markdown string, which hide
/// the brackets and separators in them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Quote {
    None,
    Plain,
    Markdown,
    /// The backtick of the closing `` `" `` has been seen; its quote follows.
    MarkdownClosing,
}

impl Quote {
    /// Steps over the character at `rest`'s start, and says whether it was
    /// inside a string (and so is not syntax).
    fn step(&mut self, rest: &str) -> bool {
        match *self {
            Quote::Markdown => {
                if rest.starts_with("`\"") {
                    *self = Quote::MarkdownClosing;
                }
                true
            }
            Quote::MarkdownClosing => {
                *self = Quote::None;
                true
            }
            Quote::Plain => {
                if rest.starts_with('"') {
                    *self = Quote::None;
                }
                true
            }
            Quote::None => {
                if rest.starts_with("\"`") {
                    *self = Quote::Markdown;
                    true
                } else if rest.starts_with('"') {
                    *self = Quote::Plain;
                    true
                } else {
                    false
                }
            }
        }
    }
}

/// What is between `[` and its `]`, which has to end the statement.
fn bracket_inner<'a>(text: &'a str, statement: &str) -> Result<&'a str> {
    let Some(body) = text.strip_prefix('[') else {
        bail!(i18n::t!("`[` is missing ({statement})."));
    };
    let mut quote = Quote::None;
    for (offset, character) in body.char_indices() {
        if quote.step(body.get(offset..).unwrap_or_default()) {
            continue;
        }
        match character {
            '[' => bail!(i18n::t!("There is a `[` inside `[` ({statement}).")),
            ']' => {
                let after = body.get(offset + 1..).unwrap_or_default();
                if !after.trim().is_empty() {
                    bail!(i18n::t!(
                        "Could not read `{}` after `]` ({statement}).",
                        after.trim()
                    ));
                }
                return Ok(body.get(..offset).unwrap_or_default());
            }
            _ => {}
        }
    }
    bail!(i18n::t!("It is not closed with `]` ({statement})."))
}

/// Splits `text` at `delimiter` where it is outside strings and brackets.
fn split_top_level(text: &str, delimiter: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut quote = Quote::None;
    let mut depth = 0i32;
    let mut start = 0;
    for (offset, character) in text.char_indices() {
        if quote.step(text.get(offset..).unwrap_or_default()) {
            continue;
        }
        match character {
            '[' | '{' => depth += 1,
            ']' | '}' => depth -= 1,
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

/// What follows the `%%{…}%%` directives a comment block starts with, when it
/// is a statement. Mermaid cuts the directives out of the line and reads the
/// rest, which a preserved comment would hide from the form and move.
pub(crate) fn statement_after_directives(block: &str) -> Option<&str> {
    let mut rest = block.trim_start();
    while let Some(after) = rest.strip_prefix("%%{") {
        let end = after.find("}%%")?;
        rest = after.get(end + 3..)?.trim_start();
    }
    let rest = rest.trim_end();
    (!rest.is_empty() && !rest.starts_with("%%")).then_some(rest)
}

pub(crate) fn directive_statement_error(statement: &str) -> String {
    i18n::t!(
        "`{statement}`, written after the directive `%%{{…}}%%` on the same line, is not supported by the structured editor. Mermaid removes the directive and reads this statement. Break the line after the directive, then open it again."
    )
}

/// Whether a string or a bracket is still open at the end of `text`, which
/// makes Mermaid read the next line as part of the same statement.
fn is_open(text: &str) -> bool {
    let mut quote = Quote::None;
    let mut depth = 0i32;
    for (offset, character) in text.char_indices() {
        if quote.step(text.get(offset..).unwrap_or_default()) {
            continue;
        }
        match character {
            '[' | '{' => depth += 1,
            ']' | '}' => depth -= 1,
            _ => {}
        }
    }
    quote != Quote::None || depth > 0
}

/// Writes the document back as Mermaid source, without a trailing newline.
pub fn emit_xychart(document: &XyChartDocument) -> String {
    let mut out = String::new();

    for line in preserved_at(document, PreservedPosition::Preamble) {
        out.push_str(line);
        out.push('\n');
    }

    out.push_str(document.keyword.keyword());
    if let Some(orientation) = document.orientation {
        out.push(' ');
        out.push_str(orientation.keyword());
    }
    out.push('\n');

    for line in preserved_at(document, PreservedPosition::Top) {
        out.push_str(line);
        out.push('\n');
    }

    if let Some(title) = &document.title {
        out.push_str(&format!("{INDENT}title {}\n", emit_text(title)));
    }

    if document.x_axis.is_written() {
        let mut parts = Vec::new();
        if let Some(title) = &document.x_axis.title {
            parts.push(emit_text(title));
        }
        match &document.x_axis.data {
            XyXAxisData::None => {}
            XyXAxisData::Categories(categories) => parts.push(format!(
                "[{}]",
                categories
                    .iter()
                    .map(emit_text)
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            XyXAxisData::Range { min, max } => parts.push(format!("{min} --> {max}")),
        }
        out.push_str(&format!("{INDENT}x-axis {}\n", parts.join(" ")));
    }

    if document.y_axis.title.is_some() || document.y_axis.range.is_some() {
        let mut parts = Vec::new();
        if let Some(title) = &document.y_axis.title {
            parts.push(emit_text(title));
        }
        if let Some(range) = &document.y_axis.range {
            parts.push(format!("{} --> {}", range.min, range.max));
        }
        out.push_str(&format!("{INDENT}y-axis {}\n", parts.join(" ")));
    }

    for series in &document.series {
        for line in &series.leading_lines {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str(INDENT);
        out.push_str(series.kind.keyword());
        if let Some(name) = &series.name {
            out.push(' ');
            out.push_str(&emit_text(name));
        }
        out.push_str(&format!(" [{}]\n", series.values.join(", ")));
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
    document: &XyChartDocument,
    position: PreservedPosition,
) -> impl Iterator<Item = &str> {
    document
        .preserved_lines
        .iter()
        .filter(move |line| line.position == position)
        .map(|line| line.text.as_str())
}

/// A text in the spelling it was opened with, quoted when a bare text has
/// been edited into something Mermaid would read differently without quotes.
pub fn emit_text(text: &XyText) -> String {
    match text.style {
        XyTextStyle::Bare if is_bare_xychart_text(&text.text) => text.text.clone(),
        XyTextStyle::Bare | XyTextStyle::Quoted => format!("\"{}\"", text.text),
        XyTextStyle::Markdown => format!("\"`{}`\"", text.text),
    }
}

/// Whether `text` reads back as itself when it is written without quotes, in
/// Mermaid as well as in `merman`: ASCII letters and `_` only. `merman` reads
/// far more bare (Japanese included), but Mermaid's lexer refuses anything
/// outside ASCII, splits a digit off an axis title (`y-axis Rev1 0 --> 10`)
/// and stops at most punctuation, so everything else is written quoted.
pub fn is_bare_xychart_text(text: &str) -> bool {
    text.starts_with(|character: char| character.is_ascii_alphabetic())
        && text
            .chars()
            .all(|character| character.is_ascii_alphabetic() || character == '_')
}

/// Whether `text` can be written as a title, an axis name, a category or a
/// series name: Mermaid's quotes have no escape, and it trims what is inside
/// them, so the quote itself, a comment marker, a line break and whitespace at
/// either end cannot be written.
pub fn is_valid_xychart_text(text: &XyText) -> bool {
    let value = text.text.as_str();
    !value.is_empty()
        && value.trim() == value
        && !value.contains("%%")
        && !value.contains(['\n', '\r'])
        && match text.style {
            XyTextStyle::Markdown => !value.contains('`') && !value.contains('"'),
            XyTextStyle::Bare | XyTextStyle::Quoted => !value.contains('"'),
        }
}

/// Whether `text` is a number the way `merman` reads one in a list or a range:
/// a sign, digits and a dot, e.g. `12`, `-3.5`, `+.5`.
pub fn is_valid_xychart_number(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|character| character.is_ascii_digit() || matches!(character, '+' | '-' | '.'))
        && text.parse::<f64>().is_ok_and(f64::is_finite)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MermaidTheme, render_to_svg};
    use merman::diagrams::xychart::{XyChartAxisRenderModel, XyChartPlotType};
    use merman::{Engine, ParseOptions};

    /// The template `markdown::InsertMermaidDiagram` writes, which has to come
    /// back byte for byte. The editor's tests read it from the action itself.
    const TEMPLATE: &str = "xychart-beta\n    %% bar は棒、line は折れ線。値の数は x-axis の項目の数と揃える\n    title \"月別の売上\"\n    x-axis [\"1月\", \"2月\", \"3月\", \"4月\"]\n    y-axis \"売上 (万円)\" 0 --> 100\n    bar [30, 45, 60, 80]\n    line [30, 45, 60, 80]";

    fn parse(source: &str) -> XyChartDocument {
        parse_xychart(source).unwrap_or_else(|error| panic!("{source} should parse: {error}"))
    }

    /// The emitted text must read back as the same document and emit the same
    /// text, otherwise confirming the editor twice would keep rewriting the
    /// fence.
    fn assert_stable(source: &str) -> String {
        let first = parse(source);
        let first_text = emit_xychart(&first);
        let second = parse_xychart(&first_text)
            .unwrap_or_else(|error| panic!("emitted source should parse: {error}\n{first_text}"));
        assert_eq!(first, second, "the document changed through:\n{first_text}");
        assert_eq!(emit_xychart(&second), first_text);
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

    fn merman_model(source: &str) -> Option<merman::diagrams::xychart::XyChartDiagramRenderModel> {
        let parsed = Engine::new()
            .parse_diagram_for_render_model_sync(source, ParseOptions::strict())
            .ok()??;
        match parsed.into_parts().1 {
            RenderSemanticModel::XyChart(model) => Some(model),
            _ => None,
        }
    }

    /// Where Mermaid puts each value: the category or x position it pairs the
    /// value with, which depends on the order of the statements as well.
    fn plot_data(source: &str) -> Vec<Vec<(String, Option<f64>)>> {
        merman_model(source)
            .unwrap_or_else(|| panic!("merman should read {source}"))
            .plots
            .into_iter()
            .map(|plot| plot.data)
            .collect()
    }

    fn number(text: &str) -> f64 {
        text.parse::<f64>()
            .unwrap_or_else(|error| panic!("`{text}` should be a number: {error}"))
    }

    /// What Mermaid draws from `source` is what the document says: the same
    /// title, axes and series, value for value.
    fn assert_matches_merman(source: &str, document: &XyChartDocument) {
        let model = merman_model(source).unwrap_or_else(|| panic!("merman should read {source}"));
        assert_eq!(
            model.title.as_deref(),
            document.title.as_ref().map(|title| title.text.as_str()),
            "title of {source}"
        );
        if let Some(orientation) = document.orientation {
            assert_eq!(model.orientation, orientation.keyword(), "{source}");
        }
        let x_title = document
            .x_axis
            .title
            .as_ref()
            .map(|title| title.text.clone())
            .unwrap_or_default();
        match (&model.x_axis, &document.x_axis.data) {
            (XyChartAxisRenderModel::Band { title, categories }, XyXAxisData::Categories(ours)) => {
                assert_eq!(title, &x_title, "{source}");
                assert_eq!(
                    categories,
                    &ours
                        .iter()
                        .map(|text| text.text.clone())
                        .collect::<Vec<_>>(),
                    "{source}"
                );
            }
            (
                XyChartAxisRenderModel::Linear { title, min, max },
                XyXAxisData::Range {
                    min: our_min,
                    max: our_max,
                },
            ) => {
                assert_eq!(title, &x_title, "{source}");
                assert_eq!(*min, Some(number(our_min)), "{source}");
                assert_eq!(*max, Some(number(our_max)), "{source}");
            }
            (axis, XyXAxisData::None) => {
                let title = match axis {
                    XyChartAxisRenderModel::Band { title, categories } => {
                        assert!(categories.is_empty(), "{source}");
                        title
                    }
                    XyChartAxisRenderModel::Linear { title, .. } => title,
                };
                assert_eq!(title, &x_title, "{source}");
            }
            (axis, data) => panic!("x axis of {source}: merman {axis:?}, ours {data:?}"),
        }
        let XyChartAxisRenderModel::Linear { title, min, max } = &model.y_axis else {
            panic!("merman's y axis is always linear: {source}");
        };
        assert_eq!(
            title,
            &document
                .y_axis
                .title
                .as_ref()
                .map(|title| title.text.clone())
                .unwrap_or_default(),
            "{source}"
        );
        if let Some(range) = &document.y_axis.range {
            assert_eq!(*min, Some(number(&range.min)), "{source}");
            assert_eq!(*max, Some(number(&range.max)), "{source}");
        }
        assert_eq!(model.plots.len(), document.series.len(), "{source}");
        for (plot, series) in model.plots.iter().zip(&document.series) {
            let kind = match plot.plot_type {
                XyChartPlotType::Bar => XySeriesKind::Bar,
                XyChartPlotType::Line => XySeriesKind::Line,
            };
            assert_eq!(kind, series.kind, "{source}");
            assert_eq!(
                plot.values,
                series
                    .values
                    .iter()
                    .map(|value| number(value))
                    .collect::<Vec<_>>(),
                "{source}"
            );
        }
    }

    /// Every XY chart `merman` ships: each one the editor opens reads as what
    /// Mermaid draws, is written back stably as the same chart, and still
    /// draws. The ones it refuses are listed with their reasons.
    #[test]
    fn merman_examples_round_trip() {
        let fixtures = crate::merman_fixtures("xychart");
        if fixtures.is_empty() {
            return;
        }
        assert!(
            fixtures.len() > 50,
            "found only {} examples",
            fixtures.len()
        );

        let mut opened = 0;
        let mut merman_refused = 0;
        let mut refused = Vec::new();
        for (path, source) in &fixtures {
            if merman_model(source).is_none() {
                merman_refused += 1;
                assert!(parse_xychart(source).is_err(), "{path:?}");
                continue;
            }
            let document = match parse_xychart(source) {
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
            assert_eq!(plot_data(&emitted), plot_data(source), "{path:?}");
            assert_renders(&emitted);
        }
        eprintln!(
            "xychart examples: {opened} opened, {merman_refused} refused by merman, {} refused by the editor:\n{}",
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
        assert_eq!(emit_xychart(&document), TEMPLATE);
        assert_eq!(
            document.title,
            Some(XyText::new("月別の売上", XyTextStyle::Quoted))
        );
        assert_eq!(
            document
                .x_axis
                .categories()
                .iter()
                .map(|category| category.text.as_str())
                .collect::<Vec<_>>(),
            vec!["1月", "2月", "3月", "4月"]
        );
        assert_eq!(
            document.y_axis,
            XyYAxis {
                title: Some(XyText::new("売上 (万円)", XyTextStyle::Quoted)),
                range: Some(XyRange {
                    min: "0".to_string(),
                    max: "100".to_string()
                }),
            }
        );
        assert_eq!(document.series.len(), 2);
        assert_matches_merman(TEMPLATE, &document);
        assert_renders(TEMPLATE);
    }

    #[test]
    fn examples_round_trip_and_render() {
        for source in [
            "xychart-beta horizontal\n    title \"横向き\"\n    x-axis 月 [a, b]\n    bar 売上 [1, 2]",
            "xychart\n    x-axis 0 --> 2\n    line [0, 1, 0, 1]",
            "xychart\n    x-axis 軸 1 --> 5\n    y-axis 値\n    line [1, 2]",
            "xychart-beta\n    y-axis -2.4 --> 3.5\n    line [+1.3, .6, 2.4, -.34]",
            "xychart-beta\n    title \"`**太字**`\"\n    bar [1]",
            "---\ntitle: 前置き\n---\n%%{init: {\"theme\": \"dark\"}}%%\nxychart-beta\n    accTitle: 概要\n    bar [1, 2]",
            "xychart-beta\n    x-axis [\"日本語 の 項目\", \"セミコロン; 入り\", \"かっこ [ ] 入り\"]\n    bar [1, 2, 3]",
        ] {
            let emitted = assert_stable(source);
            assert_matches_merman(&emitted, &parse(&emitted));
            assert_eq!(plot_data(&emitted), plot_data(source), "{source}");
            assert_renders(&emitted);
        }
    }

    #[test]
    fn preserved_lines_keep_their_place() {
        let source = "%%{init: {\"theme\": \"dark\"}}%%\nxychart-beta\n  %% 先頭のメモ\n  title 図\n  %% 途中のメモ\n  x-axis [a, b]\n  %% 棒のメモ\n  bar [1, 2]\n  accDescr {\n    説明の 1 行目\n    2 行目\n  }\n  line [2, 1]\n  %% 末尾のメモ";
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
                    "%%{init: {\"theme\": \"dark\"}}%%".to_string()
                ),
                (PreservedPosition::Top, "%% 先頭のメモ".to_string()),
                (PreservedPosition::Bottom, "%% 途中のメモ".to_string()),
                (PreservedPosition::Bottom, "%% 末尾のメモ".to_string()),
            ]
        );
        assert_eq!(
            document
                .series
                .iter()
                .map(|series| series.leading_lines.clone())
                .collect::<Vec<_>>(),
            vec![
                vec!["  %% 棒のメモ".to_string()],
                vec!["  accDescr {\n    説明の 1 行目\n    2 行目\n  }".to_string()],
            ]
        );
        assert_eq!(
            assert_stable(source),
            "%%{init: {\"theme\": \"dark\"}}%%\nxychart-beta\n  %% 先頭のメモ\n    title \"図\"\n    x-axis [a, b]\n  %% 棒のメモ\n    bar [1, 2]\n  accDescr {\n    説明の 1 行目\n    2 行目\n  }\n    line [2, 1]\n  %% 途中のメモ\n  %% 末尾のメモ"
        );
    }

    /// Moving a series carries the comment written for it, which is what the
    /// official example of per-series colors relies on.
    #[test]
    fn a_series_comment_moves_with_its_series() {
        let mut document = parse("xychart\n%% 黒\nline [1, 2]\n%% 青\nbar [2, 3]");
        document.series.swap(0, 1);
        assert_eq!(
            emit_xychart(&document),
            "xychart\n%% 青\n    bar [2, 3]\n%% 黒\n    line [1, 2]"
        );
    }

    /// Mermaid squeezes the whitespace out of a text without quotes, so the
    /// form would show a text the fence does not hold; such a text is refused
    /// with the way to write it, and every other refusal names its reason.
    #[test]
    fn constructs_that_would_change_are_refused() {
        for (source, reason) in [
            (
                "xychart\n  title Sales Revenue\n  bar [1]",
                "collapses the spaces",
            ),
            (
                "xychart\n  x-axis [cat2 asdf]\n  bar [1]",
                "collapses the spaces",
            ),
            ("xychart\n  title a\n  title b", "written twice"),
            ("xychart\n  x-axis [a]\n  x-axis [b]", "written twice"),
            ("xychart\n  bar [1, 2] %% メモ", "comment"),
            ("xychart\n  bar [1,\n  %% メモ\n  2]", "comment"),
            ("xychart\n  barTitle [1]", "Could not read"),
            (
                "xychart abc\n  bar [1]",
                "not supported by the structured editor",
            ),
            ("xychart\n  x-axis [\"\"]\n  bar [1]", "empty category"),
            ("xychart\n  accTitle: a; bar [1]", "`;`"),
        ] {
            let error = parse_xychart(source)
                .err()
                .unwrap_or_else(|| panic!("`{source}` should be refused"));
            let message = format!("{error}");
            assert!(
                message.contains(reason),
                "the reason for `{source}` should say `{reason}`: {message}"
            );
        }
        // What merman itself cannot read is refused too.
        for source in [
            "xychart\n  bar [1, a]",
            "xychart\n  y-axis [1, 2]",
            "xychart\n  line \"t\"",
        ] {
            assert!(parse_xychart(source).is_err(), "{source}");
        }
    }

    /// `;` separates statements the way a line break does, and is written
    /// back as one statement per line. `merman` 0.7 also let a list run over
    /// lines; 0.8 refuses that, as Mermaid does.
    #[test]
    fn separators_are_normalized_and_continued_lists_refused() {
        let continued = "xychart\n  bar [1,\n    2]";
        assert!(render_to_svg(continued, &MermaidTheme::default()).is_err());
        assert!(parse_xychart(continued).is_err());

        let source = "xychart\n  title \"a; b\"; x-axis [p, q]\n  bar [1, 2]";
        let document = parse(source);
        assert_eq!(
            document.title,
            Some(XyText::new("a; b", XyTextStyle::Quoted))
        );
        assert_eq!(
            document.series.first().map(|series| series.values.len()),
            Some(2)
        );
        assert_eq!(
            assert_stable(source),
            "xychart\n    title \"a; b\"\n    x-axis [p, q]\n    bar [1, 2]"
        );
    }

    /// Mermaid draws a series with fewer values than categories with gaps and
    /// leaves extra values out; neither is a reason to refuse the chart.
    #[test]
    fn a_value_count_that_differs_from_the_categories_is_kept() {
        let source = "xychart\n  x-axis [cat1, cat2, cat3]\n  bar [10, 20]\n  line [1, 2, 3, 4]";
        let document = parse(source);
        assert_eq!(document.structure_error(), None);
        assert_eq!(
            assert_stable(source),
            "xychart\n    x-axis [\"cat1\", \"cat2\", \"cat3\"]\n    bar [10, 20]\n    line [1, 2, 3, 4]"
        );
    }

    #[test]
    fn a_bare_text_that_needs_quotes_is_quoted() {
        let mut document = parse(TEMPLATE);
        document.title = Some(XyText::new("月別 の 売上", XyTextStyle::Bare));
        if let XyXAxisData::Categories(categories) = &mut document.x_axis.data
            && let Some(first) = categories.first_mut()
        {
            *first = XyText::new("10", XyTextStyle::Bare);
        }
        let emitted = emit_xychart(&document);
        assert!(emitted.contains("title \"月別 の 売上\""), "{emitted}");
        assert!(emitted.contains("[\"10\", "), "{emitted}");
        let reparsed = parse(&emitted);
        assert_eq!(
            reparsed.title.as_ref().map(|title| title.text.as_str()),
            Some("月別 の 売上")
        );
        assert_renders(&emitted);
    }

    #[test]
    fn structure_errors_are_reported() {
        let base = parse(TEMPLATE);
        let cases: Vec<(Box<dyn Fn(&mut XyChartDocument)>, &str)> = vec![
            (
                Box::new(|document| {
                    document.title = Some(XyText::new("引用 \" あり", XyTextStyle::Quoted))
                }),
                "The title",
            ),
            (
                Box::new(|document| {
                    document.x_axis.data = XyXAxisData::Categories(Vec::new());
                }),
                "has no categories",
            ),
            (
                Box::new(|document| {
                    document.x_axis.data = XyXAxisData::Range {
                        min: "a".to_string(),
                        max: "2".to_string(),
                    };
                }),
                "x-axis range",
            ),
            (
                Box::new(|document| {
                    document.y_axis.range = Some(XyRange {
                        min: String::new(),
                        max: "2".to_string(),
                    });
                }),
                "y-axis range",
            ),
            (
                Box::new(|document| {
                    if let Some(series) = document.series.first_mut() {
                        series.values = vec!["1".to_string(), String::new(), "3".to_string()];
                    }
                }),
                "value 2 is empty",
            ),
            (
                Box::new(|document| {
                    if let Some(series) = document.series.first_mut() {
                        series.values = vec!["1e3".to_string()];
                    }
                }),
                "is not a number",
            ),
            (
                Box::new(|document| {
                    if let Some(series) = document.series.first_mut() {
                        series.values.clear();
                    }
                }),
                "has no values",
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
    fn values_and_texts_are_validated() {
        for value in ["1", "-3.5", "+.5", ".6", "012", "5."] {
            assert!(is_valid_xychart_number(value), "{value}");
        }
        for value in ["", "1e3", "a", "1.2.3", "--1", "１", "inf", "."] {
            assert!(!is_valid_xychart_number(value), "{value}");
        }
        assert!(is_valid_xychart_text(&XyText::new(
            "日本語",
            XyTextStyle::Bare
        )));
        assert!(is_valid_xychart_text(&XyText::new(
            "a; b [c]",
            XyTextStyle::Quoted
        )));
        assert!(!is_valid_xychart_text(&XyText::new(
            " a",
            XyTextStyle::Quoted
        )));
        assert!(!is_valid_xychart_text(&XyText::new(
            "a\"b",
            XyTextStyle::Quoted
        )));
        assert!(!is_valid_xychart_text(&XyText::new(
            "50%% 達成",
            XyTextStyle::Quoted
        )));
        assert!(!is_valid_xychart_text(&XyText::new(
            "",
            XyTextStyle::Quoted
        )));
        assert!(is_bare_xychart_text("jan"));
        assert!(!is_bare_xychart_text("1月"));
        assert!(!is_bare_xychart_text("a b"));
        assert!(!is_bare_xychart_text("a,b"));
    }

    #[test]
    fn an_empty_fence_opens_as_a_blank_chart() {
        let document = parse("");
        assert_eq!(document, XyChartDocument::empty());
        assert_eq!(emit_xychart(&document), "xychart-beta");
        assert_renders("xychart-beta");
    }

    #[test]
    fn crlf_and_keyword_case_are_tolerated() {
        let source = "xychart HORIZONTAL\r\n  TITLE \"図\"  \r\n  Bar [1, 2]\r\n";
        assert_eq!(
            assert_stable(source),
            "xychart horizontal\n    title \"図\"\n    bar [1, 2]"
        );
    }

    #[test]
    fn the_keyword_is_recognized_the_way_merman_recognizes_it() {
        for source in [
            "xychart-beta\n    bar [1, 2]",
            "xychart\n    bar [1, 2]",
            "xychart-beta horizontal\n    bar [1]",
        ] {
            assert!(is_xychart_source(source), "{source} should be a XY chart");
        }
        for source in [
            "xychart-betas\n  bar [1]",
            "xy\n  bar [1]",
            "XYCHART-beta",
            "",
            "%% xychart",
            "flowchart TD\n    A --> B",
        ] {
            assert!(
                !is_xychart_source(source),
                "{source} should not be a XY chart"
            );
        }
    }

    /// Mermaid pairs a series' values with the x axis as it is when the series
    /// is read, so a series written before the x axis's categories or range is
    /// drawn at 1, 2, 3… and the form, which writes the axes first, would move
    /// it. A title-only `x-axis` changes nothing and still opens.
    #[test]
    fn an_x_axis_after_a_series_is_refused() {
        for (source, reordered) in [
            (
                "xychart\n  bar [10, 20]\n  x-axis [a, b]",
                "xychart\n  x-axis [a, b]\n  bar [10, 20]",
            ),
            (
                "xychart\n  bar [10, 20]\n  x-axis 0 --> 10",
                "xychart\n  x-axis 0 --> 10\n  bar [10, 20]",
            ),
        ] {
            assert_ne!(plot_data(source), plot_data(reordered), "{source}");
            let message = format!(
                "{}",
                parse_xychart(source)
                    .err()
                    .unwrap_or_else(|| panic!("`{source}` should be refused"))
            );
            assert!(message.contains("after a series (bar / line)"), "{message}");
        }

        let source = "xychart\n  bar [10, 20]\n  x-axis \"月\"";
        let emitted = assert_stable(source);
        assert_eq!(plot_data(&emitted), plot_data(source));
    }

    /// `merman` steps along an x axis without categories by
    /// `(max - min) / (values - 1)`; where that never gets from one end to
    /// the other it neither parses nor draws the chart, it runs until the
    /// memory is gone. Such a chart is refused before `merman` sees it, and
    /// the form cannot write one.
    #[test]
    fn an_x_axis_merman_never_finishes_is_refused() {
        let endless = [
            "xychart\n  x-axis 5 --> 5\n  bar [1, 2]",
            "xychart\n  bar [1]\n  line [1, 2]",
            "xychart\n  x-axis \"月\"\n  bar [1]\n  line [1, 2]",
            "xychart\n  x-axis 100000000000000000 --> 100000000000000032\n  bar [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]",
        ];
        for source in endless {
            let message = format!(
                "{}",
                parse_xychart(source)
                    .err()
                    .unwrap_or_else(|| panic!("`{source}` should be refused"))
            );
            assert!(message.contains("never finishes drawing"), "{message}");
        }

        let mut document = parse("xychart\n  bar [1, 2]\n  line [1, 2]");
        assert_eq!(document.structure_error(), None);
        if let Some(first) = document.series.first_mut() {
            first.values.pop();
        }
        assert!(
            document
                .structure_error()
                .is_some_and(|error| error.contains("first series")),
            "{:?}",
            document.structure_error()
        );

        // The neighbours of those charts finish, and open.
        for source in [
            "xychart\n  x-axis 5 --> 5\n  bar [1]",
            "xychart\n  x-axis 10 --> 0\n  bar [1, 2]",
            "xychart\n  bar [1, 2]\n  line [1]",
            "xychart\n  bar [1, 2]\n  line [1, 2, 3, 4]",
            "xychart\n  x-axis [a]\n  bar [1]\n  line [1, 2]",
        ] {
            let emitted = assert_stable(source);
            assert_eq!(parse(source).structure_error(), None, "{source}");
            assert_renders(&emitted);
        }
    }

    /// Mermaid cuts a `%%{init}%%` directive out of its line and reads what
    /// follows, which a preserved comment would hide from the form and move
    /// below the other statements.
    #[test]
    fn a_statement_after_a_directive_on_the_same_line_is_refused() {
        let source = "xychart\n  bar [1]\n%%{init: {}}%%   line [2]\n  bar [3]";
        assert_eq!(merman_model(source).map(|model| model.plots.len()), Some(3));
        let message = format!("{}", parse_xychart(source).err().expect("refused"));
        assert!(message.contains("after the directive"), "{message}");
        assert!(parse_xychart("xychart\n%%{init: {\n}}%% bar [1]").is_err());

        let source = "xychart\n  bar [1]\n%%{init: {}}%% %% メモ\n%%{init: {}}%%";
        let document = parse(source);
        assert_eq!(document.series.len(), 1);
        assert_stable(source);
    }

    /// Mermaid's statement splitter runs an open quote or bracket on into
    /// the lines below, so an accessibility line with one swallows the
    /// statements after it, which the form would still show.
    #[test]
    fn an_open_quote_in_an_accessibility_line_is_refused() {
        let source = "xychart\n  accTitle: 5\" の画面\n  title \"t\"\n  bar [1]";
        assert_eq!(merman_model(source).map(|model| model.plots.len()), Some(0));
        let message = format!("{}", parse_xychart(source).err().expect("refused"));
        assert!(message.contains("accTitle"), "{message}");

        let source =
            "xychart\n  accTitle: \"引用\" あり\n  accDescr {\n    [1] と \"2\"\n  }\n  bar [1]";
        assert_eq!(parse(source).series.len(), 1);
        assert_stable(source);
    }
}
