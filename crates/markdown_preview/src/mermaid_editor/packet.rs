//! The packet diagram half of the Mermaid editor's form.
//!
//! A packet diagram is a title and a run of bit fields. Mermaid refuses a
//! field that does not start right after the one before it, so the form asks
//! only for each field's length and shows the bit range that follows from it:
//! moving, adding or removing a field renumbers the rest, and a gap or an
//! overlap cannot be typed.

use editor::Editor;
use gpui::{AnyElement, App, Context, Entity, Window};
use mermaid_render::packet_model::{
    PacketDocument, PacketField, PacketNotation, is_valid_packet_bits, is_valid_packet_label,
    is_valid_packet_title, parse_packet_bits,
};
use ui::prelude::*;

use super::{
    Form, LABEL_FIELD_WIDTH, MermaidEditor, NUMBER_FIELD_WIDTH, add_button, field_frame, neighbour,
    row_controls, section_title,
};

const NEW_FIELD_BITS: u64 = 8;
const NEW_FIELD_LABEL: &str = "新しい項目";

pub(super) struct PacketForm {
    pub(super) document: PacketDocument,
    pub(super) title: Entity<Editor>,
    pub(super) fields: Vec<FieldRow>,
}

pub(super) struct FieldRow {
    pub(super) bits: Entity<Editor>,
    pub(super) label: Entity<Editor>,
}

impl PacketForm {
    pub(super) fn fields(&self) -> Vec<Entity<Editor>> {
        std::iter::once(self.title.clone())
            .chain(
                self.fields
                    .iter()
                    .flat_map(|row| [row.bits.clone(), row.label.clone()]),
            )
            .collect()
    }

    /// The packet diagram the form currently describes. A length that does not
    /// read as one becomes 0, which `PacketDocument::structure_error` refuses.
    pub(super) fn current_document(&self, cx: &App) -> PacketDocument {
        let mut document = self.document.clone();
        let title = self.title.read(cx).text(cx);
        document.title = if title.is_empty() {
            // A bare `title` with nothing after it is a title statement all the
            // same, and turning it into no statement at all would rewrite a
            // fence the user only looked at.
            document.title.take().filter(|title| title.is_empty())
        } else {
            Some(title)
        };
        for (field, row) in document.fields.iter_mut().zip(&self.fields) {
            field.bits = parse_packet_bits(row.bits.read(cx).text(cx).trim()).unwrap_or(0);
            field.label = row.label.read(cx).text(cx);
        }
        document
    }
}

/// The first thing wrong with what the user typed, in the words shown above the
/// form. Every check lives in `PacketDocument::structure_error`, which the
/// caller consults right after this.
pub(super) fn problem(_document: &PacketDocument) -> Option<SharedString> {
    None
}

impl MermaidEditor {
    pub(super) fn new_packet_form(
        &mut self,
        document: PacketDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PacketForm {
        let title = self.new_field(
            document.title.clone().unwrap_or_default().as_str(),
            window,
            cx,
        );
        let contents = document
            .fields
            .iter()
            .map(|field| (field.bits.to_string(), field.label.clone()))
            .collect::<Vec<_>>();
        let fields = contents
            .iter()
            .map(|(bits, label)| FieldRow {
                bits: self.new_field(bits, window, cx),
                label: self.new_field(label, window, cx),
            })
            .collect();
        PacketForm {
            document,
            title,
            fields,
        }
    }

    pub(super) fn packet(&self) -> Option<&PacketForm> {
        match &self.form {
            Form::Packet(form) => Some(form),
            _ => None,
        }
    }

    pub(super) fn packet_mut(&mut self) -> Option<&mut PacketForm> {
        match &mut self.form {
            Form::Packet(form) => Some(form),
            _ => None,
        }
    }

    fn packet_changed(&mut self, cx: &mut Context<Self>) {
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn add_packet_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A new field is written the way the one before it is, so that a
        // packet of `+8` lengths does not grow a lone range.
        let notation = self
            .packet()
            .and_then(|form| form.document.fields.last())
            .map_or(PacketNotation::Range, |field| match field.notation {
                PacketNotation::Length => PacketNotation::Length,
                PacketNotation::Range | PacketNotation::Single => PacketNotation::Range,
            });
        let bits = self.new_field(&NEW_FIELD_BITS.to_string(), window, cx);
        let label = self.new_field(NEW_FIELD_LABEL, window, cx);
        let Some(form) = self.packet_mut() else {
            return;
        };
        form.document.fields.push(PacketField {
            bits: NEW_FIELD_BITS,
            notation,
            label: NEW_FIELD_LABEL.to_string(),
        });
        form.fields.push(FieldRow { bits, label });
        self.packet_changed(cx);
    }

    pub(super) fn remove_packet_field(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(form) = self.packet_mut() else {
            return;
        };
        if index >= form.fields.len() || index >= form.document.fields.len() {
            return;
        }
        form.document.fields.remove(index);
        form.fields.remove(index);
        self.packet_changed(cx);
    }

    pub(super) fn move_packet_field(
        &mut self,
        index: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.packet_mut() else {
            return;
        };
        let Some(other) = neighbour(index, forward, form.fields.len()) else {
            return;
        };
        if other >= form.document.fields.len() || index >= form.document.fields.len() {
            return;
        }
        form.document.fields.swap(index, other);
        form.fields.swap(index, other);
        self.packet_changed(cx);
    }

    pub(super) fn render_packet(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(form) = self.packet() else {
            return Vec::new();
        };
        let current = form.current_document(cx);
        let ranges = current.ranges();
        let last = form.fields.len().saturating_sub(1);

        let header = h_flex()
            .w_full()
            .gap_2()
            .child(section_title("タイトル"))
            .child(
                field_frame(
                    &form.title,
                    LABEL_FIELD_WIDTH,
                    current
                        .title
                        .as_ref()
                        .is_none_or(|title| is_valid_packet_title(title)),
                    window,
                    cx,
                )
                .flex_1()
                .child(form.title.clone()),
            )
            .into_any_element();

        let mut section = v_flex().w_full().gap_1().child(section_title(
            "項目 (ビット数とラベル。範囲は上から自動で決まります)",
        ));
        for index in 0..form.fields.len() {
            let (Some(row), Some(field), Some((start, end))) = (
                form.fields.get(index),
                current.fields.get(index),
                ranges.get(index),
            ) else {
                continue;
            };
            let bits_text = row.bits.read(cx).text(cx);
            let range = if start == end {
                format!("{start}")
            } else {
                format!("{start}–{end}")
            };
            section = section.child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        div()
                            .w(NUMBER_FIELD_WIDTH)
                            .flex_none()
                            .child(Label::new(range).size(LabelSize::Small).color(Color::Muted)),
                    )
                    .child(
                        field_frame(
                            &row.bits,
                            NUMBER_FIELD_WIDTH,
                            is_valid_packet_bits(bits_text.trim()),
                            window,
                            cx,
                        )
                        .child(row.bits.clone()),
                    )
                    .child(
                        Label::new("ビット")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        field_frame(
                            &row.label,
                            LABEL_FIELD_WIDTH,
                            is_valid_packet_label(&field.label),
                            window,
                            cx,
                        )
                        .flex_1()
                        .child(row.label.clone()),
                    )
                    .child(row_controls(
                        "mermaid-packet-field",
                        index,
                        index == 0,
                        index >= last,
                        cx.listener(move |this, _, _window, cx| {
                            this.move_packet_field(index, false, cx)
                        }),
                        cx.listener(move |this, _, _window, cx| {
                            this.move_packet_field(index, true, cx)
                        }),
                        cx.listener(move |this, _, _window, cx| {
                            this.remove_packet_field(index, cx)
                        }),
                    )),
            );
        }
        let section = section
            .child(add_button(
                "mermaid-packet-add-field",
                "項目を追加",
                cx.listener(|this, _, window, cx| this.add_packet_field(window, cx)),
            ))
            .into_any_element();

        vec![header, section]
    }

    pub(super) fn render_packet_toolbar(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> AnyElement {
        div().into_any_element()
    }
}

#[cfg(test)]
#[path = "packet_tests.rs"]
mod packet_tests;
