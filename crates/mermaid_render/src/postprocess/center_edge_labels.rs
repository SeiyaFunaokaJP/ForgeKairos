//! Centers an edge label's `<text>` on the background box drawn for it, and
//! centers the box itself on the edge it annotates.
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
//! The `<g class="label">` offset is derived from a bounding box that assumes
//! the label's own content starts at `x = 0`, so it shifts everything inside
//! by minus half the label's width to center that assumed box on the edge
//! (`translate(200,33)` above is a point on the edge). The background rect
//! goes along with that assumption (`x = -2`, i.e. starting near zero), so it
//! ends up correctly centered on the edge. But the text is emitted with
//! `text-anchor="middle"` and its rows anchored at `x = 0`, which was only
//! correct *before* the `-w/2` wrapper offset was introduced; a renderer
//! therefore draws the text centered on the *left edge* of the background
//! box, i.e. half the label's width too far left. On a left-to-right
//! flowchart that lands the label on top of the node the edge comes from, and
//! since `<g class="edgeLabels">` precedes `<g class="nodes">` in document
//! order, the node paints over the label and the text disappears. Class
//! diagram edge labels are written by the same merman helper and are off by
//! the same amount. This pass fixes that by shifting such a `<text>` back
//! onto the center of its own sibling background box.
//!
//! ER relationship labels go through a different merman code path that
//! already draws the background rect centered on its own local origin
//! (`x = -width/2`), with the text's `x = 0` anchor matching it correctly.
//! But that rect is *still* wrapped in a `<g class="label">` that applies the
//! same `-w/2` assumption meant for the `x = 0`-based boxes above, so the
//! whole label — box and text together, already mutually aligned — ends up
//! shifted a further half-width away from the edge it annotates. This pass
//! detects that double shift (a background rect centered on zero, inside a
//! wrapper whose own offset is exactly `-width/2`) and nudges the rect (and,
//! following it, the text) back by `+width/2` to cancel it out.
//!
//! Both fixes are idempotent: a rect or text this pass has already shifted
//! carries its own `transform`, which is checked before shifting it again,
//! and the ER fix additionally checks the *wrapper's* offset before acting,
//! so a hypothetical future merman that no longer double-shifts (wrapper
//! offset no longer `-width/2`) is left alone rather than over-corrected.

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
    /// The `translate(dx, ...)` of the current edge label's `<g class="label">`
    /// wrapper (the direct child of `<g class="edgeLabel">`), used to tell an
    /// ER-style double-shifted background rect apart from one that is not.
    label_group_dx: Option<f64>,
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

/// The `dx` of `e`'s own `transform="translate(dx[, dy])"`, or `0.0` if it has
/// none, or a transform other than `translate` (this pass never writes any
/// other kind, and merman does not use any other kind for these wrappers).
fn transform_translate_dx(e: &BytesStart<'_>) -> Result<f64> {
    let Some(attr) = e.try_get_attribute("transform")? else {
        return Ok(0.0);
    };
    let value = attr.unescape_value()?;
    let Some((_, rest)) = value.split_once("translate(") else {
        return Ok(0.0);
    };
    let Some((args, _)) = rest.split_once(')') else {
        return Ok(0.0);
    };
    Ok(args
        .split([',', ' '])
        .map(str::trim)
        .find(|part| !part.is_empty())
        .and_then(|part| part.parse::<f64>().ok())
        .unwrap_or(0.0))
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
        // positions correctly. A `transform` is also how an earlier pass
        // over this same SVG marks a `<text>` it already shifted.
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

    /// Inspects `e` for the `background` class merman gives an edge label's
    /// rect. Returns `Ok(None)` when `e` is not one (its sibling `<text>`
    /// then gets no shift). Otherwise returns the local x-center the
    /// sibling `<text>` should align to, together with a replacement element
    /// for `e` itself when the rect needs to move (`None` to keep it as-is).
    fn recenter_background_rect<'a>(
        &self,
        e: &BytesStart<'a>,
    ) -> Result<Option<(Option<BytesStart<'a>>, f64)>> {
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
        let local_center = x + width / 2.0;

        if e.try_get_attribute("transform")?.is_some() {
            // Either an earlier pass over this same SVG already nudged this
            // rect, or merman placed it with a transform of its own. Report
            // where it ended up rather than pushing a second `transform`
            // attribute, which would be malformed and would drop whichever of
            // the two a renderer discards.
            return Ok(Some((None, local_center + transform_translate_dx(e)?)));
        }

        // The ER double-shift bug (see the module doc): a background rect
        // already centered on its own local zero, wrapped in a `<g
        // class="label">` whose own offset is exactly minus half this rect's
        // width. Both conditions matter: a rect centered on zero inside a
        // wrapper that *isn't* also offset by `-width/2` is not
        // double-shifted (e.g. a hypothetical merman that stops applying the
        // redundant wrapper offset), so it must be left alone.
        //
        // The wrapper offset is *not* what tells the diagram kinds apart:
        // flowchart and class diagram wrappers are offset by exactly
        // `-width/2` too. What singles out the ER path is the rect's own
        // `x ~= -width/2`; the second condition only keeps a future merman
        // that fixes the wrapper from being over-corrected here.
        let label_group_dx = self.label_group_dx.unwrap_or(0.0);
        let is_double_shifted =
            local_center.abs() < MIN_SHIFT && (label_group_dx + width / 2.0).abs() < MIN_SHIFT;
        if is_double_shifted {
            let shift = width / 2.0;
            Ok(Some((
                Some(with_translate_x(e, shift)?),
                local_center + shift,
            )))
        } else {
            Ok(Some((None, local_center)))
        }
    }

    fn process_event<'a>(&mut self, event: Event<'a>) -> Result<Event<'a>> {
        if self.edge_label_depth == 0 {
            if let Event::Start(e) = &event {
                if has_class(e, "edgeLabel")? {
                    self.edge_label_depth = 1;
                    self.label_group_dx = None;
                }
            }
            return Ok(event);
        }

        // The direct child of `<g class="edgeLabel">` is the `<g
        // class="label">` wrapper whose offset `recenter_background_rect`
        // needs; remember it before that rect (several levels further down)
        // is reached.
        if self.edge_label_depth == 1 {
            if let Event::Start(e) = &event {
                if has_class(e, "label")? {
                    self.label_group_dx = Some(transform_translate_dx(e)?);
                }
            }
        }

        let mut next_background_center_x = None;
        let event = match &event {
            Event::Start(e) | Event::Empty(e) if e.name().as_ref() == b"rect" => {
                match self.recenter_background_rect(e)? {
                    Some((replacement, center)) => {
                        next_background_center_x = Some(center);
                        match replacement {
                            Some(new_elem) => match event {
                                Event::Start(_) => Event::Start(new_elem),
                                _ => Event::Empty(new_elem),
                            },
                            None => event,
                        }
                    }
                    None => event,
                }
            }
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
            Event::Start(_) => {
                self.edge_label_depth += 1;
                self.background_center_x = next_background_center_x;
            }
            Event::Empty(_) => {
                self.background_center_x = next_background_center_x;
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
        label_group_dx: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MermaidTheme;
    use quick_xml::Reader;
    use quick_xml::Writer;

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

    /// Runs only this module's pass over `svg` (as opposed to the full
    /// [`crate::render_to_svg`] pipeline), for fixtures that target this
    /// pass's logic directly rather than a diagram source string.
    fn apply(svg: &str) -> String {
        let mut reader = Reader::from_str(svg);
        reader.config_mut().check_end_names = false;
        let mut events = Vec::new();
        loop {
            match reader.read_event() {
                Ok(Event::Eof) => break,
                Ok(event) => events.push(Ok(event)),
                Err(err) => events.push(Err(err.into())),
            }
        }
        let mut writer = Writer::new(Vec::new());
        for event in process(events.into_iter()) {
            writer
                .write_event(event.expect("test fixture is well-formed XML"))
                .expect("writing back a test fixture cannot fail");
        }
        String::from_utf8(writer.into_inner()).expect("test fixture is valid UTF-8")
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
        /// Absolute x of the `<g class="edgeLabel">` itself: the point on the
        /// edge this label annotates.
        anchor_x: Option<f64>,
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
                labels.push(EdgeLabel {
                    anchor_x: Some(x),
                    ..EdgeLabel::default()
                });
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
        (
            "erDiagram, English",
            "erDiagram\n    CUSTOMER ||--o{ ORDER : places",
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

    /// A label's background box must also be centered on the point of the
    /// edge it annotates (the `<g class="edgeLabel">` anchor), not merely on
    /// its own text. ER relationship labels emit a background rect already
    /// centered on its own local origin, but the `<g class="label">` wrapper
    /// around it re-centers *that* box again, so without this pass the whole
    /// label (box and text moving together) lands half its width away from
    /// the edge, even though the text sits correctly on the box.
    #[test]
    fn edge_label_backgrounds_are_centered_on_their_anchor() {
        for (name, source) in DIAGRAMS {
            let svg = crate::render_to_svg(source, &theme())
                .unwrap_or_else(|err| panic!("{name} failed to render: {err:#}"));
            let (_, labels) = collect(&svg);
            assert!(
                labels.iter().any(|label| label.background.is_some()),
                "{name} rendered no edge label backgrounds, so this case tests nothing"
            );

            for label in &labels {
                let (Some(background), Some(anchor_x)) = (label.background, label.anchor_x) else {
                    continue;
                };
                assert!(
                    (background.center_x() - anchor_x).abs() < 3.0,
                    "{name}: label {:?} background centers at {} but the edge it \
                     annotates passes through x = {anchor_x}",
                    label.text,
                    background.center_x(),
                );
            }
        }
    }

    /// Labels merman already centers must come through untouched: state
    /// diagram labels arrive as foreignObject fallbacks whose text carries an
    /// absolute `x`, unrelated to the `<g class="label">`/background-rect
    /// structure this pass otherwise looks at.
    #[test]
    fn already_centered_labels_are_left_alone() {
        let source = "stateDiagram-v2\n    [*] --> 待機中\n    待機中 --> 実行中: 開始要求を受理";
        let svg = crate::render_to_svg(source, &theme())
            .unwrap_or_else(|err| panic!("{source} failed to render: {err:#}"));
        assert!(
            !svg.contains(r#"<text transform="translate("#),
            "an already-centered label was shifted: {svg}"
        );
    }

    /// Re-running this pass over its own output (simulating a Zed
    /// postprocess that ran twice, or a second look at an already-fixed SVG)
    /// must not move anything further: a `<text>` or background `<rect>` this
    /// pass already shifted carries its own `transform`, which is what a
    /// later pass checks before shifting it again.
    #[test]
    fn is_idempotent_on_its_own_output() {
        for (name, source) in DIAGRAMS {
            let svg = crate::render_to_svg(source, &theme())
                .unwrap_or_else(|err| panic!("{name} failed to render: {err:#}"));
            let twice = apply(&svg);
            assert_eq!(
                svg, twice,
                "{name}: a second pass over already-processed output moved something"
            );
        }
    }

    /// A background rect centered on zero is only the ER double-shift bug
    /// (see the module doc) when its `<g class="label">` wrapper is *also*
    /// offset by minus half its width. A wrapper offset of zero looks like a
    /// hypothetical future merman that stopped applying that redundant
    /// offset itself; such a label must be left exactly as found, not
    /// shifted a further half-width by this pass mistaking it for the bug.
    #[test]
    fn leaves_an_already_correctly_centered_er_style_label_alone() {
        let svg = concat!(
            r#"<svg><g class="edgeLabels">"#,
            r#"<g class="edgeLabel" transform="translate(58, 140.5)">"#,
            r#"<g class="label" transform="translate(0, -8.5)">"#,
            r#"<g><rect class="background" x="-28.0478515625" y="-1" width="56.095703125" height="17"/>"#,
            r#"<text y="-10.1" text-anchor="middle"><tspan x="0">発注する</tspan></text></g>"#,
            r#"</g></g></g></svg>"#,
        );
        assert_eq!(
            apply(svg),
            svg,
            "a label whose wrapper no longer double-shifts must not be moved"
        );
    }

    /// A background rect that already carries a `transform` of its own is left
    /// where merman put it. Shifting it would mean pushing a second
    /// `transform` attribute onto the same element, which is malformed XML,
    /// and a renderer that keeps only one of the two would drop either
    /// merman's placement or this pass's correction.
    #[test]
    fn a_background_rect_with_its_own_transform_is_left_alone() {
        let svg = concat!(
            r#"<svg><g class="edgeLabels">"#,
            r#"<g class="edgeLabel" transform="translate(58, 140.5)">"#,
            r#"<g class="label" transform="translate(-28.0478515625, -8.5)">"#,
            r#"<g><rect class="background" transform="translate(0,2)" x="-28.0478515625" y="-1" width="56.095703125" height="17"/>"#,
            r#"<text y="-10.1" text-anchor="middle"><tspan x="0">発注する</tspan></text></g>"#,
            r#"</g></g></g></svg>"#,
        );
        let out = apply(svg);
        assert_eq!(
            out, svg,
            "a rect merman already transformed must keep exactly one transform"
        );
    }
}
