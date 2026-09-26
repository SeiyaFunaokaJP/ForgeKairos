//! A sequence diagram's SVG is not laid out in the `nodes` / `edgePaths` /
//! `edgeLabels` sections the other kinds share, so it is read here on its own:
//!
//! * a participant is drawn twice, at the top and at the bottom, as a shape
//!   carrying `name="{participant id}"` and an `actor-top` / `actor-bottom`
//!   class — a bare `<rect>` whose label is the `<text>` beside it in the same
//!   group, or a whole `<g>` (the stick figure of an `actor`) holding both;
//! * a message is a `<line>` or `<path>` with `data-et="message"` and
//!   `data-id="i{n}"`, preceded by one `<text class="messageText">` per line of
//!   its text, none of which carry an id;
//! * a note is a `<g data-et="note" data-id="i{n}">` around a `rect.note` and
//!   its `noteText` lines.
//!
//! The `{n}` of a message and of a note counts every entry of merman's message
//! list, activations and block boundaries included, so it is only used to put
//! messages and notes in order; their position in that order is the
//! [`EdgeGeometry::statement_index`] the source is searched by.
//!
//! merman draws text without measuring it into the SVG, so a text's box is
//! estimated from its anchor, its font size and its characters. That is only
//! ever used to hit-test and to highlight, never to lay anything out.

use quick_xml::events::BytesStart;

use super::{BoundingBox, Frame, attribute, attribute_f64, extend_optional, has_class, join_rows};
use crate::{EdgeGeometry, NodeGeometry};

const DEFAULT_FONT_SIZE: f64 = 16.0;

/// A participant box being read.
struct PendingParticipant {
    name: String,
    /// The depth of the element whose end completes the participant: the `<g>`
    /// that is the participant, or the group around a bare `<rect>`.
    end_depth: usize,
    bounds: Option<BoundingBox>,
    rows: Vec<String>,
}

struct PendingNote {
    order: u64,
    start_depth: usize,
    bounds: Option<BoundingBox>,
    rows: Vec<String>,
}

/// One line of text whose box is still being read.
struct PendingText {
    start_depth: usize,
    text: String,
    anchor: Option<(f64, f64)>,
    font_size: f64,
    text_anchor: TextAnchor,
    /// Whether the line is drawn with `dy="1em"`, which puts it one line below
    /// its `y`.
    shifted: bool,
    role: TextRole,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TextRole {
    Participant,
    Note,
    Message,
}

#[derive(Clone, Copy)]
enum TextAnchor {
    Start,
    Middle,
    End,
}

/// A message or a note, keyed by merman's own number for it.
struct Item {
    order: u64,
    edge_id: String,
    label: Option<String>,
    rect: Option<[f32; 4]>,
}

#[derive(Default)]
pub(super) struct SequenceGeometry {
    participant: Option<PendingParticipant>,
    note: Option<PendingNote>,
    text: Option<PendingText>,
    /// The lines of message text read since the last message, which belong to
    /// the next one.
    message_rows: Vec<String>,
    message_bounds: Option<BoundingBox>,
    participants: Vec<NodeGeometry>,
    items: Vec<Item>,
    /// Set by an id this pass cannot read, which makes the numbering of every
    /// message and note untrustworthy.
    unreadable: bool,
}

impl SequenceGeometry {
    pub(super) fn on_start(
        &mut self,
        element: &BytesStart<'_>,
        depth: usize,
        empty: bool,
        frame: Frame,
    ) {
        let name = element.name();
        let tag = name.as_ref();

        if self.text.is_some() {
            return;
        }
        if tag == b"text" && !empty {
            self.start_text(element, depth, frame);
            return;
        }

        if self.participant.is_none()
            && (has_class(element, "actor-top") || has_class(element, "actor-bottom"))
            && let Some(participant) = attribute(element, "name")
        {
            // A bare `<rect>` is completed by the end of the group around it,
            // which also holds its label; a `<g>` by its own end.
            let end_depth = if tag == b"g" && !empty {
                depth
            } else {
                depth.saturating_sub(1)
            };
            self.participant = Some(PendingParticipant {
                name: participant,
                end_depth,
                bounds: None,
                rows: Vec::new(),
            });
        }
        if let Some(participant) = self.participant.as_mut() {
            if let Some(bounds) = shape_bounds_in(element, frame) {
                extend_optional(&mut participant.bounds, bounds);
            }
            return;
        }

        if tag == b"g"
            && !empty
            && self.note.is_none()
            && attribute(element, "data-et").as_deref() == Some("note")
        {
            match attribute(element, "data-id")
                .as_deref()
                .and_then(item_order)
            {
                Some(order) => {
                    self.note = Some(PendingNote {
                        order,
                        start_depth: depth,
                        bounds: None,
                        rows: Vec::new(),
                    });
                }
                None => self.unreadable = true,
            }
            return;
        }
        if let Some(note) = self.note.as_mut() {
            if tag == b"rect"
                && has_class(element, "note")
                && let Some(bounds) = shape_bounds_in(element, frame)
            {
                extend_optional(&mut note.bounds, bounds);
            }
            return;
        }

        if attribute(element, "data-et").as_deref() == Some("message") {
            let Some(data_id) = attribute(element, "data-id") else {
                self.unreadable = true;
                return;
            };
            let Some(order) = item_order(&data_id) else {
                self.unreadable = true;
                return;
            };
            let mut bounds = self.message_bounds.take();
            if let Some(line) = shape_bounds_in(element, frame) {
                extend_optional(&mut bounds, line);
            }
            let rows = std::mem::take(&mut self.message_rows);
            self.items.push(Item {
                order,
                edge_id: data_id,
                label: join_rows(rows),
                rect: bounds
                    .map(|bounds| bounds.padded(2.0))
                    .filter(BoundingBox::is_usable)
                    .map(BoundingBox::to_rect),
            });
        }
    }

    fn start_text(&mut self, element: &BytesStart<'_>, depth: usize, frame: Frame) {
        let role = if self.participant.is_some() {
            TextRole::Participant
        } else if self.note.is_some() {
            TextRole::Note
        } else if has_class(element, "messageText") {
            TextRole::Message
        } else {
            return;
        };
        let style = attribute(element, "style").unwrap_or_default();
        let text_anchor = match attribute(element, "text-anchor")
            .or_else(|| style_value(&style, "text-anchor"))
            .as_deref()
        {
            Some("start") => TextAnchor::Start,
            Some("end") => TextAnchor::End,
            _ => TextAnchor::Middle,
        };
        let font_size = style_value(&style, "font-size")
            .and_then(|value| value.trim_end_matches("px").trim().parse::<f64>().ok())
            .filter(|size| size.is_finite() && *size > 0.0)
            .unwrap_or(DEFAULT_FONT_SIZE);
        let anchor = match (attribute_f64(element, "x"), attribute_f64(element, "y")) {
            (Some(x), Some(y)) if frame.usable => Some((x + frame.dx, y + frame.dy)),
            _ => None,
        };
        self.text = Some(PendingText {
            start_depth: depth,
            text: String::new(),
            anchor,
            font_size,
            text_anchor,
            shifted: attribute(element, "dy").as_deref() == Some("1em"),
            role,
        });
    }

    pub(super) fn on_text(&mut self, text: &str) {
        if let Some(pending) = self.text.as_mut() {
            pending.text.push_str(text);
        }
    }

    pub(super) fn on_end(&mut self, depth: usize) {
        if self
            .text
            .as_ref()
            .is_some_and(|text| text.start_depth == depth)
            && let Some(text) = self.text.take()
        {
            self.finish_text(text);
        }
        if self
            .note
            .as_ref()
            .is_some_and(|note| note.start_depth == depth)
            && let Some(note) = self.note.take()
        {
            self.items.push(Item {
                order: note.order,
                edge_id: format!("i{}", note.order),
                label: join_rows(note.rows),
                rect: note
                    .bounds
                    .filter(BoundingBox::is_usable)
                    .map(BoundingBox::to_rect),
            });
        }
        if self
            .participant
            .as_ref()
            .is_some_and(|participant| participant.end_depth == depth)
            && let Some(participant) = self.participant.take()
            && let Some(bounds) = participant.bounds.filter(BoundingBox::is_usable)
        {
            self.participants.push(NodeGeometry {
                node_id: participant.name,
                label: join_rows(participant.rows).unwrap_or_default(),
                rect: bounds.to_rect(),
            });
        }
    }

    fn finish_text(&mut self, text: PendingText) {
        let line = text.text.replace('\u{200B}', "");
        let bounds = text
            .anchor
            .and_then(|anchor| estimated_text_bounds(&text, anchor));
        match text.role {
            TextRole::Participant => {
                if let Some(participant) = self.participant.as_mut() {
                    participant.rows.push(line);
                    if let Some(bounds) = bounds {
                        extend_optional(&mut participant.bounds, bounds);
                    }
                }
            }
            TextRole::Note => {
                if let Some(note) = self.note.as_mut() {
                    note.rows.push(line);
                }
            }
            TextRole::Message => {
                self.message_rows.push(line);
                if let Some(bounds) = bounds {
                    extend_optional(&mut self.message_bounds, bounds);
                }
            }
        }
    }

    /// The participants' boxes, top row first, and the messages and notes in
    /// the order they were written.
    pub(super) fn finish(self) -> (Vec<NodeGeometry>, Vec<EdgeGeometry>) {
        let mut participants = self.participants;
        // merman writes a participant's bottom box before its top one, and
        // which box counts as the first copy decides nothing but the order a
        // consumer meets them in; top first is the order they are read in.
        participants.sort_by(|first, second| first.rect[1].total_cmp(&second.rect[1]));
        if self.unreadable {
            return (participants, Vec::new());
        }
        let mut items = self.items;
        items.sort_by_key(|item| item.order);
        if items.windows(2).any(|pair| pair[0].order == pair[1].order) {
            return (participants, Vec::new());
        }
        let edges = items
            .into_iter()
            .enumerate()
            .map(|(index, item)| EdgeGeometry {
                edge_id: item.edge_id,
                from: String::new(),
                to: String::new(),
                occurrence: 0,
                statement_index: Some(index),
                label: item.label,
                label_rect: item.rect,
            })
            .collect();
        (participants, edges)
    }
}

/// merman's number for a message or a note, from its `data-id="i{n}"`.
fn item_order(data_id: &str) -> Option<u64> {
    data_id.strip_prefix('i')?.parse().ok()
}

/// A shape's box in `viewBox` coordinates, when the frame it is drawn in can be
/// folded into a translation.
fn shape_bounds_in(element: &BytesStart<'_>, frame: Frame) -> Option<BoundingBox> {
    if !frame.usable {
        return None;
    }
    super::shape_bounds(element).map(|bounds| bounds.translated(frame.dx, frame.dy))
}

/// The value of `property` in an inline `style`, if it sets one.
fn style_value(style: &str, property: &str) -> Option<String> {
    style.split(';').find_map(|declaration| {
        let (name, value) = declaration.split_once(':')?;
        (name.trim() == property).then(|| value.trim().to_string())
    })
}

/// Roughly where a line of text is drawn: a full em per wide character and a
/// little over half of one per narrow one, which errs on the wide side for the
/// Latin text of proportional fonts.
fn estimated_text_bounds(text: &PendingText, (x, y): (f64, f64)) -> Option<BoundingBox> {
    let width: f64 = text
        .text
        .chars()
        .filter(|character| *character != '\u{200B}')
        .map(|character| {
            if (character as u32) < 0x1100 {
                0.6
            } else {
                1.0
            }
        })
        .sum::<f64>()
        * text.font_size;
    if width <= 0.0 {
        return None;
    }
    let left = match text.text_anchor {
        TextAnchor::Start => x,
        TextAnchor::Middle => x - width / 2.0,
        TextAnchor::End => x - width,
    };
    let center_y = if text.shifted { y + text.font_size } else { y };
    let half_height = text.font_size * 0.7;
    let mut bounds = BoundingBox::point(left, center_y - half_height);
    bounds.extend(left + width, center_y + half_height);
    Some(bounds)
}
