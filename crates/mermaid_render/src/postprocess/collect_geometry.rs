//! Collects a coordinate map of a flowchart's nodes and edge labels, so that a
//! rendered diagram can be hit-tested back to the mermaid source.
//!
//! merman keeps the diagram's structure in the SVG it emits: every flowchart
//! node is a `<g class="node …" id="{diagram_id}-flowchart-{node id}-{n}">`
//! wrapping a shape that carries the `label-container` class, and every edge is
//! a `<path data-id="L_{from}_{to}_{n}">` whose label (when it has one) is a
//! `<g class="label" data-id="L_{from}_{to}_{n}">` holding a
//! `<rect class="background">`. This pass walks the event stream and turns that
//! into a [`DiagramGeometry`] in `viewBox` coordinates.
//!
//! It runs after [`super::center_edge_labels`] so that the edge label boxes it
//! reports are the corrected ones, and before the passes that only rewrite
//! styles and classes, which never move anything.
//!
//! Only flowcharts are mapped; for every other diagram kind the result stays
//! empty, since neither the node ids nor the source syntax of the other kinds
//! line up with what the editing UI needs.

use std::cell::RefCell;
use std::rc::Rc;

use anyhow::Result;
use quick_xml::events::{BytesStart, Event};

use crate::{DiagramGeometry, EdgeGeometry, NodeGeometry};

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

/// The section of the diagram currently being walked. merman groups a
/// flowchart into `<g class="nodes">`, `<g class="edgePaths">` and
/// `<g class="edgeLabels">`; anything outside them (clusters, markers, defs)
/// is not mapped.
#[derive(Clone, Copy, PartialEq)]
enum Section {
    Nodes,
    EdgePaths,
    EdgeLabels,
}

struct PendingNode {
    node_id: String,
    start_depth: usize,
    rows: Vec<String>,
    bounds: Option<BoundingBox>,
    /// The depth of the innermost open element that carries the
    /// `label-container` class, if any; shapes below it make up the node's box.
    container_depth: Option<usize>,
}

struct PendingEdgeLabel {
    edge_id: String,
    start_depth: usize,
    rows: Vec<String>,
    bounds: Option<BoundingBox>,
}

struct CollectGeometry<I> {
    inner: I,
    sink: Rc<RefCell<DiagramGeometry>>,
    /// `{svg id}-flowchart-`, the prefix merman puts in front of every node id.
    node_id_prefix: String,
    is_flowchart: bool,
    view_box: [f32; 4],
    depth: usize,
    frames: Vec<Frame>,
    section: Option<(Section, usize)>,
    node: Option<PendingNode>,
    edge_label: Option<PendingEdgeLabel>,
    nodes: Vec<NodeGeometry>,
    edge_ids: Vec<String>,
    edge_labels: Vec<(String, Option<String>, Option<[f32; 4]>)>,
    finished: bool,
}

/// An attribute's value, read for measurement only.
///
/// Nothing here may fail: the diagram has to render whether or not this pass
/// can make sense of it, so an attribute list quick-xml refuses to parse reads
/// as absent, and a value whose entities do not decode (a stray `&`, which the
/// passes that need the text handle themselves) is measured raw.
fn attribute(element: &BytesStart<'_>, name: &str) -> Option<String> {
    let attr = element.try_get_attribute(name).ok()??;
    Some(match attr.unescape_value() {
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

    /// Strips the `{svg id}-flowchart-` prefix and the `-{n}` suffix merman
    /// wraps around a node's own id.
    fn plain_node_id(&self, element_id: &str) -> Option<String> {
        let rest = element_id
            .strip_prefix(self.node_id_prefix.as_str())
            .or_else(|| {
                element_id
                    .find("-flowchart-")
                    .map(|index| &element_id[index + "-flowchart-".len()..])
            })?;
        let (name, index) = rest.rsplit_once('-')?;
        if name.is_empty() || index.is_empty() || !index.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        Some(name.to_string())
    }

    fn on_start(&mut self, element: &BytesStart<'_>, depth: usize, empty: bool, frame: Frame) {
        if !self.is_flowchart {
            if element.name().as_ref() == b"svg" {
                self.read_svg_root(element);
            }
            return;
        }

        if self.section.is_none() {
            let section = if has_class(element, "nodes") {
                Some(Section::Nodes)
            } else if has_class(element, "edgePaths") {
                Some(Section::EdgePaths)
            } else if has_class(element, "edgeLabels") {
                Some(Section::EdgeLabels)
            } else {
                None
            };
            if let Some(section) = section {
                self.section = Some((section, depth));
                return;
            }
        }

        let Some((section, _)) = self.section else {
            return;
        };

        match section {
            Section::Nodes => self.on_node_element(element, depth, empty, frame),
            Section::EdgePaths => {
                if element.name().as_ref() == b"path"
                    && let Some(edge_id) = self.edge_id_of(element)
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
        if !role.starts_with("flowchart")
            && !class
                .split_ascii_whitespace()
                .any(|token| token == "flowchart")
        {
            return;
        }
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
        self.is_flowchart = true;
    }

    fn edge_id_of(&self, element: &BytesStart<'_>) -> Option<String> {
        if let Some(data_id) = attribute(element, "data-id") {
            return Some(data_id);
        }
        let element_id = attribute(element, "id")?;
        element_id
            .find("-L_")
            .map(|index| element_id[index + 1..].to_string())
    }

    fn on_node_element(
        &mut self,
        element: &BytesStart<'_>,
        depth: usize,
        empty: bool,
        frame: Frame,
    ) {
        if self.node.is_none() {
            if !has_class(element, "node") {
                return;
            }
            let Some(element_id) = attribute(element, "id") else {
                return;
            };
            let Some(node_id) = self.plain_node_id(&element_id) else {
                return;
            };
            self.node = Some(PendingNode {
                node_id,
                start_depth: depth,
                rows: Vec::new(),
                bounds: None,
                container_depth: None,
            });
            return;
        }

        let is_container = has_class(element, "label-container");
        let is_row = element.name().as_ref() == b"tspan" && has_class(element, "text-outer-tspan");
        let bounds = (frame.usable && (is_container || self.has_open_container()))
            .then(|| shape_bounds(element))
            .flatten();
        let Some(node) = self.node.as_mut() else {
            return;
        };
        if is_row {
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
        if let Some(node) = self.node.as_mut()
            && node.container_depth == Some(depth)
        {
            node.container_depth = None;
        }
        if self
            .node
            .as_ref()
            .is_some_and(|node| node.start_depth == depth)
            && let Some(node) = self.node.take()
        {
            if let Some(bounds) = node.bounds.filter(BoundingBox::is_usable) {
                self.nodes.push(NodeGeometry {
                    node_id: node.node_id,
                    label: join_rows(node.rows).unwrap_or_default(),
                    rect: bounds.to_rect(),
                });
            }
        }
        if self
            .edge_label
            .as_ref()
            .is_some_and(|edge_label| edge_label.start_depth == depth)
            && let Some(edge_label) = self.edge_label.take()
        {
            self.edge_labels.push((
                edge_label.edge_id,
                join_rows(edge_label.rows),
                edge_label
                    .bounds
                    .filter(BoundingBox::is_usable)
                    .map(BoundingBox::to_rect),
            ));
        }
        if let Some((_, section_depth)) = self.section
            && section_depth == depth
        {
            self.section = None;
        }
    }

    fn on_text(&mut self, text: &str) {
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
        let nodes = std::mem::take(&mut self.nodes);
        let edge_labels = std::mem::take(&mut self.edge_labels);
        let node_ids: Vec<&str> = nodes.iter().map(|node| node.node_id.as_str()).collect();
        let mut seen_pairs: Vec<((String, String), usize)> = Vec::new();
        let edges = self
            .edge_ids
            .iter()
            .map(|edge_id| {
                let (from, to) = split_edge_id(edge_id, &node_ids);
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
                let label = edge_labels.iter().find(|(id, _, _)| id == edge_id);
                EdgeGeometry {
                    edge_id: edge_id.clone(),
                    from,
                    to,
                    occurrence,
                    label: label.and_then(|(_, label, _)| label.clone()),
                    label_rect: label.and_then(|(_, _, rect)| *rect),
                }
            })
            .collect();
        drop(node_ids);
        *self.sink.borrow_mut() = DiagramGeometry {
            view_box: self.view_box,
            nodes,
            edges,
        };
    }
}

fn join_rows(rows: Vec<String>) -> Option<String> {
    let joined = rows.join("\n");
    (!joined.trim().is_empty()).then_some(joined)
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
                if self.node.is_some() || self.edge_label.is_some() {
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
        node_id_prefix: format!("{svg_id}-flowchart-"),
        is_flowchart: false,
        view_box: [0.0; 4],
        depth: 0,
        frames: Vec::new(),
        section: None,
        node: None,
        edge_label: None,
        nodes: Vec::new(),
        edge_ids: Vec::new(),
        edge_labels: Vec::new(),
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
