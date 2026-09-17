//! The pie chart half of the Mermaid editor's form.
//!
//! A pie chart is a title, a flag and a list of labelled numbers, so the form
//! is one row per slice. The percentage beside each row is what the drawing
//! will show, worked out from the values as they are typed rather than from the
//! last one that parsed, so that a half-typed number does not silently move the
//! other slices.

use editor::Editor;
use gpui::{AnyElement, App, Context, Entity, Window};
use mermaid_render::pie_model::{
    PieDocument, PieSlice, is_valid_pie_label, is_valid_pie_title, is_valid_pie_value,
};
use ui::{Checkbox, ToggleState, prelude::*};

use super::{
    Form, LABEL_FIELD_WIDTH, MermaidEditor, NUMBER_FIELD_WIDTH, add_button, field_frame, neighbour,
    row_controls, section_title,
};

pub(super) struct PieForm {
    pub(super) document: PieDocument,
    pub(super) title: Entity<Editor>,
    pub(super) slices: Vec<SliceRow>,
}

pub(super) struct SliceRow {
    pub(super) label: Entity<Editor>,
    pub(super) value: Entity<Editor>,
}

impl PieForm {
    pub(super) fn fields(&self) -> Vec<Entity<Editor>> {
        std::iter::once(self.title.clone())
            .chain(
                self.slices
                    .iter()
                    .flat_map(|row| [row.label.clone(), row.value.clone()]),
            )
            .collect()
    }

    /// The chart the form currently describes.
    pub(super) fn current_document(&self, cx: &App) -> PieDocument {
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
        for (slice, row) in document.slices.iter_mut().zip(&self.slices) {
            slice.label = row.label.read(cx).text(cx);
            slice.value = row.value.read(cx).text(cx).trim().to_string();
        }
        document
    }
}

/// The first thing wrong with what the user typed, in the words shown above the
/// form.
pub(super) fn problem(document: &PieDocument) -> Option<SharedString> {
    if let Some(title) = &document.title
        && !is_valid_pie_title(title)
    {
        return Some("タイトルに `%%` と改行は使えません (前後の空白も不可)。".into());
    }
    for slice in &document.slices {
        if !is_valid_pie_label(&slice.label) {
            return Some(
                format!("ラベル「{}」は使えません (空と `%%` は不可)。", slice.label).into(),
            );
        }
        if !is_valid_pie_value(&slice.value) {
            return Some(
                format!(
                    "「{}」の値「{}」は使えません (0 以上の小数だけ)。",
                    slice.label, slice.value
                )
                .into(),
            );
        }
    }
    None
}

impl MermaidEditor {
    pub(super) fn new_pie_form(
        &mut self,
        document: PieDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PieForm {
        let title = self.new_field(
            document.title.clone().unwrap_or_default().as_str(),
            window,
            cx,
        );
        let mut slices = Vec::with_capacity(document.slices.len());
        for slice in &document.slices {
            let (label, value) = (slice.label.clone(), slice.value.clone());
            slices.push(SliceRow {
                label: self.new_field(&label, window, cx),
                value: self.new_field(&value, window, cx),
            });
        }
        PieForm {
            document,
            title,
            slices,
        }
    }

    pub(super) fn pie(&self) -> Option<&PieForm> {
        match &self.form {
            Form::Pie(form) => Some(form),
            _ => None,
        }
    }

    pub(super) fn pie_mut(&mut self) -> Option<&mut PieForm> {
        match &mut self.form {
            Form::Pie(form) => Some(form),
            _ => None,
        }
    }

    pub(super) fn add_slice(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let label = self.new_field("", window, cx);
        let value = self.new_field("1", window, cx);
        let Some(form) = self.pie_mut() else {
            return;
        };
        form.document.slices.push(PieSlice {
            label: String::new(),
            value: "1".to_string(),
        });
        form.slices.push(SliceRow { label, value });
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn remove_slice(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(form) = self.pie_mut() else {
            return;
        };
        if index >= form.slices.len() || index >= form.document.slices.len() {
            return;
        }
        form.document.slices.remove(index);
        form.slices.remove(index);
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn move_slice(&mut self, index: usize, forward: bool, cx: &mut Context<Self>) {
        let Some(form) = self.pie_mut() else {
            return;
        };
        let Some(other) = neighbour(index, forward, form.slices.len()) else {
            return;
        };
        if other >= form.document.slices.len() || index >= form.document.slices.len() {
            return;
        }
        form.document.slices.swap(index, other);
        form.slices.swap(index, other);
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn set_show_data(&mut self, show_data: bool, cx: &mut Context<Self>) {
        let Some(form) = self.pie_mut() else {
            return;
        };
        form.document.show_data = show_data;
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn render_pie(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(form) = self.pie() else {
            return Vec::new();
        };
        let current = form.current_document(cx);
        let total = current.total();
        let last = form.slices.len().saturating_sub(1);

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
                        .is_none_or(|title| is_valid_pie_title(title)),
                    window,
                    cx,
                )
                .flex_1()
                .child(form.title.clone()),
            )
            .child(
                Checkbox::new("mermaid-pie-show-data", form.document.show_data.into())
                    .label("値を表示")
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|this, state: &ToggleState, _window, cx| {
                        this.set_show_data(*state == ToggleState::Selected, cx);
                    })),
            )
            .into_any_element();

        let mut section = v_flex().w_full().gap_1().child(section_title("項目"));
        for index in 0..form.slices.len() {
            let Some(row) = form.slices.get(index) else {
                continue;
            };
            let Some(slice) = current.slices.get(index) else {
                continue;
            };
            section = section.child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        field_frame(
                            &row.label,
                            LABEL_FIELD_WIDTH,
                            is_valid_pie_label(&slice.label),
                            window,
                            cx,
                        )
                        .flex_1()
                        .child(row.label.clone()),
                    )
                    .child(
                        field_frame(
                            &row.value,
                            NUMBER_FIELD_WIDTH,
                            is_valid_pie_value(&slice.value),
                            window,
                            cx,
                        )
                        .child(row.value.clone()),
                    )
                    .child(
                        Label::new(share_label(slice, total))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(row_controls(
                        "mermaid-pie-slice",
                        index,
                        index == 0,
                        index >= last,
                        cx.listener(move |this, _, _window, cx| this.move_slice(index, false, cx)),
                        cx.listener(move |this, _, _window, cx| this.move_slice(index, true, cx)),
                        cx.listener(move |this, _, _window, cx| this.remove_slice(index, cx)),
                    )),
            );
        }
        let section = section
            .child(add_button(
                "mermaid-pie-add-slice",
                "項目を追加",
                cx.listener(|this, _, window, cx| this.add_slice(window, cx)),
            ))
            .into_any_element();

        vec![header, section]
    }
}

/// The share of the chart one slice takes, as the drawing will show it. A chart
/// whose values are all zero has no shares to speak of, and dividing by its
/// total would give every slice `NaN`.
pub(super) fn share_label(slice: &PieSlice, total: f64) -> SharedString {
    let Some(value) = slice.value_number().filter(|value| value.is_finite()) else {
        return "—".into();
    };
    if total <= 0.0 {
        return "—".into();
    }
    format!("{:.1}%", value / total * 100.0).into()
}
