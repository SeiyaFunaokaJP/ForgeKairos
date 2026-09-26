//! The labels of pie charts, gantt charts and ER attribute cells, which are
//! neither nodes nor edges and are reported as [`LabelGeometry`].
//!
//! merman spells them like this:
//!
//! * pie: every legend entry is a `<g class="legend" transform="translate(…)">`
//!   holding an 18px `<rect>` and a `<text>`, in the order of the slices merman
//!   keeps (one per distinct label). The title is a `<text class="pieTitleText">`.
//!   The slices themselves carry nothing that names them, and a wedge's
//!   bounding box covers its neighbours, so they are not mapped.
//! * gantt: a task is a `<rect id="{diagram id}-{task id}" class="task …">` and
//!   a `<text id="{diagram id}-{task id}-text">`; a section is a
//!   `<text class="sectionTitle …">` with a `<tspan>` per line; the title is a
//!   `<text class="titleText">`.
//! * ER attributes: each attribute row of an entity is a
//!   `<g class="row-rect-odd">` / `<g class="row-rect-even">` of paths inside
//!   the entity's node, and the attribute's name is a `<foreignObject>` label
//!   lifted out to the end of the document (see the parent module), anchored at
//!   the centre of its cell.
//!
//! A `<text>` only says where it is anchored, so its box is estimated from the
//! characters it holds and its font size. That is enough to hit-test a label
//! with, which is all these boxes are used for.

use quick_xml::events::BytesStart;

use super::{
    BoundingBox, CollectGeometry, DiagramKind, Frame, PendingFallback, attribute, attribute_f64,
    extend_optional, has_class, own_translate, shape_bounds,
};
use crate::{LabelGeometry, LabelItem};

/// Pie legend text size, from merman's pie stylesheet (`pieLegendTextSize`).
const PIE_LEGEND_FONT_SIZE: f64 = 17.0;
/// Pie title size, from merman's pie stylesheet (`pieTitleTextSize`).
const PIE_TITLE_FONT_SIZE: f64 = 25.0;
/// Gantt title size, from merman's gantt stylesheet.
const GANTT_TITLE_FONT_SIZE: f64 = 18.0;
/// merman's default when a label says nothing about its size.
const DEFAULT_FONT_SIZE: f64 = 16.0;
/// Slack around an estimated text box, so a click on the edge of a glyph that
/// is wider than the estimate still lands.
const TEXT_PADDING: f64 = 2.0;

#[derive(Clone, Copy, PartialEq)]
enum Anchor {
    Start,
    Middle,
    End,
}

#[derive(Clone, Copy, PartialEq)]
enum Baseline {
    /// SVG's default: `y` is where the glyphs stand.
    Alphabetic,
    /// `y` is the middle of the first line.
    Central,
}

enum TextRole {
    PieTitle,
    PieLegend,
    GanttTitle,
    GanttSection,
    GanttTask(String),
}

struct OpenText {
    role: TextRole,
    start_depth: usize,
    lines: Vec<String>,
    x: f64,
    y: f64,
    font_size: f64,
    /// The width merman measured the text at, where it says (a task label's
    /// `width-{n}` class).
    measured_width: Option<f64>,
    anchor: Anchor,
    baseline: Baseline,
}

struct OpenLegend {
    start_depth: usize,
    bounds: Option<BoundingBox>,
    label: String,
}

struct OpenRow {
    start_depth: usize,
    bounds: Option<BoundingBox>,
}

#[derive(Default)]
pub(super) struct ChartLabels {
    text: Option<OpenText>,
    legend: Option<OpenLegend>,
    legend_count: usize,
    labels: Vec<LabelGeometry>,
    task_bars: Vec<(String, BoundingBox)>,
    task_texts: Vec<(String, String, BoundingBox)>,
    row: Option<OpenRow>,
    /// The attribute rows of the entity being walked.
    rows: Vec<BoundingBox>,
    /// The attribute rows of every entity, by entity name.
    entity_rows: Vec<(String, Vec<BoundingBox>)>,
}

impl ChartLabels {
    pub(super) fn has_open_text(&self) -> bool {
        self.text.is_some()
    }
}

/// A rough advance for `text`: a full em for the wide characters Japanese is
/// written in, a little over half of one for everything else.
fn estimated_text_width(text: &str, font_size: f64) -> f64 {
    text.chars()
        .map(|character| {
            if character == ' ' {
                0.3
            } else if is_wide(character) {
                1.0
            } else {
                0.6
            }
        })
        .sum::<f64>()
        * font_size
}

fn is_wide(character: char) -> bool {
    matches!(character as u32,
        0x1100..=0x115F
        | 0x2E80..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1FAFF
        | 0x20000..=0x3FFFD)
}

/// The size a `font-size` attribute or a `font-size:` style declaration gives.
pub(super) fn font_size_of(element: &BytesStart<'_>) -> Option<f64> {
    if let Some(size) = attribute(element, "font-size") {
        return parse_pixels(&size);
    }
    let style = attribute(element, "style")?;
    style.split(';').find_map(|declaration| {
        let (name, value) = declaration.split_once(':')?;
        (name.trim() == "font-size")
            .then(|| parse_pixels(value))
            .flatten()
    })
}

fn parse_pixels(value: &str) -> Option<f64> {
    let value = value.trim();
    value
        .strip_suffix("px")
        .unwrap_or(value)
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|size| size.is_finite() && *size > 0.0)
}

fn text_box(text: &OpenText) -> Option<BoundingBox> {
    let width = text.measured_width.unwrap_or_else(|| {
        text.lines
            .iter()
            .map(|line| estimated_text_width(line.trim_end(), text.font_size))
            .fold(0.0_f64, f64::max)
    });
    // NaN must fall through to `None` as well, hence the explicit test.
    if width.is_nan() || width <= 0.0 {
        return None;
    }
    let left = match text.anchor {
        Anchor::Start => text.x,
        Anchor::Middle => text.x - width / 2.0,
        Anchor::End => text.x - width,
    };
    let line_count = text.lines.len().max(1) as f64;
    let top = match text.baseline {
        Baseline::Alphabetic => text.y - text.font_size * 0.8,
        Baseline::Central => text.y - text.font_size / 2.0,
    };
    let mut bounds = BoundingBox::point(left - TEXT_PADDING, top - TEXT_PADDING);
    bounds.extend(
        left + width + TEXT_PADDING,
        top + line_count * text.font_size + TEXT_PADDING,
    );
    Some(bounds)
}

impl<I> CollectGeometry<I> {
    /// Walks the elements of a pie or a gantt chart. Everything they map is
    /// outside the node and edge sections the other kinds use.
    pub(super) fn on_chart_element(
        &mut self,
        element: &BytesStart<'_>,
        depth: usize,
        empty: bool,
        frame: Frame,
        kind: DiagramKind,
    ) {
        if !frame.usable {
            return;
        }
        let name = element.name();
        let name = name.as_ref();
        if let Some(text) = self.chart.text.as_mut() {
            if name == b"tspan" {
                text.lines.push(String::new());
            }
            return;
        }
        match kind {
            DiagramKind::Pie => self.on_pie_element(element, name, depth, empty, frame),
            DiagramKind::Gantt => self.on_gantt_element(element, name, depth, empty, frame),
            _ => {}
        }
    }

    fn on_pie_element(
        &mut self,
        element: &BytesStart<'_>,
        name: &[u8],
        depth: usize,
        empty: bool,
        frame: Frame,
    ) {
        if let Some(legend) = self.chart.legend.as_mut() {
            match name {
                b"rect" => {
                    if let Some(bounds) = shape_bounds(element) {
                        extend_optional(&mut legend.bounds, bounds.translated(frame.dx, frame.dy));
                    }
                }
                b"text" if !empty => {
                    self.open_text(
                        element,
                        depth,
                        frame,
                        TextRole::PieLegend,
                        PIE_LEGEND_FONT_SIZE,
                        Anchor::Start,
                    );
                }
                _ => {}
            }
            return;
        }
        if name == b"g" && has_class(element, "legend") && !empty {
            self.chart.legend = Some(OpenLegend {
                start_depth: depth,
                bounds: None,
                label: String::new(),
            });
        } else if name == b"text" && has_class(element, "pieTitleText") && !empty {
            self.open_text(
                element,
                depth,
                frame,
                TextRole::PieTitle,
                PIE_TITLE_FONT_SIZE,
                Anchor::Middle,
            );
        }
    }

    fn on_gantt_element(
        &mut self,
        element: &BytesStart<'_>,
        name: &[u8],
        depth: usize,
        empty: bool,
        frame: Frame,
    ) {
        match name {
            // A `vert` task is a marker line across the whole chart, whose bar
            // would claim every click near it; its label alone is mapped.
            b"rect" if has_class(element, "task") && !has_class(element, "vert") => {
                let Some(task_id) = attribute(element, "id")
                    .and_then(|id| self.gantt_task_id(&id).map(str::to_string))
                else {
                    return;
                };
                if let Some(bounds) = shape_bounds(element) {
                    self.chart
                        .task_bars
                        .push((task_id, bounds.translated(frame.dx, frame.dy)));
                }
            }
            b"text" if !empty => {
                if has_class(element, "titleText") {
                    self.open_text(
                        element,
                        depth,
                        frame,
                        TextRole::GanttTitle,
                        GANTT_TITLE_FONT_SIZE,
                        Anchor::Middle,
                    );
                } else if has_class(element, "sectionTitle") {
                    self.open_text(
                        element,
                        depth,
                        frame,
                        TextRole::GanttSection,
                        DEFAULT_FONT_SIZE,
                        Anchor::Start,
                    );
                    if let Some(text) = self.chart.text.as_mut() {
                        text.baseline = Baseline::Central;
                        // The whole block is shifted up by half its extra
                        // lines, which merman writes as `dy` in ems.
                        let shift = attribute(element, "dy")
                            .and_then(|dy| dy.trim().strip_suffix("em")?.trim().parse::<f64>().ok())
                            .unwrap_or(0.0);
                        text.y += shift * text.font_size;
                    }
                } else if let Some(task_id) = attribute(element, "id").and_then(|id| {
                    self.gantt_task_id(&id)
                        .and_then(|id| id.strip_suffix("-text"))
                        .map(str::to_string)
                }) {
                    let anchor = if has_class(element, "taskTextOutsideLeft") {
                        Anchor::End
                    } else if has_class(element, "taskTextOutsideRight") {
                        Anchor::Start
                    } else {
                        Anchor::Middle
                    };
                    self.open_text(
                        element,
                        depth,
                        frame,
                        TextRole::GanttTask(task_id),
                        DEFAULT_FONT_SIZE,
                        anchor,
                    );
                }
            }
            _ => {}
        }
    }

    /// A gantt element id with the diagram's own id taken off.
    fn gantt_task_id<'a>(&self, element_id: &'a str) -> Option<&'a str> {
        element_id
            .strip_prefix(self.svg_id.as_str())?
            .strip_prefix('-')
            .filter(|id| !id.is_empty())
    }

    fn open_text(
        &mut self,
        element: &BytesStart<'_>,
        depth: usize,
        frame: Frame,
        role: TextRole,
        default_font_size: f64,
        anchor: Anchor,
    ) {
        let Some((dx, dy)) = own_translate(element) else {
            return;
        };
        self.chart.text = Some(OpenText {
            role,
            start_depth: depth,
            lines: Vec::new(),
            x: frame.dx + dx + attribute_f64(element, "x").unwrap_or(0.0),
            y: frame.dy + dy + attribute_f64(element, "y").unwrap_or(0.0),
            font_size: font_size_of(element).unwrap_or(default_font_size),
            measured_width: attribute(element, "class").and_then(|class| {
                class
                    .split_ascii_whitespace()
                    .find_map(|token| token.strip_prefix("width-")?.parse::<f64>().ok())
                    .filter(|width| width.is_finite() && *width > 0.0)
            }),
            anchor,
            baseline: Baseline::Alphabetic,
        });
    }

    pub(super) fn on_chart_text(&mut self, content: &str) {
        let Some(text) = self.chart.text.as_mut() else {
            return;
        };
        match text.lines.last_mut() {
            Some(line) => line.push_str(content),
            None => text.lines.push(content.to_string()),
        }
    }

    pub(super) fn on_chart_end(&mut self, depth: usize) {
        if self
            .chart
            .text
            .as_ref()
            .is_some_and(|text| text.start_depth == depth)
            && let Some(text) = self.chart.text.take()
        {
            self.close_text(text);
        }
        if self
            .chart
            .legend
            .as_ref()
            .is_some_and(|legend| legend.start_depth == depth)
            && let Some(legend) = self.chart.legend.take()
        {
            // Every entry takes its place in the count, mapped or not, since
            // the position is what names the slice.
            let index = self.chart.legend_count;
            self.chart.legend_count += 1;
            if let Some(bounds) = legend.bounds.filter(BoundingBox::is_usable) {
                self.chart.labels.push(LabelGeometry {
                    item: LabelItem::PieSlice { index },
                    label: legend.label,
                    rect: bounds.to_rect(),
                });
            }
        }
    }

    fn close_text(&mut self, text: OpenText) {
        let label = text
            .lines
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n");
        let bounds = text_box(&text);
        match text.role {
            TextRole::PieLegend => {
                if let Some(legend) = self.chart.legend.as_mut() {
                    legend.label = label;
                    if let Some(bounds) = bounds {
                        extend_optional(&mut legend.bounds, bounds);
                    }
                }
            }
            TextRole::GanttTask(task_id) => {
                if let Some(bounds) = bounds {
                    self.chart.task_texts.push((task_id, label, bounds));
                }
            }
            TextRole::PieTitle | TextRole::GanttTitle => {
                if let Some(bounds) = bounds.filter(|_| !label.trim().is_empty()) {
                    self.chart.labels.push(LabelGeometry {
                        item: LabelItem::Title,
                        label,
                        rect: bounds.to_rect(),
                    });
                }
            }
            TextRole::GanttSection => {
                if let Some(bounds) = bounds.filter(|_| !label.trim().is_empty()) {
                    self.chart.labels.push(LabelGeometry {
                        item: LabelItem::GanttSection {
                            name: label.clone(),
                        },
                        label,
                        rect: bounds.to_rect(),
                    });
                }
            }
        }
    }

    /// Reads an ER entity's attribute rows. Returns whether `element` was part
    /// of one, in which case it says nothing about the entity's own box.
    pub(super) fn on_er_row_element(
        &mut self,
        element: &BytesStart<'_>,
        depth: usize,
        empty: bool,
        frame: Frame,
    ) -> bool {
        if let Some(row) = self.chart.row.as_mut() {
            if frame.usable
                && let Some(bounds) = shape_bounds(element)
            {
                extend_optional(&mut row.bounds, bounds.translated(frame.dx, frame.dy));
            }
            return true;
        }
        if element.name().as_ref() == b"g"
            && (has_class(element, "row-rect-odd") || has_class(element, "row-rect-even"))
        {
            if !empty {
                self.chart.row = Some(OpenRow {
                    start_depth: depth,
                    bounds: None,
                });
            }
            return true;
        }
        false
    }

    pub(super) fn on_er_row_end(&mut self, depth: usize) {
        if self
            .chart
            .row
            .as_ref()
            .is_some_and(|row| row.start_depth == depth)
            && let Some(row) = self.chart.row.take()
        {
            // An unusable row is still a row: dropping it would shift every
            // attribute below it onto the wrong name.
            self.chart.rows.push(
                row.bounds
                    .filter(BoundingBox::is_usable)
                    .unwrap_or(BoundingBox::point(f64::NAN, f64::NAN)),
            );
        }
    }

    /// Called when an ER entity's node closes, with its name, or `None` when
    /// the node was not mapped.
    pub(super) fn on_er_entity_end(&mut self, entity: Option<String>) {
        self.chart.row = None;
        let rows = std::mem::take(&mut self.chart.rows);
        if let Some(entity) = entity
            && !rows.is_empty()
        {
            self.chart.entity_rows.push((entity, rows));
        }
    }

    /// Everything this module mapped, once the document has been read.
    pub(super) fn finish_chart_labels(
        &mut self,
        attribute_names: Vec<PendingFallback>,
    ) -> Vec<LabelGeometry> {
        let mut labels = std::mem::take(&mut self.chart.labels);

        let bars = std::mem::take(&mut self.chart.task_bars);
        let texts = std::mem::take(&mut self.chart.task_texts);
        let task_ids = bars
            .iter()
            .map(|(id, _)| id.as_str())
            .chain(texts.iter().map(|(id, _, _)| id.as_str()));
        let mut seen: Vec<&str> = Vec::new();
        for task_id in task_ids {
            if seen.contains(&task_id) {
                continue;
            }
            seen.push(task_id);
            let task_bars: Vec<_> = bars.iter().filter(|(id, _)| id == task_id).collect();
            let task_texts: Vec<_> = texts.iter().filter(|(id, _, _)| id == task_id).collect();
            // Two tasks with the same id cannot be told apart, and editing
            // the one would be read back as the other.
            if task_bars.len() > 1 || task_texts.len() > 1 {
                continue;
            }
            let mut bounds: Option<BoundingBox> = None;
            for (_, bar) in &task_bars {
                extend_optional(&mut bounds, *bar);
            }
            for (_, _, text) in &task_texts {
                extend_optional(&mut bounds, *text);
            }
            let Some(bounds) = bounds.filter(BoundingBox::is_usable) else {
                continue;
            };
            labels.push(LabelGeometry {
                item: LabelItem::GanttTask {
                    task_id: task_id.to_string(),
                },
                label: task_texts
                    .first()
                    .map(|(_, label, _)| label.clone())
                    .unwrap_or_default(),
                rect: bounds.to_rect(),
            });
        }

        let entity_rows = std::mem::take(&mut self.chart.entity_rows);
        for name in attribute_names {
            let (Some((x, y)), text) = (name.anchor, name.text.trim()) else {
                continue;
            };
            if text.is_empty() {
                continue;
            }
            let mut claimants = entity_rows.iter().flat_map(|(entity, rows)| {
                rows.iter()
                    .enumerate()
                    .filter(move |(_, row)| {
                        row.is_usable()
                            && x >= row.min_x
                            && x <= row.max_x
                            && y >= row.min_y
                            && y <= row.max_y
                    })
                    .map(move |(index, row)| (entity, index, *row))
            });
            let Some((entity, index, row)) = claimants.next() else {
                continue;
            };
            if claimants.next().is_some() {
                continue;
            }
            // The fallback text is anchored at the middle of its cell.
            let half_width =
                estimated_text_width(text, name.font_size.unwrap_or(DEFAULT_FONT_SIZE)) / 2.0
                    + TEXT_PADDING;
            let mut cell = BoundingBox::point((x - half_width).max(row.min_x), row.min_y);
            cell.extend((x + half_width).min(row.max_x), row.max_y);
            if !cell.is_usable() {
                continue;
            }
            labels.push(LabelGeometry {
                item: LabelItem::ErAttribute {
                    entity: entity.clone(),
                    index,
                },
                label: text.to_string(),
                rect: cell.to_rect(),
            });
        }

        labels
    }
}

#[cfg(test)]
#[path = "chart_labels_tests.rs"]
mod tests;
