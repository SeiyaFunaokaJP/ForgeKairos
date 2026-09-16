//! Centers an edge label's `<text>` on the background box drawn for it.
//!
//! With `htmlLabels: false` (see `render::to_merman_config`) merman emits edge
//! labels as native SVG text:
//!
//! ```xml
//! <g class="edgeLabel" transform="translate(200,33)">
//!   <g class="label" transform="translate(-67.7,-11.5)">
//!     <g>
//!       <rect class="background" x="-2" y="-1" width="135.5" height="23"/>
//!       <text y="-10.1" text-anchor="middle"><tspan x="0" .../></text>
//!     </g>
//!   </g>
//! </g>
//! ```
//!
//! The `<g class="label">` offset and the background rect are derived from a
//! bounding box that assumes the text starts at `x = 0`, but the text is
//! emitted with `text-anchor="middle"` and its rows anchored at `x = 0`. A
//! renderer therefore draws the text centered on the *left edge* of the
//! background box, i.e. half the label's width too far left. On a
//! left-to-right flowchart that lands the label on top of the node the edge
//! comes from, and since `<g class="edgeLabels">` precedes `<g class="nodes">`
//! in document order, the node paints over the label and the text disappears.
//! Class diagram edge labels are written by the same merman helper and are off
//! by the same amount.
//!
//! This pass shifts such a `<text>` back onto the center of its own background
//! box. Labels merman already centers (e.g. ER relationship labels, whose
//! background is emitted centered on the anchor) have a background center of
//! zero and are left untouched.

use anyhow::Result;
use quick_xml::events::{BytesStart, Event};

struct CenterEdgeLabels<I> {
    inner: I,
    /// Element nesting depth inside the current `<g class="edgeLabel">`; zero
    /// while outside one.
    edge_label_depth: usize,
    /// Horizontal center of the background rect that was just emitted, in the
    /// coordinate system its sibling `<text>` is placed in.
    background_center_x: Option<f64>,
}

/// The distance below which recentering is a no-op and the element is left
/// exactly as merman wrote it.
const MIN_SHIFT: f64 = 0.01;

fn attr_f64(e: &BytesStart<'_>, name: &str) -> Result<Option<f64>> {
    let Some(attr) = e.try_get_attribute(name)? else {
        return Ok(None);
    };
    Ok(attr.unescape_value()?.trim().parse::<f64>().ok())
}

fn has_class(e: &BytesStart<'_>, class: &str) -> Result<bool> {
    let Some(attr) = e.try_get_attribute("class")? else {
        return Ok(false);
    };
    Ok(attr
        .unescape_value()?
        .split_ascii_whitespace()
        .any(|token| token == class))
}

fn background_center_x(e: &BytesStart<'_>) -> Result<Option<f64>> {
    if !has_class(e, "background")? {
        return Ok(None);
    }
    let Some(width) = attr_f64(e, "width")? else {
        return Ok(None);
    };
    let x = attr_f64(e, "x")?.unwrap_or(0.0);
    if !width.is_finite() || width <= 0.0 || !x.is_finite() {
        return Ok(None);
    }
    Ok(Some(x + width / 2.0))
}

fn with_translate_x<'a>(e: &BytesStart<'_>, shift: f64) -> Result<BytesStart<'a>> {
    let name = e.name();
    let tag = std::str::from_utf8(name.as_ref())?;
    let mut new_elem = BytesStart::new(tag.to_owned());
    new_elem.push_attribute(("transform", format!("translate({shift},0)").as_str()));
    for attr in e.attributes() {
        new_elem.push_attribute(attr?);
    }
    Ok(new_elem)
}

impl<I> CenterEdgeLabels<I> {
    /// How far `e` has to move for its anchor to land on the pending
    /// background center, or `None` when it must be left alone.
    fn text_shift(&self, e: &BytesStart<'_>) -> Result<Option<f64>> {
        let Some(center_x) = self.background_center_x else {
            return Ok(None);
        };
        // A `transform` of merman's own would compose with the one added
        // here, and an `x` means the anchor is not at the origin, which is
        // what the shift below is measured from. Both appear on the
        // `<text>` of a foreignObject fallback label, which merman already
        // positions correctly.
        if e.try_get_attribute("transform")?.is_some() || e.try_get_attribute("x")?.is_some() {
            return Ok(None);
        }
        let is_centered = e
            .try_get_attribute("text-anchor")?
            .map(|attr| attr.unescape_value())
            .transpose()?
            .is_some_and(|anchor| anchor.trim() == "middle");
        if !is_centered || center_x.abs() < MIN_SHIFT {
            return Ok(None);
        }
        Ok(Some(center_x))
    }

    fn process_event<'a>(&mut self, event: Event<'a>) -> Result<Event<'a>> {
        if self.edge_label_depth == 0 {
            if let Event::Start(e) = &event {
                if has_class(e, "edgeLabel")? {
                    self.edge_label_depth = 1;
                }
            }
            return Ok(event);
        }

        let event = match &event {
            Event::Start(e) | Event::Empty(e) if e.name().as_ref() == b"text" => {
                match self.text_shift(e)? {
                    Some(shift) => {
                        let new_elem = with_translate_x(e, shift)?;
                        match event {
                            Event::Start(_) => Event::Start(new_elem),
                            _ => Event::Empty(new_elem),
                        }
                    }
                    None => event,
                }
            }
            _ => event,
        };

        match &event {
            Event::Start(e) => {
                self.edge_label_depth += 1;
                self.background_center_x = background_center_x_of(e)?;
            }
            Event::Empty(e) => {
                self.background_center_x = background_center_x_of(e)?;
            }
            Event::End(_) => {
                self.background_center_x = None;
                self.edge_label_depth = self.edge_label_depth.saturating_sub(1);
            }
            _ => {}
        }

        Ok(event)
    }
}

/// The background center to remember for the next `<text>`, which is cleared
/// by anything other than a background rect so that only a rect immediately
/// preceding the text can move it.
fn background_center_x_of(e: &BytesStart<'_>) -> Result<Option<f64>> {
    if e.name().as_ref() == b"rect" {
        background_center_x(e)
    } else {
        Ok(None)
    }
}

impl<'a, I: Iterator<Item = Result<Event<'a>>>> Iterator for CenterEdgeLabels<I> {
    type Item = Result<Event<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        let event = match self.inner.next()? {
            Ok(event) => event,
            Err(err) => return Some(Err(err)),
        };
        Some(self.process_event(event))
    }
}

pub(super) fn process<'a>(
    events: impl Iterator<Item = Result<Event<'a>>>,
) -> impl Iterator<Item = Result<Event<'a>>> {
    CenterEdgeLabels {
        inner: events,
        edge_label_depth: 0,
        background_center_x: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MermaidTheme;
    use quick_xml::Reader;

    /// Merman truncates the `font-family` attribute it writes into `<text>`
    /// elements at ~258 bytes, which corrupts the SVG for the long default
    /// stack of [`MermaidTheme::default`]. Diagram tests therefore use a short
    /// family, like the real app does.
    fn theme() -> MermaidTheme {
        MermaidTheme {
            font_family: "Zed Plex Sans, sans-serif".to_string(),
            ..MermaidTheme::default()
        }
    }

    #[derive(Debug, Clone, Copy)]
    struct Rect {
        left: f64,
        top: f64,
        right: f64,
        bottom: f64,
    }

    impl Rect {
        fn center_x(&self) -> f64 {
            (self.left + self.right) / 2.0
        }

        fn overlaps(&self, other: &Rect) -> bool {
            self.left < other.right
                && other.left < self.right
                && self.top < other.bottom
                && other.top < self.bottom
        }
    }

    #[derive(Debug, Default)]
    struct EdgeLabel {
        background: Option<Rect>,
        /// Absolute x the label's rows are anchored at.
        text_anchor_x: Option<f64>,
        text: String,
    }

    fn translate_of(e: &BytesStart<'_>) -> (f64, f64) {
        let Ok(Some(attr)) = e.try_get_attribute("transform") else {
            return (0.0, 0.0);
        };
        let Ok(value) = attr.unescape_value() else {
            return (0.0, 0.0);
        };
        let Some((_, rest)) = value.split_once("translate(") else {
            return (0.0, 0.0);
        };
        let Some((args, _)) = rest.split_once(')') else {
            return (0.0, 0.0);
        };
        let mut parts = args
            .split([',', ' '])
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(|part| part.parse::<f64>().unwrap_or(0.0));
        (parts.next().unwrap_or(0.0), parts.next().unwrap_or(0.0))
    }

    fn shape_rect(e: &BytesStart<'_>, x: f64, y: f64) -> Option<Rect> {
        let number = |name: &str| attr_f64(e, name).ok().flatten().unwrap_or(0.0);
        match e.name().as_ref() {
            b"rect" => {
                let width = attr_f64(e, "width").ok().flatten()?;
                let height = attr_f64(e, "height").ok().flatten()?;
                let left = x + number("x");
                let top = y + number("y");
                Some(Rect {
                    left,
                    top,
                    right: left + width,
                    bottom: top + height,
                })
            }
            b"circle" => {
                let radius = attr_f64(e, "r").ok().flatten()?;
                Some(Rect {
                    left: x + number("cx") - radius,
                    top: y + number("cy") - radius,
                    right: x + number("cx") + radius,
                    bottom: y + number("cy") + radius,
                })
            }
            _ => None,
        }
    }

    /// Walks a rendered diagram and returns every node shape and every edge
    /// label, in absolute user-space coordinates.
    fn collect(svg: &str) -> (Vec<Rect>, Vec<EdgeLabel>) {
        let mut reader = Reader::from_str(svg);
        reader.config_mut().check_end_names = false;
        let mut offsets = vec![(0.0, 0.0)];
        let mut node_depth: Option<usize> = None;
        let mut edge_label_depth: Option<usize> = None;
        let mut nodes = Vec::new();
        let mut labels: Vec<EdgeLabel> = Vec::new();
        let mut text_depth: Option<usize> = None;

        loop {
            let event = reader
                .read_event()
                .expect("a rendered diagram must be well-formed XML");
            let (e, is_start) = match &event {
                Event::Eof => break,
                Event::Start(e) => (e, true),
                Event::Empty(e) => (e, false),
                Event::Text(bytes) if text_depth.is_some() => {
                    if let (Some(label), Ok(text)) = (labels.last_mut(), bytes.xml_content()) {
                        label.text.push_str(&text);
                    }
                    continue;
                }
                Event::End(_) => {
                    let depth = offsets.len();
                    if node_depth == Some(depth) {
                        node_depth = None;
                    }
                    if edge_label_depth == Some(depth) {
                        edge_label_depth = None;
                    }
                    if text_depth == Some(depth) {
                        text_depth = None;
                    }
                    offsets.pop();
                    continue;
                }
                _ => continue,
            };

            let (parent_x, parent_y) = *offsets.last().expect("the root offset is never popped");
            let (dx, dy) = translate_of(e);
            let (x, y) = (parent_x + dx, parent_y + dy);
            if is_start {
                offsets.push((x, y));
            }
            let depth = offsets.len();
            let is_background = has_class(e, "background").unwrap_or(false);

            if node_depth.is_none() && has_class(e, "node").unwrap_or(false) {
                node_depth = Some(depth);
            }
            if edge_label_depth.is_none() && has_class(e, "edgeLabel").unwrap_or(false) {
                edge_label_depth = Some(depth);
                labels.push(EdgeLabel::default());
            }

            if node_depth.is_some() && !is_background {
                if let Some(rect) = shape_rect(e, x, y) {
                    nodes.push(rect);
                }
            }
            if edge_label_depth.is_some() {
                if let Some(label) = labels.last_mut() {
                    if is_background {
                        label.background = shape_rect(e, x, y);
                    }
                    if e.name().as_ref() == b"text" {
                        label.text_anchor_x = Some(x);
                        if is_start {
                            text_depth = Some(depth);
                        }
                    }
                }
            }
        }

        (nodes, labels)
    }

    const DIAGRAMS: &[(&str, &str)] = &[
        (
            "flowchart LR, English",
            "flowchart LR\n    A[Start] -->|approved request| B[Review]\n    B -- rejected with reason --> C[Reject]",
        ),
        (
            "flowchart LR, Japanese",
            "flowchart LR\n    A[開始] -->|承認された申請| B[レビュー]\n    B -- 理由を付けて却下された --> C[却下]",
        ),
        (
            "flowchart TD, Japanese",
            "flowchart TD\n    A[開始] -->|承認された申請| B[レビュー]\n    B -->|却下| C[終了]\n    B -->|保留にする| D[保留]",
        ),
        (
            "flowchart LR, long Japanese label",
            "flowchart LR\n    A[開始] -->|この処理はとても長い日本語のラベルで説明されています| B[レビュー]",
        ),
        (
            "classDiagram, Japanese",
            "classDiagram\n    class 顧客 {\n        +名前 文字列\n    }\n    顧客 --> 注文 : 発注する",
        ),
        (
            "erDiagram, Japanese",
            "erDiagram\n    顧客 ||--o{ 注文 : \"発注する\"",
        ),
    ];

    /// An edge label's text must sit on the background box drawn for it, and
    /// that box must not land on a node: `<g class="edgeLabels">` is emitted
    /// before `<g class="nodes">`, so anything under a node is painted over
    /// and disappears.
    #[test]
    fn edge_labels_sit_on_their_background_and_clear_the_nodes() {
        for (name, source) in DIAGRAMS {
            let svg = crate::render_to_svg(source, &theme())
                .unwrap_or_else(|err| panic!("{name} failed to render: {err:#}"));
            let (nodes, labels) = collect(&svg);
            assert!(
                labels.iter().any(|label| label.background.is_some()),
                "{name} rendered no edge label backgrounds, so this case tests nothing"
            );

            for label in &labels {
                let Some(background) = label.background else {
                    continue;
                };
                let anchor = label
                    .text_anchor_x
                    .unwrap_or_else(|| panic!("{name}: edge label background without text"));
                assert!(
                    (anchor - background.center_x()).abs() < 0.5,
                    "{name}: label {:?} is anchored at {anchor} but its background spans \
                     {}..{}, so the text is drawn beside its own background",
                    label.text,
                    background.left,
                    background.right,
                );
                for node in &nodes {
                    assert!(
                        !background.overlaps(node),
                        "{name}: label {:?} at {}..{} overlaps a node at {}..{}",
                        label.text,
                        background.left,
                        background.right,
                        node.left,
                        node.right,
                    );
                }
            }
        }
    }

    /// Labels merman already centers must come through untouched: ER
    /// relationship labels emit their background centered on the text anchor,
    /// and state diagram labels arrive as foreignObject fallbacks whose text
    /// carries an absolute `x`.
    #[test]
    fn already_centered_labels_are_left_alone() {
        for source in [
            "erDiagram\n    顧客 ||--o{ 注文 : \"発注する\"",
            "stateDiagram-v2\n    [*] --> 待機中\n    待機中 --> 実行中: 開始要求を受理",
        ] {
            let svg = crate::render_to_svg(source, &theme())
                .unwrap_or_else(|err| panic!("{source} failed to render: {err:#}"));
            assert!(
                !svg.contains(r#"<text transform="translate("#),
                "an already-centered label was shifted: {svg}"
            );
        }
    }
}
