//! Collects a coordinate map of a diagram's nodes and edge labels, so that a
//! rendered diagram can be hit-tested back to the mermaid source.
//!
//! merman keeps the diagram's structure in the SVG it emits, in a spelling that
//! differs per diagram kind because each kind has its own renderer:
//!
//! * flowchart: `<g class="node …" id="{diagram_id}-flowchart-{node id}-{n}">`
//!   wrapping a shape that carries the `label-container` class, and
//!   `<path data-id="L_{from}_{to}_{n}">` per edge, whose label (when it has
//!   one) is a `<g class="label" data-id="L_{from}_{to}_{n}">` holding a
//!   `<rect class="background">`.
//! * ER: `<g class="node …" id="{diagram_id}-entity-{name}-{n}">` around a
//!   `label-container` (an entity with no attributes) or an `outer-path` group
//!   (one with attributes), and
//!   `<path data-id="id_{from element id}_{to element id}_{n}">` per
//!   relationship, whose label is spelled like a flowchart's.
//! * state: `<g class="node statediagram-state" id="{diagram_id}-state-{state id}-{n}">`
//!   (`merman` 0.7 left the diagram id out),
//!   and `<path data-id="edge{n}">` per transition, where `{n}` numbers the
//!   diagram's transitions in the order they were written. A composite state
//!   nests a whole `<g class="root">` — its own sections, under one more
//!   translate — inside the enclosing `<g class="nodes">`.
//! * class: `<g class="node …" id="{diagram_id}-classId-{class id}-{n}">`
//!   around a `label-container`, whose name sits in a `label-group` beside the
//!   annotation, member and method groups, and
//!   `<path data-id="id_{from}_{to}_{n}">` per relation, with its label spelled
//!   like a flowchart's. A note is a node too, under an id of its own shape,
//!   and is not mapped.
//! * sequence: laid out in a shape of its own, read by [`sequence`].
//!
//! State and ER labels are drawn through `<foreignObject>`, which merman's
//! resvg-safe pipeline replaces with a `<text>` (and, for an edge label, a
//! background `<rect>`) appended at the end of the document, carrying absolute
//! coordinates and the ancestors' classes but no id. Those are matched back
//! onto the boxes collected above by position, and only when exactly one box
//! claims them.
//!
//! This pass walks the event stream and turns all of that into a
//! [`DiagramGeometry`] in `viewBox` coordinates. It runs after
//! [`super::center_edge_labels`] so that the edge label boxes it reports are
//! the corrected ones, and before the passes that only rewrite styles and
//! classes, which never move anything.
//!
//! Pie charts, gantt charts and ER attribute cells are read by
//! [`chart_labels`] and reported as labels rather than nodes.
//!
//! Every other diagram kind comes back empty rather than guessed at, since
//! neither the node ids nor the source syntax of the other kinds line up with
//! what the editing UI needs.

mod chart_labels;

use std::cell::RefCell;
use std::rc::Rc;

use anyhow::Result;
use quick_xml::XmlVersion;
use quick_xml::events::{BytesStart, Event};

use crate::{DiagramGeometry, EdgeGeometry, NodeGeometry};

mod sequence;

/// An axis-aligned bounding box accumulated from the points of one or more
/// shapes.
#[derive(Clone, Copy, Debug)]
struct BoundingBox {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

impl BoundingBox {
    fn point(x: f64, y: f64) -> Self {
        Self {
            min_x: x,
            min_y: y,
            max_x: x,
            max_y: y,
        }
    }

    fn extend(&mut self, x: f64, y: f64) {
        self.min_x = self.min_x.min(x);
        self.min_y = self.min_y.min(y);
        self.max_x = self.max_x.max(x);
        self.max_y = self.max_y.max(y);
    }

    fn union(&mut self, other: BoundingBox) {
        self.extend(other.min_x, other.min_y);
        self.extend(other.max_x, other.max_y);
    }

    fn translated(self, dx: f64, dy: f64) -> Self {
        Self {
            min_x: self.min_x + dx,
            min_y: self.min_y + dy,
            max_x: self.max_x + dx,
            max_y: self.max_y + dy,
        }
    }

    fn padded(self, padding: f64) -> Self {
        Self {
            min_x: self.min_x - padding,
            min_y: self.min_y - padding,
            max_x: self.max_x + padding,
            max_y: self.max_y + padding,
        }
    }

    fn to_rect(self) -> [f32; 4] {
        [
            self.min_x as f32,
            self.min_y as f32,
            (self.max_x - self.min_x) as f32,
            (self.max_y - self.min_y) as f32,
        ]
    }

    fn is_usable(&self) -> bool {
        self.min_x.is_finite()
            && self.min_y.is_finite()
            && self.max_x > self.min_x
            && self.max_y > self.min_y
    }
}

fn extend_optional(slot: &mut Option<BoundingBox>, other: BoundingBox) {
    match slot {
        Some(existing) => existing.union(other),
        None => *slot = Some(other),
    }
}

/// The accumulated `translate()` in effect for an element's children, and
/// whether that accumulation is still faithful.
#[derive(Clone, Copy)]
struct Frame {
    dx: f64,
    dy: f64,
    /// Cleared once an ancestor carries a transform this pass cannot fold into
    /// a translation (a rotation, scale or matrix). Geometry below such an
    /// element is dropped rather than reported at the wrong place.
    usable: bool,
}

/// The diagram kinds whose SVG this pass can read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DiagramKind {
    Flowchart,
    Er,
    State,
    Pie,
    Gantt,
    Class,
    Sequence,
}

/// The section of the diagram currently being walked. merman groups a
/// diagram into `<g class="nodes">`, `<g class="edgePaths">` and
/// `<g class="edgeLabels">`; anything outside them (clusters, markers, defs)
/// is not mapped. A composite state nests another set of them inside
/// `<g class="nodes">`, so the open sections are kept as a stack.
#[derive(Clone, Copy, PartialEq)]
enum Section {
    Nodes,
    EdgePaths,
    EdgeLabels,
}

struct PendingNode {
    node_id: String,
    /// The element's own id with the diagram's prefix removed
    /// (`entity-顧客-0`), which is how an ER relationship names its ends.
    element_suffix: String,
    start_depth: usize,
    rows: Vec<String>,
    bounds: Option<BoundingBox>,
    /// The depth of the innermost open element that carries the
    /// `label-container` class, if any; shapes below it make up the node's box.
    container_depth: Option<usize>,
    /// The depth of an open group whose text is not the node's label (a
    /// class's annotation, members and methods), if any.
    muted_depth: Option<usize>,
}

struct PendingEdgeLabel {
    edge_id: String,
    start_depth: usize,
    rows: Vec<String>,
    bounds: Option<BoundingBox>,
    /// Where the label group itself is anchored, which is the point the
    /// replacement for a `<foreignObject>` label is drawn at.
    anchor: (f64, f64),
}

/// What a `<text>` that replaced a `<foreignObject>` label belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FallbackRole {
    Node,
    Edge,
    /// An ER attribute's name cell.
    AttributeName,
}

/// A label merman drew outside the element it belongs to, to be matched back
/// onto that element by position.
struct PendingFallback {
    role: FallbackRole,
    start_depth: usize,
    /// The label's lines, one per `<text>`, joined with line breaks.
    text: String,
    /// Where the label is centered: midway between its first line's anchor
    /// and its last's, since a wrapped label is drawn as one `<text>` per line
    /// around the point merman laid the label out at.
    anchor: Option<(f64, f64)>,
    first_anchor: Option<(f64, f64)>,
    bounds: Option<BoundingBox>,
    font_size: Option<f64>,
}

struct CollectGeometry<I> {
    inner: I,
    sink: Rc<RefCell<DiagramGeometry>>,
    /// The `id` of the root `<svg>`, which prefixes the element ids of every
    /// kind but the state diagram.
    svg_id: String,
    kind: Option<DiagramKind>,
    view_box: [f32; 4],
    depth: usize,
    frames: Vec<Frame>,
    sections: Vec<(Section, usize)>,
    node: Option<PendingNode>,
    edge_label: Option<PendingEdgeLabel>,
    fallback: Option<PendingFallback>,
    nodes: Vec<NodeGeometry>,
    /// The element id suffix of each collected node, positionally matching
    /// `nodes`.
    node_suffixes: Vec<String>,
    edge_ids: Vec<String>,
    edge_labels: Vec<EdgeLabel>,
    fallbacks: Vec<PendingFallback>,
    chart: chart_labels::ChartLabels,
    sequence: sequence::SequenceGeometry,
    finished: bool,
}

struct EdgeLabel {
    edge_id: String,
    text: Option<String>,
    rect: Option<[f32; 4]>,
    anchor: (f64, f64),
}

/// An attribute's value, read for measurement only.
///
/// Nothing here may fail: the diagram has to render whether or not this pass
/// can make sense of it, so an attribute list quick-xml refuses to parse reads
/// as absent, and a value whose entities do not decode (a stray `&`, which the
/// passes that need the text handle themselves) is measured raw.
fn attribute(element: &BytesStart<'_>, name: &str) -> Option<String> {
    let attr = element.try_get_attribute(name).ok()??;
    Some(match attr.normalized_value(XmlVersion::Implicit1_0) {
        Ok(value) => value.into_owned(),
        Err(_) => String::from_utf8_lossy(attr.value.as_ref()).into_owned(),
    })
}

fn attribute_f64(element: &BytesStart<'_>, name: &str) -> Option<f64> {
    attribute(element, name).and_then(|value| value.trim().parse::<f64>().ok())
}

fn has_class(element: &BytesStart<'_>, class: &str) -> bool {
    attribute(element, "class")
        .is_some_and(|value| value.split_ascii_whitespace().any(|token| token == class))
}

/// The element's own `transform`, folded into a translation.
///
/// Returns `None` when the transform contains anything other than
/// `translate()`, which this pass cannot represent.
fn own_translate(element: &BytesStart<'_>) -> Option<(f64, f64)> {
    let Some(value) = attribute(element, "transform") else {
        return Some((0.0, 0.0));
    };
    let mut remainder = value.as_str();
    let mut dx = 0.0;
    let mut dy = 0.0;
    let mut leftover = String::new();
    while let Some(start) = remainder.find("translate(") {
        leftover.push_str(&remainder[..start]);
        let after = &remainder[start + "translate(".len()..];
        let end = after.find(')')?;
        let mut arguments = after[..end]
            .split([',', ' '])
            .map(str::trim)
            .filter(|part| !part.is_empty());
        let x = arguments.next().and_then(|part| part.parse::<f64>().ok())?;
        let y = match arguments.next() {
            Some(part) => part.parse::<f64>().ok()?,
            None => 0.0,
        };
        dx += x;
        dy += y;
        remainder = &after[end + 1..];
    }
    leftover.push_str(remainder);
    if !leftover.trim().is_empty() {
        return None;
    }
    Some((dx, dy))
}

/// The bounding box of a shape element in its own coordinate system, with its
/// own `transform` already applied.
fn shape_bounds(element: &BytesStart<'_>) -> Option<BoundingBox> {
    let bounds = match element.name().as_ref() {
        b"rect" => {
            let x = attribute_f64(element, "x").unwrap_or(0.0);
            let y = attribute_f64(element, "y").unwrap_or(0.0);
            let (Some(width), Some(height)) = (
                attribute_f64(element, "width"),
                attribute_f64(element, "height"),
            ) else {
                return None;
            };
            let mut bounds = BoundingBox::point(x, y);
            bounds.extend(x + width, y + height);
            Some(bounds)
        }
        b"circle" => {
            let cx = attribute_f64(element, "cx").unwrap_or(0.0);
            let cy = attribute_f64(element, "cy").unwrap_or(0.0);
            let Some(r) = attribute_f64(element, "r") else {
                return None;
            };
            let mut bounds = BoundingBox::point(cx - r, cy - r);
            bounds.extend(cx + r, cy + r);
            Some(bounds)
        }
        b"ellipse" => {
            let cx = attribute_f64(element, "cx").unwrap_or(0.0);
            let cy = attribute_f64(element, "cy").unwrap_or(0.0);
            let (Some(rx), Some(ry)) = (attribute_f64(element, "rx"), attribute_f64(element, "ry"))
            else {
                return None;
            };
            let mut bounds = BoundingBox::point(cx - rx, cy - ry);
            bounds.extend(cx + rx, cy + ry);
            Some(bounds)
        }
        b"line" => {
            let x1 = attribute_f64(element, "x1").unwrap_or(0.0);
            let y1 = attribute_f64(element, "y1").unwrap_or(0.0);
            let x2 = attribute_f64(element, "x2").unwrap_or(0.0);
            let y2 = attribute_f64(element, "y2").unwrap_or(0.0);
            let mut bounds = BoundingBox::point(x1, y1);
            bounds.extend(x2, y2);
            Some(bounds)
        }
        b"polygon" | b"polyline" => {
            let Some(points) = attribute(element, "points") else {
                return None;
            };
            let mut numbers = points
                .split([',', ' ', '\n', '\t'])
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .filter_map(|part| part.parse::<f64>().ok());
            let (Some(x), Some(y)) = (numbers.next(), numbers.next()) else {
                return None;
            };
            let mut bounds = BoundingBox::point(x, y);
            while let (Some(x), Some(y)) = (numbers.next(), numbers.next()) {
                bounds.extend(x, y);
            }
            Some(bounds)
        }
        b"path" => {
            let data = attribute(element, "d")?;
            path_bounds(&data)
        }
        _ => None,
    };
    let (dx, dy) = own_translate(element)?;
    Some(bounds?.translated(dx, dy))
}

/// The bounding box of a path's control points.
///
/// Curve control points lie outside the curve itself, so this is an
/// over-approximation in general. merman draws its rounded shapes as long runs
/// of short cubic segments whose control points sit essentially on the outline,
/// so the result is within a fraction of a pixel of the true outline — close
/// enough to hit-test a node with.
fn path_bounds(data: &str) -> Option<BoundingBox> {
    let mut numbers = PathNumbers::new(data);
    let mut bounds: Option<BoundingBox> = None;
    let mut current = (0.0_f64, 0.0_f64);
    let mut subpath_start = current;
    let mut command = None;

    loop {
        match numbers.next_token()? {
            PathToken::End => break,
            PathToken::Command(letter) => {
                if letter.eq_ignore_ascii_case(&'z') {
                    current = subpath_start;
                    command = None;
                } else {
                    command = Some(letter);
                }
                continue;
            }
            PathToken::Number(first) => {
                let Some(letter) = command else {
                    return None;
                };
                let relative = letter.is_ascii_lowercase();
                let base = if relative { current } else { (0.0, 0.0) };
                // The coordinate pairs a command consumes after its first
                // number, and whether it also has leading non-coordinate
                // arguments (the arc's radii, rotation and flags).
                let (pairs_after_first, arc): (i32, bool) = match letter.to_ascii_lowercase() {
                    'm' | 'l' | 't' => (0, false),
                    'h' | 'v' => (-1, false),
                    'c' => (2, false),
                    's' | 'q' => (1, false),
                    'a' => (0, true),
                    _ => return None,
                };

                if pairs_after_first < 0 {
                    // Horizontal or vertical line: one number, one axis.
                    let horizontal = letter.eq_ignore_ascii_case(&'h');
                    let value = if relative {
                        if horizontal {
                            current.0 + first
                        } else {
                            current.1 + first
                        }
                    } else {
                        first
                    };
                    if horizontal {
                        current.0 = value;
                    } else {
                        current.1 = value;
                    }
                } else if arc {
                    // rx ry rotation large-arc sweep x y — only the endpoint
                    // is a coordinate, and the arc itself can bulge past it.
                    let _ry = numbers.next_number()?;
                    let _rotation = numbers.next_number()?;
                    let _large_arc = numbers.next_number()?;
                    let _sweep = numbers.next_number()?;
                    let x = numbers.next_number()?;
                    let y = numbers.next_number()?;
                    current = (base.0 + x, base.1 + y);
                } else {
                    let y = numbers.next_number()?;
                    let mut point = (base.0 + first, base.1 + y);
                    extend_optional(&mut bounds, BoundingBox::point(point.0, point.1));
                    for _ in 0..pairs_after_first {
                        let x = numbers.next_number()?;
                        let y = numbers.next_number()?;
                        point = (base.0 + x, base.1 + y);
                        extend_optional(&mut bounds, BoundingBox::point(point.0, point.1));
                    }
                    current = point;
                    if letter.eq_ignore_ascii_case(&'m') {
                        subpath_start = current;
                        // A moveto's repeated pairs are implicit linetos.
                        command = Some(if relative { 'l' } else { 'L' });
                    }
                }

                extend_optional(&mut bounds, BoundingBox::point(current.0, current.1));
            }
        }
    }

    bounds.filter(|bounds| bounds.min_x.is_finite() && bounds.min_y.is_finite())
}

enum PathToken {
    Command(char),
    Number(f64),
    End,
}

/// A minimal tokenizer for the `d` attribute: command letters and numbers,
/// with separators (whitespace and commas) skipped.
struct PathNumbers<'a> {
    data: &'a str,
    position: usize,
}

impl<'a> PathNumbers<'a> {
    fn new(data: &'a str) -> Self {
        Self { data, position: 0 }
    }

    fn skip_separators(&mut self) {
        while let Some(character) = self.data[self.position..].chars().next() {
            if character.is_ascii_whitespace() || character == ',' {
                self.position += character.len_utf8();
            } else {
                break;
            }
        }
    }

    /// `None` signals malformed input, which makes the whole path unusable.
    fn next_token(&mut self) -> Option<PathToken> {
        self.skip_separators();
        let rest = &self.data[self.position..];
        let Some(character) = rest.chars().next() else {
            return Some(PathToken::End);
        };
        if character.is_ascii_alphabetic() {
            self.position += character.len_utf8();
            return Some(PathToken::Command(character));
        }
        let mut end = 0;
        let bytes = rest.as_bytes();
        while end < bytes.len() {
            let byte = bytes[end];
            let is_sign = (byte == b'+' || byte == b'-')
                && (end == 0 || bytes[end - 1] == b'e' || bytes[end - 1] == b'E');
            if byte.is_ascii_digit() || byte == b'.' || is_sign || byte == b'e' || byte == b'E' {
                end += 1;
            } else {
                break;
            }
        }
        let number = rest[..end].parse::<f64>().ok()?;
        self.position += end;
        Some(PathToken::Number(number))
    }

    fn next_number(&mut self) -> Option<f64> {
        match self.next_token()? {
            PathToken::Number(number) => Some(number),
            _ => None,
        }
    }
}

impl<I> CollectGeometry<I> {
    fn frame(&self) -> Frame {
        self.frames.last().copied().unwrap_or(Frame {
            dx: 0.0,
            dy: 0.0,
            usable: true,
        })
    }

    /// Strips the prefix and the `-{n}` suffix merman wraps around a node's own
    /// id. The prefix is the diagram's own id followed by the kind's word; a
    /// state diagram under `merman` 0.7 left the diagram id out.
    fn plain_node_id(&self, element_id: &str, kind: DiagramKind) -> Option<String> {
        let infix = match kind {
            DiagramKind::Flowchart => "-flowchart-",
            DiagramKind::Er => "-entity-",
            DiagramKind::State => {
                let prefixed = format!("{}-state-", self.svg_id);
                let rest = element_id
                    .strip_prefix(prefixed.as_str())
                    .or_else(|| element_id.strip_prefix("state-"))?;
                return strip_index(rest);
            }
            DiagramKind::Pie | DiagramKind::Gantt => return None,
            DiagramKind::Class => "-classId-",
            DiagramKind::Sequence => return None,
        };
        let prefixed = format!("{}{infix}", self.svg_id);
        let rest = element_id.strip_prefix(prefixed.as_str()).or_else(|| {
            element_id
                .find(infix)
                .map(|index| &element_id[index + infix.len()..])
        })?;
        strip_index(rest)
    }

    /// The element's id with the diagram's own id and the separating `-`
    /// removed, which is how an ER relationship's `data-id` names its ends.
    fn element_suffix(&self, element_id: &str) -> String {
        element_id
            .strip_prefix(self.svg_id.as_str())
            .and_then(|rest| rest.strip_prefix('-'))
            .unwrap_or(element_id)
            .to_string()
    }

    fn on_start(&mut self, element: &BytesStart<'_>, depth: usize, empty: bool, frame: Frame) {
        let Some(kind) = self.kind else {
            if element.name().as_ref() == b"svg" {
                self.read_svg_root(element);
            }
            return;
        };
        if matches!(kind, DiagramKind::Pie | DiagramKind::Gantt) {
            self.on_chart_element(element, depth, empty, frame, kind);
            return;
        }
        if kind == DiagramKind::Sequence {
            self.sequence.on_start(element, depth, empty, frame);
            return;
        }

        // The fallback labels sit outside the diagram's sections, and carry the
        // classes of the element they were lifted out of, so they have to be
        // recognized before anything else would read those classes as a section
        // or a node of its own.
        if self.fallback.is_some() {
            self.on_fallback_element(element, frame);
            return;
        }
        if has_class(element, "merman-foreignobject-fallback") {
            if let Some(role) = fallback_role(element) {
                self.fallback = Some(PendingFallback {
                    role,
                    start_depth: depth,
                    text: String::new(),
                    anchor: None,
                    first_anchor: None,
                    bounds: None,
                    font_size: None,
                });
            }
            return;
        }

        // A nested `<g class="root">` opens its own sections inside the
        // enclosing one, so a section is looked for at every element rather
        // than only while none is open.
        let section = if has_class(element, "nodes") {
            Some(Section::Nodes)
        } else if has_class(element, "edgePaths") {
            Some(Section::EdgePaths)
        } else if has_class(element, "edgeLabels") {
            Some(Section::EdgeLabels)
        } else {
            None
        };
        if let Some(section) = section
            && !empty
        {
            self.sections.push((section, depth));
            return;
        }

        let Some((section, _)) = self.sections.last().copied() else {
            return;
        };

        match section {
            Section::Nodes => self.on_node_element(element, depth, empty, frame, kind),
            Section::EdgePaths => {
                if element.name().as_ref() == b"path"
                    && let Some(edge_id) = self.edge_id_of(element, kind)
                {
                    self.edge_ids.push(edge_id);
                }
            }
            Section::EdgeLabels => self.on_edge_label_element(element, depth, frame),
        }
    }

    fn read_svg_root(&mut self, element: &BytesStart<'_>) {
        let role = attribute(element, "aria-roledescription").unwrap_or_default();
        let class = attribute(element, "class").unwrap_or_default();
        let has_class_token =
            |wanted: &str| class.split_ascii_whitespace().any(|token| token == wanted);
        let kind = if role.starts_with("flowchart") || has_class_token("flowchart") {
            DiagramKind::Flowchart
        } else if role == "er" || has_class_token("erDiagram") {
            DiagramKind::Er
        } else if role.starts_with("stateDiagram") || has_class_token("statediagram") {
            DiagramKind::State
        } else if role == "pie" {
            DiagramKind::Pie
        } else if role == "gantt" {
            DiagramKind::Gantt
        } else if role == "class" || has_class_token("classDiagram") {
            DiagramKind::Class
        } else if role == "sequence" {
            DiagramKind::Sequence
        } else {
            return;
        };
        let Some(view_box) = attribute(element, "viewBox") else {
            return;
        };
        let values: Vec<f32> = view_box
            .split([',', ' ', '\n', '\t'])
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .filter_map(|part| part.parse::<f32>().ok())
            .collect();
        let [min_x, min_y, width, height] = values[..] else {
            return;
        };
        if !(width > 0.0 && height > 0.0) {
            return;
        }
        self.view_box = [min_x, min_y, width, height];
        self.kind = Some(kind);
    }

    fn edge_id_of(&self, element: &BytesStart<'_>, kind: DiagramKind) -> Option<String> {
        let id = match attribute(element, "data-id") {
            Some(data_id) => data_id,
            None => {
                let element_id = attribute(element, "id")?;
                element_id
                    .find("-L_")
                    .map(|index| element_id[index + 1..].to_string())?
            }
        };
        // A state diagram draws a note as an edge too, with an id of its own
        // shape; only the numbered transitions are mapped.
        if kind == DiagramKind::State && transition_index(&id).is_none() {
            return None;
        }
        // A class diagram draws the line to a note as an edge as well.
        if kind == DiagramKind::Class && !id.starts_with("id_") {
            return None;
        }
        Some(id)
    }

    fn on_fallback_element(&mut self, element: &BytesStart<'_>, frame: Frame) {
        let is_text = element.name().as_ref() == b"text";
        let is_rect = element.name().as_ref() == b"rect";
        if !is_text && !is_rect {
            return;
        }
        let bounds = frame.usable.then(|| shape_bounds(element)).flatten();
        let anchor = (
            attribute_f64(element, "x").map(|x| x + frame.dx),
            attribute_f64(element, "y").map(|y| y + frame.dy),
        );
        let font_size = is_text
            .then(|| chart_labels::font_size_of(element))
            .flatten();
        let Some(fallback) = self.fallback.as_mut() else {
            return;
        };
        if is_text {
            if !fallback.text.is_empty() {
                fallback.text.push('\n');
            }
            if let (Some(x), Some(y)) = anchor {
                let (first_x, first_y) = *fallback.first_anchor.get_or_insert((x, y));
                fallback.anchor = Some(((first_x + x) / 2.0, (first_y + y) / 2.0));
            }
            if font_size.is_some() {
                fallback.font_size = font_size;
            }
        } else if let Some(bounds) = bounds {
            extend_optional(&mut fallback.bounds, bounds.translated(frame.dx, frame.dy));
        }
    }

    fn on_node_element(
        &mut self,
        element: &BytesStart<'_>,
        depth: usize,
        empty: bool,
        frame: Frame,
        kind: DiagramKind,
    ) {
        if self.node.is_none() {
            if !has_class(element, "node") {
                return;
            }
            // A state diagram draws start and end markers, notes, forks and
            // choices as nodes as well. Only a `statediagram-state` names a
            // state the source declares, and only it has a label to edit.
            if kind == DiagramKind::State && !has_class(element, "statediagram-state") {
                return;
            }
            let Some(element_id) = attribute(element, "id") else {
                return;
            };
            let Some(node_id) = self.plain_node_id(&element_id, kind) else {
                return;
            };
            self.node = Some(PendingNode {
                node_id,
                element_suffix: self.element_suffix(&element_id),
                start_depth: depth,
                rows: Vec::new(),
                bounds: None,
                container_depth: None,
                muted_depth: None,
            });
            return;
        }

        if kind == DiagramKind::Er && self.on_er_row_element(element, depth, empty, frame) {
            return;
        }

        // An ER entity with attributes and a state drawn with a title bar put
        // their outline in a group of `<path>`s or a bare `<rect>` instead of
        // the `label-container` a flowchart node uses.
        let is_container = has_class(element, "label-container")
            || (kind != DiagramKind::Flowchart
                && (has_class(element, "outer-path") || has_class(element, "outer")));
        let is_row = element.name().as_ref() == b"tspan" && has_class(element, "text-outer-tspan");
        let bounds = (frame.usable && (is_container || self.has_open_container()))
            .then(|| shape_bounds(element))
            .flatten();
        let mutes = kind == DiagramKind::Class
            && !empty
            && (has_class(element, "annotation-group")
                || has_class(element, "members-group")
                || has_class(element, "methods-group"));
        let Some(node) = self.node.as_mut() else {
            return;
        };
        if mutes && node.muted_depth.is_none() {
            node.muted_depth = Some(depth);
        }
        if is_row && node.muted_depth.is_none() {
            node.rows.push(String::new());
        }
        if is_container || node.container_depth.is_some() {
            if let Some(bounds) = bounds {
                extend_optional(&mut node.bounds, bounds.translated(frame.dx, frame.dy));
            }
            if is_container && !empty && node.container_depth.is_none() {
                node.container_depth = Some(depth);
            }
        }
    }

    fn has_open_container(&self) -> bool {
        self.node
            .as_ref()
            .is_some_and(|node| node.container_depth.is_some())
    }

    fn on_edge_label_element(&mut self, element: &BytesStart<'_>, depth: usize, frame: Frame) {
        if self.edge_label.is_none() {
            if !has_class(element, "label") {
                return;
            }
            let Some(edge_id) = attribute(element, "data-id") else {
                return;
            };
            self.edge_label = Some(PendingEdgeLabel {
                edge_id,
                start_depth: depth,
                rows: Vec::new(),
                bounds: None,
                // The enclosing `<g class="edgeLabel">` is translated to where
                // the label is drawn, which is the point a label lifted into a
                // `<foreignObject>` replacement is written at.
                anchor: (frame.dx, frame.dy),
            });
            return;
        }

        let is_background = element.name().as_ref() == b"rect" && has_class(element, "background");
        let is_row = element.name().as_ref() == b"tspan" && has_class(element, "text-outer-tspan");
        let bounds = (is_background && frame.usable)
            .then(|| shape_bounds(element))
            .flatten();
        let Some(edge_label) = self.edge_label.as_mut() else {
            return;
        };
        if is_row {
            edge_label.rows.push(String::new());
        }
        if let Some(bounds) = bounds {
            extend_optional(
                &mut edge_label.bounds,
                bounds.translated(frame.dx, frame.dy),
            );
        }
    }

    fn on_end(&mut self, depth: usize) {
        if self.kind == Some(DiagramKind::Sequence) {
            self.sequence.on_end(depth);
            return;
        }
        self.on_chart_end(depth);
        self.on_er_row_end(depth);
        if let Some(node) = self.node.as_mut()
            && node.container_depth == Some(depth)
        {
            node.container_depth = None;
        }
        if let Some(node) = self.node.as_mut()
            && node.muted_depth == Some(depth)
        {
            node.muted_depth = None;
        }
        if self
            .node
            .as_ref()
            .is_some_and(|node| node.start_depth == depth)
            && let Some(node) = self.node.take()
        {
            let mut mapped = None;
            if let Some(bounds) = node.bounds.filter(BoundingBox::is_usable) {
                mapped = Some(node.node_id.clone());
                self.nodes.push(NodeGeometry {
                    node_id: node.node_id,
                    label: join_rows(node.rows).unwrap_or_default(),
                    rect: bounds.to_rect(),
                });
                self.node_suffixes.push(node.element_suffix);
            }
            self.on_er_entity_end(mapped);
        }
        if self
            .edge_label
            .as_ref()
            .is_some_and(|edge_label| edge_label.start_depth == depth)
            && let Some(edge_label) = self.edge_label.take()
        {
            self.edge_labels.push(EdgeLabel {
                edge_id: edge_label.edge_id,
                text: join_rows(edge_label.rows),
                rect: edge_label
                    .bounds
                    .filter(BoundingBox::is_usable)
                    .map(BoundingBox::to_rect),
                anchor: edge_label.anchor,
            });
        }
        if self
            .fallback
            .as_ref()
            .is_some_and(|fallback| fallback.start_depth == depth)
            && let Some(fallback) = self.fallback.take()
        {
            self.fallbacks.push(fallback);
        }
        if let Some((_, section_depth)) = self.sections.last().copied()
            && section_depth == depth
        {
            self.sections.pop();
        }
    }

    fn on_text(&mut self, text: &str) {
        if self.kind == Some(DiagramKind::Sequence) {
            self.sequence.on_text(text);
            return;
        }
        if self.chart.has_open_text() {
            self.on_chart_text(text);
            return;
        }
        if self
            .node
            .as_ref()
            .is_some_and(|node| node.muted_depth.is_some())
        {
            return;
        }
        if let Some(fallback) = self.fallback.as_mut() {
            fallback.text.push_str(text);
            return;
        }
        let target = match (self.node.as_mut(), self.edge_label.as_mut()) {
            (Some(node), _) => &mut node.rows,
            (None, Some(edge_label)) => &mut edge_label.rows,
            (None, None) => return,
        };
        match target.last_mut() {
            Some(row) => row.push_str(text),
            None => target.push(text.to_string()),
        }
    }

    fn finish(&mut self) {
        let kind = self.kind;
        if kind == Some(DiagramKind::Sequence) {
            let (nodes, edges) = std::mem::take(&mut self.sequence).finish();
            *self.sink.borrow_mut() = DiagramGeometry {
                view_box: self.view_box,
                nodes,
                edges,
                labels: Vec::new(),
            };
            return;
        }
        let mut nodes = std::mem::take(&mut self.nodes);
        let mut edge_labels = std::mem::take(&mut self.edge_labels);
        let attribute_names = self.apply_fallback_labels(&mut nodes, &mut edge_labels);
        let labels = self.finish_chart_labels(attribute_names);
        let node_ids: Vec<&str> = nodes.iter().map(|node| node.node_id.as_str()).collect();
        let mut seen_pairs: Vec<((String, String), usize)> = Vec::new();
        let edges = self
            .edge_ids
            .iter()
            .filter_map(|edge_id| {
                let (from, to, statement_index) = match kind {
                    // A state diagram's SVG does not name a transition's ends
                    // at all; its number is the position of the statement that
                    // wrote it, counting the whole document in order.
                    Some(DiagramKind::State) => {
                        (String::new(), String::new(), transition_index(edge_id))
                    }
                    Some(DiagramKind::Er) => {
                        let (from, to) = split_er_edge_id(edge_id, &self.node_suffixes, &node_ids)?;
                        (from, to, None)
                    }
                    // A class relation names its ends by their ids, like a
                    // flowchart edge, after an `id_` of its own.
                    Some(DiagramKind::Class) => {
                        let (from, to) = split_edge_id(
                            edge_id.strip_prefix("id_").unwrap_or(edge_id),
                            &node_ids,
                        );
                        (from, to, None)
                    }
                    _ => {
                        let (from, to) = split_edge_id(edge_id, &node_ids);
                        (from, to, None)
                    }
                };
                // merman's own `_{n}` suffix counts every edge in the diagram,
                // not just the ones between this pair, so it cannot be used to
                // find the matching statement in the source. Document order
                // within `<g class="edgePaths">` is the order the edges were
                // written in, so counting the pair here gives that position.
                let occurrence = match seen_pairs
                    .iter_mut()
                    .find(|(pair, _)| pair.0 == from && pair.1 == to)
                {
                    Some((_, count)) => {
                        *count += 1;
                        *count
                    }
                    None => {
                        seen_pairs.push(((from.clone(), to.clone()), 0));
                        0
                    }
                };
                let label = edge_labels.iter().find(|label| &label.edge_id == edge_id);
                Some(EdgeGeometry {
                    edge_id: edge_id.clone(),
                    from,
                    to,
                    occurrence,
                    statement_index,
                    label: label.and_then(|label| label.text.clone()),
                    label_rect: label.and_then(|label| label.rect),
                })
            })
            .collect();
        drop(node_ids);
        *self.sink.borrow_mut() = DiagramGeometry {
            view_box: self.view_box,
            nodes,
            edges,
            labels,
        };
    }

    /// Fills in the labels merman drew outside the element they belong to.
    ///
    /// A replacement carries no id, so it is claimed by position: a node label
    /// by the one node box that contains its anchor, an edge label by the one
    /// label group anchored at exactly that point. Anything claimed by no box,
    /// or by more than one, is dropped: the label is only ever shown, so a
    /// missing one costs a prefilled field while a wrong one would put another
    /// element's text in front of the user.
    ///
    /// An ER attribute's name is handed back instead, to be claimed by the
    /// attribute row it sits in.
    fn apply_fallback_labels(
        &mut self,
        nodes: &mut [NodeGeometry],
        edge_labels: &mut [EdgeLabel],
    ) -> Vec<PendingFallback> {
        const ANCHOR_TOLERANCE: f64 = 0.01;

        let mut attribute_names = Vec::new();
        for fallback in std::mem::take(&mut self.fallbacks) {
            if fallback.role == FallbackRole::AttributeName {
                attribute_names.push(fallback);
                continue;
            }
            let (Some((x, y)), text) = (fallback.anchor, fallback.text.trim()) else {
                continue;
            };
            if text.is_empty() {
                continue;
            }
            match fallback.role {
                FallbackRole::Node => {
                    let mut claimants = nodes.iter_mut().filter(|node| {
                        node.label.is_empty() && rect_contains(node.rect, x as f32, y as f32)
                    });
                    let Some(node) = claimants.next() else {
                        continue;
                    };
                    if claimants.next().is_some() {
                        continue;
                    }
                    node.label = text.to_string();
                }
                FallbackRole::Edge => {
                    let mut claimants = edge_labels.iter_mut().filter(|label| {
                        (label.anchor.0 - x).abs() < ANCHOR_TOLERANCE
                            && (label.anchor.1 - y).abs() < ANCHOR_TOLERANCE
                    });
                    let Some(label) = claimants.next() else {
                        continue;
                    };
                    if claimants.next().is_some() {
                        continue;
                    }
                    if label.text.is_none() {
                        label.text = Some(text.to_string());
                    }
                    if label.rect.is_none() {
                        label.rect = fallback
                            .bounds
                            .filter(BoundingBox::is_usable)
                            .map(BoundingBox::to_rect);
                    }
                }
                FallbackRole::AttributeName => {}
            }
        }
        attribute_names
    }
}

fn join_rows(rows: Vec<String>) -> Option<String> {
    let joined = rows.join("\n");
    (!joined.trim().is_empty()).then_some(joined)
}

fn rect_contains(rect: [f32; 4], x: f32, y: f32) -> bool {
    x >= rect[0] && y >= rect[1] && x <= rect[0] + rect[2] && y <= rect[1] + rect[3]
}

/// Drops the `-{n}` merman appends to an element's own id.
fn strip_index(rest: &str) -> Option<String> {
    let (name, index) = rest.rsplit_once('-')?;
    if name.is_empty() || index.is_empty() || !index.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(name.to_string())
}

/// The position of the statement a state diagram's `edge{n}` was written by.
fn transition_index(edge_id: &str) -> Option<usize> {
    edge_id.strip_prefix("edge")?.parse::<usize>().ok()
}

/// Which kind of label a `merman-foreignobject-fallback` group replaced, from
/// the classes of the element it was lifted out of.
///
/// An ER entity's attribute name is claimed by its attribute row. Its other
/// cells and a cluster's title are labels too, but none belongs to a box this
/// pass reports, so they are left unclaimed. So are a class's annotation,
/// members and methods, which `merman` 0.8 lifts out as well: only its
/// `label-group` holds the name.
fn fallback_role(element: &BytesStart<'_>) -> Option<FallbackRole> {
    let class = attribute(element, "class")?;
    if class
        .split_ascii_whitespace()
        .any(|token| token == "attribute-name")
    {
        return Some(FallbackRole::AttributeName);
    }
    let mut tokens = class.split_ascii_whitespace();
    let mut is_node = false;
    let mut is_edge = false;
    for token in &mut tokens {
        match token {
            "edgeLabel" => is_edge = true,
            "nodeLabel" => is_node = true,
            "cluster-label" | "attribute-type" | "attribute-name" | "attribute-keys"
            | "attribute-comment" | "edgeTerminals" | "annotation-group" | "members-group"
            | "methods-group" => return None,
            _ => {}
        }
    }
    match (is_edge, is_node) {
        (true, false) => Some(FallbackRole::Edge),
        (false, true) => Some(FallbackRole::Node),
        _ => None,
    }
}

/// Splits the endpoints out of an ER relationship's
/// `id_{from element id}_{to element id}_{n}`.
///
/// Both halves are element ids, which carry an index of their own and may hold
/// any character an entity name may, so the split is found by matching against
/// the ids of the entities the diagram actually drew rather than by counting
/// underscores. A relationship whose ends are not both among them is dropped.
fn split_er_edge_id(
    edge_id: &str,
    suffixes: &[String],
    node_ids: &[&str],
) -> Option<(String, String)> {
    let body = edge_id.strip_prefix("id_")?;
    let pair = match body.rsplit_once('_') {
        Some((pair, index)) if index.parse::<usize>().is_ok() => pair,
        _ => body,
    };
    for (from_index, from_suffix) in suffixes.iter().enumerate() {
        let Some(rest) = pair
            .strip_prefix(from_suffix.as_str())
            .and_then(|rest| rest.strip_prefix('_'))
        else {
            continue;
        };
        for (to_index, to_suffix) in suffixes.iter().enumerate() {
            if rest == to_suffix {
                return Some((
                    node_ids.get(from_index)?.to_string(),
                    node_ids.get(to_index)?.to_string(),
                ));
            }
        }
    }
    None
}

/// Splits the endpoints out of merman's `L_{from}_{to}_{n}` edge id.
///
/// Node ids may themselves contain `_`, so the split point is chosen by
/// checking both halves against the ids actually present in the diagram, and
/// only falls back to the first underscore when no pair matches.
fn split_edge_id(edge_id: &str, node_ids: &[&str]) -> (String, String) {
    let body = edge_id.strip_prefix("L_").unwrap_or(edge_id);
    let pair = match body.rsplit_once('_') {
        Some((pair, suffix)) if suffix.parse::<usize>().is_ok() => pair,
        _ => body,
    };
    for (index, _) in pair.match_indices('_') {
        let from = &pair[..index];
        let to = &pair[index + 1..];
        if node_ids.contains(&from) && node_ids.contains(&to) {
            return (from.to_string(), to.to_string());
        }
    }
    match pair.split_once('_') {
        Some((from, to)) => (from.to_string(), to.to_string()),
        None => (pair.to_string(), String::new()),
    }
}

impl<'a, I> Iterator for CollectGeometry<I>
where
    I: Iterator<Item = Result<Event<'a>>>,
{
    type Item = Result<Event<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        let Some(event) = self.inner.next() else {
            if !self.finished {
                self.finished = true;
                self.finish();
            }
            return None;
        };
        let event = match event {
            Ok(event) => event,
            Err(error) => return Some(Err(error)),
        };
        self.observe(&event);
        Some(Ok(event))
    }
}

impl<I> CollectGeometry<I> {
    /// Reads one event. Nothing in here reports failure: the geometry is only
    /// an aid for hit testing, so a diagram this pass cannot make sense of
    /// comes back with a partial or empty map and still renders.
    fn observe(&mut self, event: &Event<'_>) {
        match event {
            Event::Start(element) => {
                let parent = self.frame();
                let depth = self.depth + 1;
                self.on_start(element, depth, false, parent);
                self.depth = depth;
                self.frames.push(match own_translate(element) {
                    Some((dx, dy)) => Frame {
                        dx: parent.dx + dx,
                        dy: parent.dy + dy,
                        usable: parent.usable,
                    },
                    None => Frame {
                        dx: parent.dx,
                        dy: parent.dy,
                        usable: false,
                    },
                });
            }
            Event::Empty(element) => {
                let parent = self.frame();
                let depth = self.depth + 1;
                self.on_start(element, depth, true, parent);
            }
            Event::End(_) => {
                let depth = self.depth;
                self.on_end(depth);
                self.depth = self.depth.saturating_sub(1);
                self.frames.pop();
            }
            Event::Text(text) => {
                if self.node.is_some()
                    || self.edge_label.is_some()
                    || self.fallback.is_some()
                    || self.chart.has_open_text()
                    || self.kind == Some(DiagramKind::Sequence)
                {
                    let decoded = match text.decode() {
                        Ok(decoded) => decoded,
                        Err(_) => String::from_utf8_lossy(text.as_ref()).into_owned().into(),
                    };
                    // A label containing a stray `&` fails to unescape; keep
                    // the raw text rather than losing the whole geometry.
                    match quick_xml::escape::unescape(&decoded) {
                        Ok(unescaped) => self.on_text(&unescaped),
                        Err(_) => self.on_text(&decoded),
                    }
                }
            }
            // The reader hands `&amp;` and the like over as events of their
            // own. Dropping them would leave every label with a `&`, `<` or
            // `"` in it matching nothing in the source.
            Event::GeneralRef(reference) => {
                if self.node.is_some()
                    || self.edge_label.is_some()
                    || self.fallback.is_some()
                    || self.chart.has_open_text()
                    || self.kind == Some(DiagramKind::Sequence)
                {
                    let resolved = match reference.resolve_char_ref() {
                        Ok(Some(character)) => Some(character.to_string()),
                        Ok(None) | Err(_) => reference.decode().ok().and_then(|name| {
                            quick_xml::escape::resolve_predefined_entity(&name).map(str::to_string)
                        }),
                    };
                    if let Some(text) = resolved {
                        self.on_text(&text);
                    }
                }
            }
            _ => {}
        }
    }
}

pub(super) fn process<'a>(
    events: impl Iterator<Item = Result<Event<'a>>>,
    svg_id: &str,
    sink: Rc<RefCell<DiagramGeometry>>,
) -> impl Iterator<Item = Result<Event<'a>>> {
    CollectGeometry {
        inner: events,
        sink,
        svg_id: svg_id.to_string(),
        kind: None,
        view_box: [0.0; 4],
        depth: 0,
        frames: Vec::new(),
        sections: Vec::new(),
        node: None,
        edge_label: None,
        fallback: None,
        nodes: Vec::new(),
        node_suffixes: Vec::new(),
        edge_ids: Vec::new(),
        edge_labels: Vec::new(),
        fallbacks: Vec::new(),
        chart: chart_labels::ChartLabels::default(),
        sequence: sequence::SequenceGeometry::default(),
        finished: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quick_xml::Reader;

    #[test]
    fn path_bounds_follows_absolute_and_relative_commands() {
        let bounds = path_bounds("M 10 20 L 30 40 Z").expect("path should have bounds");
        assert_eq!(bounds.to_rect(), [10.0, 20.0, 20.0, 20.0]);

        let relative = path_bounds("M 10 10 l 10 0 l 0 10 z").expect("path should have bounds");
        assert_eq!(relative.to_rect(), [10.0, 10.0, 10.0, 10.0]);

        let horizontal = path_bounds("M 0 0 H 50 V 25").expect("path should have bounds");
        assert_eq!(horizontal.to_rect(), [0.0, 0.0, 50.0, 25.0]);
    }

    #[test]
    fn path_bounds_includes_curve_control_points() {
        let bounds = path_bounds("M0 0 C 10 -5, 20 5, 30 0").expect("path should have bounds");
        assert_eq!(bounds.to_rect(), [0.0, -5.0, 30.0, 10.0]);
    }

    #[test]
    fn path_bounds_rejects_unparsable_data() {
        assert!(path_bounds("M 0 0 Q 1").is_none());
        assert!(path_bounds("M 0 0 Y 1 2").is_none());
        assert!(path_bounds("10 20 L 30 40").is_none());
    }

    #[test]
    fn split_edge_id_prefers_a_split_matching_known_node_ids() {
        assert_eq!(
            split_edge_id("L_A_1_B_0", &["A_1", "B"]),
            ("A_1".to_string(), "B".to_string())
        );
        assert_eq!(
            split_edge_id("L_A_B_2", &["A", "B"]),
            ("A".to_string(), "B".to_string())
        );
        assert_eq!(
            split_edge_id("L_start_end_0", &[]),
            ("start".to_string(), "end".to_string())
        );
    }

    #[test]
    fn own_translate_rejects_transforms_it_cannot_fold() {
        let translate = BytesStart::from_content(r#"g transform="translate(3,4)""#, 1);
        assert_eq!(own_translate(&translate), Some((3.0, 4.0)));

        let rotated = BytesStart::from_content(r#"g transform="rotate(30) translate(3,4)""#, 1);
        assert_eq!(own_translate(&rotated), None);

        let none = BytesStart::from_content("g", 1);
        assert_eq!(own_translate(&none), Some((0.0, 0.0)));
    }

    /// A label merman drew outside its element carries no id, so it is claimed
    /// by position. Where more than one box could claim it, it is dropped: the
    /// label is only ever shown, and a missing one costs a prefilled field while
    /// a wrong one puts another element's text in front of the user and has them
    /// overwrite a token they never looked at.
    #[test]
    fn a_fallback_label_claimed_by_two_boxes_is_dropped() {
        let svg = concat!(
            r#"<svg id="m-0" aria-roledescription="stateDiagram-v2" viewBox="0 0 200 100">"#,
            r#"<g class="nodes">"#,
            r#"<g class="node statediagram-state" id="state-a-0" transform="translate(20,20)">"#,
            r#"<rect class="label-container" x="-15" y="-10" width="30" height="20"/></g>"#,
            r#"<g class="node statediagram-state" id="state-b-1" transform="translate(30,20)">"#,
            r#"<rect class="label-container" x="-15" y="-10" width="30" height="20"/></g>"#,
            r#"<g class="node statediagram-state" id="state-c-2" transform="translate(120,20)">"#,
            r#"<rect class="label-container" x="-15" y="-10" width="30" height="20"/></g>"#,
            r#"</g>"#,
            r#"<g class="edgePaths"><path data-id="edge0" d="M 0 0 L 10 10"/>"#,
            r#"<path data-id="edge1" d="M 0 0 L 10 10"/></g>"#,
            r#"<g class="edgeLabels">"#,
            r#"<g transform="translate(60,60)"><g class="label" data-id="edge0"></g></g>"#,
            r#"<g transform="translate(60,60)"><g class="label" data-id="edge1"></g></g>"#,
            r#"</g>"#,
            // Inside both a and b: nothing may claim it.
            r#"<g class="merman-foreignobject-fallback nodeLabel"><text x="25" y="20">both</text></g>"#,
            // Inside c alone: claimed.
            r#"<g class="merman-foreignobject-fallback nodeLabel"><text x="120" y="20">only c</text></g>"#,
            // Anchored where two edge labels sit: nothing may claim it either.
            r#"<g class="merman-foreignobject-fallback edgeLabel"><text x="60" y="60">shared</text></g>"#,
            r#"</svg>"#,
        );
        let mut reader = Reader::from_str(svg);
        reader.config_mut().check_end_names = false;
        let mut input = Vec::new();
        loop {
            match reader.read_event() {
                Ok(Event::Eof) => break,
                Ok(event) => input.push(event.into_owned()),
                Err(error) => panic!("the fixture should parse: {error}"),
            }
        }

        let sink = Rc::new(RefCell::new(DiagramGeometry::default()));
        process(input.into_iter().map(Ok), "m-0", sink.clone())
            .collect::<Result<Vec<_>>>()
            .expect("the fixture should walk");
        let geometry = sink.take();

        let labels: Vec<(&str, &str)> = geometry
            .nodes
            .iter()
            .map(|node| (node.node_id.as_str(), node.label.as_str()))
            .collect();
        assert_eq!(
            labels,
            vec![("a", ""), ("b", ""), ("c", "only c")],
            "a label two boxes could claim must be dropped, one box's must not"
        );
        assert!(
            geometry.edges.iter().all(|edge| edge.label.is_none()),
            "a label two edge label groups could claim must be dropped: {:?}",
            geometry.edges
        );
    }

    /// The pass reads attributes only to measure them, so input it cannot
    /// decode must cost the geometry, never the rendered diagram. A stray `&`
    /// in a value is the case merman actually produces (labels are written
    /// through unescaped), and it must neither abort the event stream nor
    /// rewrite a single byte of it.
    #[test]
    fn undecodable_attributes_do_not_stop_the_event_stream() {
        let svg = concat!(
            r#"<svg id="m-0" aria-roledescription="flowchart-v2" viewBox="0 0 100 50">"#,
            r#"<g class="nodes"><g class="node default" id="m-0-flowchart-A&B-0" transform="translate(20,10)">"#,
            r#"<rect class="label-container" x="-10" y="-5" width="20" height="10"/>"#,
            r#"<g class="label"><text><tspan class="text-outer-tspan">a &amp; b</tspan></text></g>"#,
            r#"</g></g>"#,
            r#"<g class="nodes"><g class="node default" id="m-0-flowchart-C-1" transform="translate(60,10)">"#,
            r#"<rect class="label-container" x="-10" y="-5" width="20" height="10" style="fill:#fff&"/>"#,
            r#"</g></g></svg>"#,
        );
        let mut reader = Reader::from_str(svg);
        reader.config_mut().check_end_names = false;
        let mut input = Vec::new();
        loop {
            match reader.read_event() {
                Ok(Event::Eof) => break,
                Ok(event) => input.push(event.into_owned()),
                Err(error) => panic!("the fixture should parse: {error}"),
            }
        }

        let sink = Rc::new(RefCell::new(DiagramGeometry::default()));
        let output: Vec<Event<'_>> = process(input.iter().cloned().map(Ok), "m-0", sink.clone())
            .collect::<Result<Vec<_>>>()
            .expect("an attribute this pass cannot read must not fail the stream");

        assert_eq!(output, input, "the pass must not rewrite the SVG");
        let geometry = sink.take();
        // The node whose id does not decode is simply not mapped; the other one
        // still is, so the diagram stays usable.
        assert_eq!(
            geometry
                .nodes
                .iter()
                .map(|node| node.node_id.as_str())
                .collect::<Vec<_>>(),
            vec!["A&B", "C"]
        );
    }
}
