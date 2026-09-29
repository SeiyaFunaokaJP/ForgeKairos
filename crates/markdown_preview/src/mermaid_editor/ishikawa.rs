//! The Ishikawa diagram half of the Mermaid editor's form.
//!
//! An Ishikawa diagram is an effect — the problem at the head of the fish —
//! and an outline of causes: the first level are the main bones, the causes on
//! them sit one level deeper and so on. The form is a field for the effect and
//! one row per cause, pushed right by its depth, with the same row buttons as
//! the tree view's outline.

use editor::Editor;
use gpui::{AnyElement, App, Context, Entity, Focusable as _, Window, rems};
use mermaid_render::flowchart_model::PreservedLine;
use mermaid_render::ishikawa_model::{IshikawaCause, IshikawaDocument, is_valid_ishikawa_text};
use mermaid_render::mindmap_model::OutlineItem;
use ui::prelude::*;

use super::tree_view::{
    DEPTH_INDENT_REMS, OutlineEdit, apply_outline_edit, clamp_outline, outline_row_controls,
};
use super::{Form, LABEL_FIELD_WIDTH, MermaidEditor, add_button, field_frame, section_title};

/// The texts a cause and an effect are added with, so that a new row is
/// visible in the drawing before anything has been typed into it.
const NEW_MAIN_BONE: &str = i18n::msg!("New cause");
const NEW_CAUSE: &str = i18n::msg!("New sub-cause");

pub(super) struct IshikawaForm {
    /// The keyword as the fence spells it, which the form does not edit.
    pub(super) keyword: String,
    pub(super) preserved_lines: Vec<PreservedLine>,
    pub(super) effect: Entity<Editor>,
    pub(super) rows: Vec<IshikawaRow>,
}

pub(super) struct IshikawaRow {
    /// The cause as it was last synced from the field; only its depth is
    /// used, the text is read from the field.
    pub(super) cause: IshikawaCause,
    pub(super) text: Entity<Editor>,
}

impl OutlineItem for IshikawaRow {
    fn depth(&self) -> usize {
        self.cause.depth
    }

    fn set_depth(&mut self, depth: usize) {
        self.cause.depth = depth;
    }
}

impl IshikawaRow {
    /// `merman` trims the text of every line, so the field's whitespace is
    /// dropped rather than reported.
    fn current_cause(&self, cx: &App) -> IshikawaCause {
        IshikawaCause {
            depth: self.cause.depth,
            text: self.text.read(cx).text(cx).trim().to_string(),
        }
    }
}

impl IshikawaForm {
    pub(super) fn fields(&self) -> Vec<Entity<Editor>> {
        std::iter::once(self.effect.clone())
            .chain(self.rows.iter().map(|row| row.text.clone()))
            .collect()
    }

    /// The Ishikawa diagram the form currently describes.
    pub(super) fn current_document(&self, cx: &App) -> IshikawaDocument {
        let effect = self.effect.read(cx).text(cx).trim().to_string();
        let mut causes = self
            .rows
            .iter()
            .map(|row| row.current_cause(cx))
            .collect::<Vec<_>>();
        clamp_outline(&mut causes);
        IshikawaDocument {
            keyword: self.keyword.clone(),
            effect: (!effect.is_empty()).then_some(effect),
            causes,
            preserved_lines: self.preserved_lines.clone(),
        }
    }
}

/// The first thing wrong with what the user typed, in the words shown above the
/// form. Every check lives in `IshikawaDocument::structure_error`, which the
/// caller consults right after this; the red frames come from
/// `is_valid_ishikawa_text` directly.
pub(super) fn problem(_document: &IshikawaDocument) -> Option<SharedString> {
    None
}

impl MermaidEditor {
    pub(super) fn new_ishikawa_form(
        &mut self,
        document: IshikawaDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> IshikawaForm {
        let effect = self.new_field(document.effect.as_deref().unwrap_or_default(), window, cx);
        let rows = document
            .causes
            .into_iter()
            .map(|cause| self.new_ishikawa_row(cause, window, cx))
            .collect();
        IshikawaForm {
            keyword: document.keyword,
            preserved_lines: document.preserved_lines,
            effect,
            rows,
        }
    }

    fn new_ishikawa_row(
        &mut self,
        cause: IshikawaCause,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> IshikawaRow {
        let text = self.new_field(&cause.text, window, cx);
        IshikawaRow { cause, text }
    }

    pub(super) fn ishikawa(&self) -> Option<&IshikawaForm> {
        match &self.form {
            Form::Ishikawa(form) => Some(form),
            _ => None,
        }
    }

    pub(super) fn ishikawa_mut(&mut self) -> Option<&mut IshikawaForm> {
        match &mut self.form {
            Form::Ishikawa(form) => Some(form),
            _ => None,
        }
    }

    fn ishikawa_changed(&mut self, cx: &mut Context<Self>) {
        self.schedule_preview(cx);
        cx.notify();
    }

    /// Adds a main bone at the end of the fish.
    pub(super) fn add_ishikawa_main_bone(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ishikawa().is_none() {
            return;
        }
        let row = self.new_ishikawa_row(IshikawaCause::new(i18n::tr(NEW_MAIN_BONE)), window, cx);
        let focus = row.text.focus_handle(cx);
        if let Some(form) = self.ishikawa_mut() {
            form.rows.push(row);
        }
        focus.focus(window, cx);
        self.ishikawa_changed(cx);
    }

    pub(super) fn edit_ishikawa(
        &mut self,
        index: usize,
        edit: OutlineEdit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(depth) = self
            .ishikawa()
            .and_then(|form| form.rows.get(index))
            .map(|row| row.cause.depth)
        else {
            return;
        };
        let new_row = edit.adds_a_row().then(|| {
            let text = if edit == OutlineEdit::AddSibling && depth == 1 {
                NEW_MAIN_BONE
            } else {
                NEW_CAUSE
            };
            self.new_ishikawa_row(IshikawaCause::new(i18n::tr(text)), window, cx)
        });
        let Some(form) = self.ishikawa_mut() else {
            return;
        };
        let (changed, added) = apply_outline_edit(&mut form.rows, index, edit, new_row);
        if let Some(row) = added.and_then(|added| form.rows.get(added)) {
            row.text.focus_handle(cx).focus(window, cx);
        }
        if changed {
            self.ishikawa_changed(cx);
        }
    }

    pub(super) fn render_ishikawa(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(form) = self.ishikawa() else {
            return Vec::new();
        };
        let effect = form.effect.read(cx).text(cx);
        let effect = effect.trim();
        let effect_row = h_flex()
            .w_full()
            .gap_2()
            .child(section_title(i18n::t!("Effect (Problem)")))
            .child(
                field_frame(
                    &form.effect,
                    LABEL_FIELD_WIDTH,
                    // An empty effect is fine until there are causes to hang
                    // on it, which the reason above the form says.
                    (effect.is_empty() && form.rows.is_empty()) || is_valid_ishikawa_text(effect),
                    window,
                    cx,
                )
                .flex_1()
                .child(form.effect.clone()),
            );

        let mut list = v_flex().w_full().gap_2().child(section_title(i18n::t!(
            "Causes (the leftmost rows are the main bones; indented rows are finer causes)"
        )));
        for (index, row) in form.rows.iter().enumerate() {
            let cause = row.current_cause(cx);
            let controls = outline_row_controls(
                "mermaid-ishikawa",
                &form.rows,
                index,
                i18n::t!("Add a Finer Cause"),
                cx.listener(move |this, edit: &OutlineEdit, window, cx| {
                    this.edit_ishikawa(index, *edit, window, cx)
                }),
            );
            list = list.child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .pl(rems(
                        DEPTH_INDENT_REMS * row.cause.depth.saturating_sub(1) as f32,
                    ))
                    .child(
                        field_frame(
                            &row.text,
                            LABEL_FIELD_WIDTH,
                            is_valid_ishikawa_text(&cause.text),
                            window,
                            cx,
                        )
                        .flex_1()
                        .child(row.text.clone()),
                    )
                    .child(controls),
            );
        }
        list = list.child(add_button(
            "mermaid-ishikawa-add-main-bone",
            i18n::t!("Add Main Bone (Cause)"),
            cx.listener(|this, _, window, cx| this.add_ishikawa_main_bone(window, cx)),
        ));

        vec![effect_row.into_any_element(), list.into_any_element()]
    }

    pub(super) fn render_ishikawa_toolbar(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> AnyElement {
        div().into_any_element()
    }
}

#[cfg(test)]
#[path = "ishikawa_tests.rs"]
mod ishikawa_tests;
