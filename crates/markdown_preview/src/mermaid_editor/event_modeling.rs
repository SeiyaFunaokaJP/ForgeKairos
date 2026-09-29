//! The event model (イベントモデリング) half of the Mermaid editor's form.
//!
//! An event model is a time line of frames, each a box of one type, and the
//! arrows between them, so the form is the list of frames in drawing order,
//! the list of arrows, and the `data` blocks frames can show. Arrows and data
//! references name frames and blocks by name; they are picked from menus
//! rather than typed, and the document keeps the names every row started
//! with, so that renaming a frame or a block carries into everything that
//! names it.

use editor::Editor;
use gpui::{AnyElement, App, Context, Entity, Window};
use mermaid_render::event_modeling_model::{
    EventModelingDataBlock, EventModelingDocument, EventModelingEntityType, EventModelingFrame,
    is_valid_data_name, is_valid_data_value, is_valid_frame_label, is_valid_frame_name,
    is_valid_inline_data, is_valid_namespace,
};
use mermaid_render::flowchart_model::{PreservedLine, PreservedPosition};
use ui::{
    ContextMenu, DropdownMenu, DropdownStyle, IconButtonShape, IconPosition, Tooltip, prelude::*,
};
use util::ResultExt as _;

use super::{
    Form, ID_FIELD_WIDTH, MermaidEditor, NUMBER_FIELD_WIDTH, add_button, field_frame, neighbour,
    row_controls_labelled, section_title,
};

// Mermaid reads names as ASCII identifiers and a data block only over
// several lines, closed by a `}` at the start of its own line.
const NEW_FRAME_LABEL: &str = "NewElement";
const NEW_DATA_NAME: &str = "Data";
const NEW_DATA_VALUE: &str = "{\n  item: value\n}";

pub(super) struct EventModelingForm {
    /// The diagram as it was opened, plus the structural edits made since.
    /// Frame and block names in it are the names the rows started with, which
    /// is what the arrows and references in it are keyed on.
    pub(super) document: EventModelingDocument,
    pub(super) frames: Vec<FrameRow>,
    pub(super) data_blocks: Vec<DataBlockRow>,
}

pub(super) struct FrameRow {
    pub(super) name: Entity<Editor>,
    pub(super) namespace: Entity<Editor>,
    pub(super) label: Entity<Editor>,
    pub(super) inline_data: Entity<Editor>,
}

pub(super) struct DataBlockRow {
    pub(super) name: Entity<Editor>,
    /// `None` for a block written over several lines, which a one-line field
    /// cannot hold; it is kept as it is and edited as text.
    pub(super) value: Option<Entity<Editor>>,
}

fn field_text(field: &Entity<Editor>, cx: &App) -> String {
    field.read(cx).text(cx).trim().to_string()
}

impl EventModelingForm {
    pub(super) fn fields(&self) -> Vec<Entity<Editor>> {
        self.frames
            .iter()
            .flat_map(|row| {
                [
                    row.name.clone(),
                    row.namespace.clone(),
                    row.label.clone(),
                    row.inline_data.clone(),
                ]
            })
            .chain(
                self.data_blocks
                    .iter()
                    .flat_map(|row| std::iter::once(row.name.clone()).chain(row.value.clone())),
            )
            .collect()
    }

    /// The event model the form currently describes, with every arrow and data
    /// reference pointed at the names as they are now spelled.
    pub(super) fn current_document(&self, cx: &App) -> EventModelingDocument {
        let mut document = self.document.clone();
        let mut frame_renames: Vec<(String, String)> = Vec::new();
        for (frame, row) in document.frames.iter_mut().zip(&self.frames) {
            let name = field_text(&row.name, cx);
            if name != frame.name {
                frame_renames.push((std::mem::replace(&mut frame.name, name.clone()), name));
            }
            let namespace = field_text(&row.namespace, cx);
            frame.namespace = (!namespace.is_empty()).then_some(namespace);
            frame.label = field_text(&row.label, cx);
            let inline_data = field_text(&row.inline_data, cx);
            frame.inline_data = (!inline_data.is_empty()).then_some(inline_data);
        }
        let mut data_renames: Vec<(String, String)> = Vec::new();
        for (block, row) in document.data_blocks.iter_mut().zip(&self.data_blocks) {
            let name = field_text(&row.name, cx);
            if name != block.name {
                data_renames.push((std::mem::replace(&mut block.name, name.clone()), name));
            }
            if let Some(value) = &row.value {
                block.value = field_text(value, cx);
            }
        }
        for frame in &mut document.frames {
            for source in &mut frame.sources {
                if let Some((_, new)) = frame_renames.iter().find(|(old, _)| old == source) {
                    *source = new.clone();
                }
            }
            if let Some(reference) = &mut frame.data_reference
                && let Some((_, new)) = data_renames.iter().find(|(old, _)| old == reference)
            {
                *reference = new.clone();
            }
        }
        document
    }

    /// The frames as the menus offer them: the name each is keyed on in the
    /// document, and the name its field holds now.
    fn frame_choices(&self, cx: &App) -> Vec<(String, String)> {
        self.document
            .frames
            .iter()
            .zip(&self.frames)
            .map(|(frame, row)| (frame.name.clone(), field_text(&row.name, cx)))
            .collect()
    }

    fn data_choices(&self, cx: &App) -> Vec<(String, String)> {
        self.document
            .data_blocks
            .iter()
            .zip(&self.data_blocks)
            .map(|(block, row)| (block.name.clone(), field_text(&row.name, cx)))
            .collect()
    }
}

/// The first thing wrong with what the user typed, in the words shown above the
/// form. Everything about the names and the arrows is checked by
/// `EventModelingDocument::structure_error`, which the caller consults next.
pub(super) fn problem(document: &EventModelingDocument) -> Option<SharedString> {
    // `eventmodeling` on its own draws an empty picture, which a diagram
    // picked from the kind menu and confirmed at once should not leave behind.
    if document.frames.is_empty() {
        return Some(i18n::t!("Add at least one frame.").into());
    }
    None
}

/// A name nothing in `taken` has — neither a name a row started with nor one
/// typed since — so that it can key a new row without colliding with either.
fn unused_name(taken: &[&str], base: &str) -> String {
    std::iter::once(base.to_string())
        .chain((2..).map(|number| format!("{base}{number}")))
        .find(|name| !taken.contains(&name.as_str()))
        .unwrap_or_else(|| base.to_string())
}

/// The number after the largest frame number written in digits, padded like
/// the template's `01`.
fn next_frame_name(taken: &[&str]) -> String {
    let largest = taken
        .iter()
        .filter(|name| !name.is_empty() && name.chars().all(|character| character.is_ascii_digit()))
        .filter_map(|name| name.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    let candidate = format!("{:02}", largest.saturating_add(1));
    if taken.contains(&candidate.as_str()) {
        unused_name(taken, &candidate)
    } else {
        candidate
    }
}

fn frame_kind_label(reset: bool) -> &'static str {
    if reset {
        i18n::tc!("event modeling frame", "Reset")
    } else {
        i18n::tc!("event modeling frame", "Time")
    }
}

/// Where an arrow is kept: the frame it points into, and its place among that
/// frame's sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RelationAt {
    pub(super) target: usize,
    pub(super) position: usize,
}

/// Hands the lines written in front of a removed statement to the one that
/// took its place, or to the end of the diagram when there is none.
fn pass_on_leading_lines(
    leading: Vec<String>,
    next: Option<&mut Vec<String>>,
    preserved_lines: &mut Vec<PreservedLine>,
) {
    match next {
        Some(next) => {
            let mut lines = leading;
            lines.append(next);
            *next = lines;
        }
        None => preserved_lines.extend(leading.into_iter().map(|text| PreservedLine {
            position: PreservedPosition::Bottom,
            text,
        })),
    }
}

impl MermaidEditor {
    pub(super) fn new_event_modeling_form(
        &mut self,
        document: EventModelingDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> EventModelingForm {
        let mut frames = Vec::with_capacity(document.frames.len());
        for frame in &document.frames {
            let (name, namespace, label, inline_data) = (
                frame.name.clone(),
                frame.namespace.clone().unwrap_or_default(),
                frame.label.clone(),
                frame.inline_data.clone().unwrap_or_default(),
            );
            frames.push(FrameRow {
                name: self.new_field(&name, window, cx),
                namespace: self.new_field(&namespace, window, cx),
                label: self.new_field(&label, window, cx),
                inline_data: self.new_field(&inline_data, window, cx),
            });
        }
        let mut data_blocks = Vec::with_capacity(document.data_blocks.len());
        for block in &document.data_blocks {
            let (name, value) = (block.name.clone(), block.value.clone());
            let value = (!value.contains('\n')).then(|| self.new_field(&value, window, cx));
            data_blocks.push(DataBlockRow {
                name: self.new_field(&name, window, cx),
                value,
            });
        }
        EventModelingForm {
            document,
            frames,
            data_blocks,
        }
    }

    pub(super) fn event_modeling(&self) -> Option<&EventModelingForm> {
        match &self.form {
            Form::EventModeling(form) => Some(form),
            _ => None,
        }
    }

    pub(super) fn event_modeling_mut(&mut self) -> Option<&mut EventModelingForm> {
        match &mut self.form {
            Form::EventModeling(form) => Some(form),
            _ => None,
        }
    }

    fn event_modeling_changed(&mut self, cx: &mut Context<Self>) {
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn add_event_modeling_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.event_modeling() else {
            return;
        };
        let current = form.current_document(cx);
        let taken = form
            .document
            .frames
            .iter()
            .chain(&current.frames)
            .map(|frame| frame.name.as_str())
            .collect::<Vec<_>>();
        let name = next_frame_name(&taken);
        let row = FrameRow {
            name: self.new_field(&name, window, cx),
            namespace: self.new_field("", window, cx),
            label: self.new_field(NEW_FRAME_LABEL, window, cx),
            inline_data: self.new_field("", window, cx),
        };
        let Some(form) = self.event_modeling_mut() else {
            return;
        };
        form.document.frames.push(EventModelingFrame {
            name,
            reset: false,
            long_keyword: false,
            entity_type: EventModelingEntityType::Event,
            long_type: false,
            namespace: None,
            label: NEW_FRAME_LABEL.to_string(),
            sources: Vec::new(),
            inline_data: None,
            data_reference: None,
            leading_lines: Vec::new(),
        });
        form.frames.push(row);
        self.event_modeling_changed(cx);
    }

    /// Removes a frame together with the arrows drawn from it, which would
    /// otherwise point at nothing.
    pub(super) fn remove_event_modeling_frame(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(form) = self.event_modeling_mut() else {
            return;
        };
        if index >= form.frames.len() || index >= form.document.frames.len() {
            return;
        }
        let removed = form.document.frames.remove(index);
        form.frames.remove(index);
        for frame in &mut form.document.frames {
            frame.sources.retain(|source| source != &removed.name);
        }
        let document = &mut form.document;
        pass_on_leading_lines(
            removed.leading_lines,
            document
                .frames
                .get_mut(index)
                .map(|frame| &mut frame.leading_lines),
            &mut document.preserved_lines,
        );
        self.event_modeling_changed(cx);
    }

    pub(super) fn move_event_modeling_frame(
        &mut self,
        index: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.event_modeling_mut() else {
            return;
        };
        let Some(other) = neighbour(index, forward, form.frames.len()) else {
            return;
        };
        if other >= form.document.frames.len() || index >= form.document.frames.len() {
            return;
        }
        form.document.frames.swap(index, other);
        form.frames.swap(index, other);
        self.event_modeling_changed(cx);
    }

    pub(super) fn update_event_modeling_frame(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut EventModelingFrame),
    ) {
        let Some(form) = self.event_modeling_mut() else {
            return;
        };
        let Some(frame) = form.document.frames.get_mut(index) else {
            return;
        };
        edit(frame);
        self.event_modeling_changed(cx);
    }

    /// Adds an arrow into the last frame from the nearest frame before it that
    /// does not already draw one there — the pair a new step in a time line
    /// most often joins.
    pub(super) fn add_event_modeling_relation(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.event_modeling_mut() else {
            return;
        };
        let source = form
            .document
            .frames
            .split_last()
            .and_then(|(last, earlier)| {
                earlier
                    .iter()
                    .rev()
                    .find(|frame| !last.sources.contains(&frame.name))
                    .map(|frame| frame.name.clone())
            });
        let Some(source) = source else {
            self.message = Some(
                i18n::t!("There is no pair of frames to add an arrow between. Add a frame first.")
                    .into(),
            );
            cx.notify();
            return;
        };
        if let Some(target) = form.document.frames.last_mut() {
            target.sources.push(source);
        }
        self.event_modeling_changed(cx);
    }

    pub(super) fn remove_event_modeling_relation(
        &mut self,
        at: RelationAt,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.event_modeling_mut() else {
            return;
        };
        let Some(frame) = form.document.frames.get_mut(at.target) else {
            return;
        };
        if at.position >= frame.sources.len() {
            return;
        }
        frame.sources.remove(at.position);
        self.event_modeling_changed(cx);
    }

    pub(super) fn set_event_modeling_relation_source(
        &mut self,
        at: RelationAt,
        source: String,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.event_modeling_mut() else {
            return;
        };
        let Some(slot) = form
            .document
            .frames
            .get_mut(at.target)
            .and_then(|frame| frame.sources.get_mut(at.position))
        else {
            return;
        };
        *slot = source;
        self.event_modeling_changed(cx);
    }

    /// Points an arrow into another frame. It joins the end of that frame's
    /// sources, since the order of a frame's sources draws nothing.
    pub(super) fn set_event_modeling_relation_target(
        &mut self,
        at: RelationAt,
        target: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.event_modeling_mut() else {
            return;
        };
        if target == at.target || target >= form.document.frames.len() {
            return;
        }
        let Some(frame) = form.document.frames.get_mut(at.target) else {
            return;
        };
        if at.position >= frame.sources.len() {
            return;
        }
        let source = frame.sources.remove(at.position);
        if let Some(frame) = form.document.frames.get_mut(target) {
            frame.sources.push(source);
        }
        self.event_modeling_changed(cx);
    }

    pub(super) fn add_event_modeling_data(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.event_modeling() else {
            return;
        };
        let current = form.current_document(cx);
        let taken = form
            .document
            .data_blocks
            .iter()
            .chain(&current.data_blocks)
            .map(|block| block.name.as_str())
            .collect::<Vec<_>>();
        let name = unused_name(&taken, NEW_DATA_NAME);
        let row = DataBlockRow {
            name: self.new_field(&name, window, cx),
            // A block over several lines has no field; it is edited as text.
            value: None,
        };
        let Some(form) = self.event_modeling_mut() else {
            return;
        };
        form.document.data_blocks.push(EventModelingDataBlock {
            name,
            value: NEW_DATA_VALUE.to_string(),
            leading_lines: Vec::new(),
        });
        form.data_blocks.push(row);
        self.event_modeling_changed(cx);
    }

    /// Removes a block, and with it every frame's reference to it, which
    /// `merman` would otherwise draw as a box without its data.
    pub(super) fn remove_event_modeling_data(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(form) = self.event_modeling_mut() else {
            return;
        };
        if index >= form.data_blocks.len() || index >= form.document.data_blocks.len() {
            return;
        }
        let removed = form.document.data_blocks.remove(index);
        form.data_blocks.remove(index);
        for frame in &mut form.document.frames {
            if frame.data_reference.as_ref() == Some(&removed.name) {
                frame.data_reference = None;
            }
        }
        let document = &mut form.document;
        pass_on_leading_lines(
            removed.leading_lines,
            document
                .data_blocks
                .get_mut(index)
                .map(|block| &mut block.leading_lines),
            &mut document.preserved_lines,
        );
        self.event_modeling_changed(cx);
    }

    pub(super) fn move_event_modeling_data(
        &mut self,
        index: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.event_modeling_mut() else {
            return;
        };
        let Some(other) = neighbour(index, forward, form.data_blocks.len()) else {
            return;
        };
        if other >= form.document.data_blocks.len() || index >= form.document.data_blocks.len() {
            return;
        }
        form.document.data_blocks.swap(index, other);
        form.data_blocks.swap(index, other);
        self.event_modeling_changed(cx);
    }

    pub(super) fn render_event_modeling(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(form) = self.event_modeling() else {
            return Vec::new();
        };
        vec![
            self.render_event_modeling_frames(form, window, cx),
            self.render_event_modeling_relations(form, window, cx),
            self.render_event_modeling_data(form, window, cx),
        ]
    }

    pub(super) fn render_event_modeling_toolbar(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> AnyElement {
        div().into_any_element()
    }

    fn render_event_modeling_frames(
        &self,
        form: &EventModelingForm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let last = form.frames.len().saturating_sub(1);
        let names = form
            .frames
            .iter()
            .map(|row| field_text(&row.name, cx))
            .collect::<Vec<_>>();
        let data_choices = form.data_choices(cx);
        let mut section = v_flex()
            .w_full()
            .gap_2()
            .child(section_title(i18n::t!("Frames (in time order from the top)")))
            .child(
                Label::new(i18n::t!(
                    "Number, separator, type, lane (optional), name. The second line is data in {{ }}."
                ))
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            );
        for index in 0..form.frames.len() {
            let (Some(row), Some(frame)) =
                (form.frames.get(index), form.document.frames.get(index))
            else {
                continue;
            };
            let name = names.get(index).cloned().unwrap_or_default();
            let name_is_valid = is_valid_frame_name(&name)
                && names.iter().filter(|other| **other == name).count() == 1;
            let namespace = field_text(&row.namespace, cx);
            let namespace_is_valid = namespace.is_empty() || is_valid_namespace(&namespace);
            let label = field_text(&row.label, cx);
            let label_is_valid = if namespace.is_empty() {
                is_valid_frame_label(&label)
            } else {
                !label.is_empty()
                    && !label.contains(char::is_whitespace)
                    && !label.contains(['.'])
                    && !label.contains("%%")
            };
            let inline_data = field_text(&row.inline_data, cx);
            let inline_is_valid = inline_data.is_empty()
                || (is_valid_inline_data(&inline_data) && frame.data_reference.is_none());

            let first_line = h_flex()
                .w_full()
                .gap_1()
                .child(
                    field_frame(&row.name, NUMBER_FIELD_WIDTH, name_is_valid, window, cx)
                        .child(row.name.clone()),
                )
                .child(self.render_event_modeling_kind_menu(index, frame.reset, window, cx))
                .child(self.render_event_modeling_type_menu(index, frame.entity_type, window, cx))
                .child(
                    field_frame(
                        &row.namespace,
                        ID_FIELD_WIDTH,
                        namespace_is_valid,
                        window,
                        cx,
                    )
                    .child(row.namespace.clone()),
                )
                .child(
                    field_frame(&row.label, ID_FIELD_WIDTH, label_is_valid, window, cx)
                        .flex_1()
                        .child(row.label.clone()),
                )
                .child(row_controls_labelled(
                    "mermaid-em-frame",
                    index,
                    index == 0,
                    index >= last,
                    i18n::t!("Delete (arrows from this frame are deleted too)"),
                    cx.listener(move |this, _, _window, cx| {
                        this.move_event_modeling_frame(index, false, cx)
                    }),
                    cx.listener(move |this, _, _window, cx| {
                        this.move_event_modeling_frame(index, true, cx)
                    }),
                    cx.listener(move |this, _, _window, cx| {
                        this.remove_event_modeling_frame(index, cx)
                    }),
                ));
            let second_line = h_flex()
                .w_full()
                .gap_1()
                .pl_4()
                .child(
                    Label::new(i18n::t!("Data"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    field_frame(
                        &row.inline_data,
                        ID_FIELD_WIDTH,
                        inline_is_valid,
                        window,
                        cx,
                    )
                    .flex_1()
                    .child(row.inline_data.clone()),
                )
                .child(self.render_event_modeling_reference_menu(
                    index,
                    frame.data_reference.as_deref(),
                    &data_choices,
                    window,
                    cx,
                ));
            section = section.child(
                v_flex()
                    .w_full()
                    .gap_1()
                    .child(first_line)
                    .child(second_line),
            );
        }
        section
            .child(add_button(
                "mermaid-em-add-frame",
                i18n::t!("Add Frame"),
                cx.listener(|this, _, window, cx| this.add_event_modeling_frame(window, cx)),
            ))
            .into_any_element()
    }

    fn render_event_modeling_kind_menu(
        &self,
        index: usize,
        reset: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for (choice, description) in [
                (
                    false,
                    i18n::t!("Time (tf): an arrow continues from the previous frame"),
                ),
                (
                    true,
                    i18n::t!("Reset (rf): takes no arrow and starts a new flow"),
                ),
            ] {
                let this = this.clone();
                menu = menu.toggleable_entry(
                    description,
                    choice == reset,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        this.update(cx, |this, cx| {
                            this.update_event_modeling_frame(index, cx, |frame| {
                                frame.reset = choice;
                            })
                        })
                        .log_err();
                    },
                );
            }
            menu
        });
        DropdownMenu::new(("mermaid-em-kind", index), frame_kind_label(reset), menu)
            .style(DropdownStyle::Outlined)
            .trigger_size(ButtonSize::Compact)
            .into_any_element()
    }

    fn render_event_modeling_type_menu(
        &self,
        index: usize,
        current: EventModelingEntityType,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for entity_type in EventModelingEntityType::ALL {
                let this = this.clone();
                menu = menu.toggleable_entry(
                    entity_type.label(),
                    entity_type == current,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        this.update(cx, |this, cx| {
                            this.update_event_modeling_frame(index, cx, |frame| {
                                if frame.entity_type != entity_type {
                                    frame.entity_type = entity_type;
                                    frame.long_type = false;
                                }
                            })
                        })
                        .log_err();
                    },
                );
            }
            menu
        });
        DropdownMenu::new(("mermaid-em-type", index), current.label(), menu)
            .style(DropdownStyle::Outlined)
            .trigger_size(ButtonSize::Compact)
            .into_any_element()
    }

    fn render_event_modeling_reference_menu(
        &self,
        index: usize,
        current: Option<&str>,
        choices: &[(String, String)],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = match current {
            Some(key) => {
                let shown = choices
                    .iter()
                    .find(|(choice, _)| choice == key)
                    .map_or(key, |(_, shown)| shown.as_str());
                i18n::t!("Reference: {shown}")
            }
            None => i18n::t!("No Reference").to_string(),
        };
        let this = cx.entity().downgrade();
        let selected = current.map(str::to_string);
        let choices = choices.to_vec();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            let entries = std::iter::once((None, i18n::t!("No Reference").to_string())).chain(
                choices
                    .iter()
                    .map(|(key, shown)| (Some(key.clone()), shown.clone())),
            );
            for (key, shown) in entries {
                let this = this.clone();
                let is_selected = key == selected;
                menu = menu.toggleable_entry(
                    shown,
                    is_selected,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        let key = key.clone();
                        this.update(cx, |this, cx| {
                            this.update_event_modeling_frame(index, cx, move |frame| {
                                frame.data_reference = key;
                            })
                        })
                        .log_err();
                    },
                );
            }
            menu
        });
        DropdownMenu::new(("mermaid-em-reference", index), label, menu)
            .style(DropdownStyle::Outlined)
            .trigger_size(ButtonSize::Compact)
            .into_any_element()
    }

    fn render_event_modeling_relations(
        &self,
        form: &EventModelingForm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let choices = form.frame_choices(cx);
        let mut section = v_flex()
            .w_full()
            .gap_1()
            .child(section_title(i18n::t!("Arrows")))
            .child(
            Label::new(i18n::t!(
                "A time frame with no arrow gets one automatically from the closest earlier frame in another lane"
            ))
            .size(LabelSize::XSmall)
            .color(Color::Muted),
        );
        let mut row_index = 0;
        for (target, frame) in form.document.frames.iter().enumerate() {
            for (position, source) in frame.sources.iter().enumerate() {
                let at = RelationAt { target, position };
                section = section.child(
                    h_flex()
                        .w_full()
                        .gap_1()
                        .child(self.render_event_modeling_endpoint_menu(
                            row_index, at, true, source, &choices, window, cx,
                        ))
                        .child(Label::new("→").size(LabelSize::Small).color(Color::Muted))
                        .child(self.render_event_modeling_endpoint_menu(
                            row_index,
                            at,
                            false,
                            &frame.name,
                            &choices,
                            window,
                            cx,
                        ))
                        .child(div().flex_1())
                        .child(
                            IconButton::new(
                                ("mermaid-em-relation-remove", row_index),
                                IconName::Trash,
                            )
                            .shape(IconButtonShape::Square)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text(i18n::t!("Delete")))
                            .on_click(cx.listener(
                                move |this, _, _window, cx| {
                                    this.remove_event_modeling_relation(at, cx)
                                },
                            )),
                        ),
                );
                row_index += 1;
            }
        }
        section
            .child(add_button(
                "mermaid-em-add-relation",
                i18n::t!("Add Arrow"),
                cx.listener(|this, _, _window, cx| this.add_event_modeling_relation(cx)),
            ))
            .into_any_element()
    }

    /// One end of an arrow. The menu shows each frame's name as it is typed now
    /// and stores the name the frame is keyed on.
    #[allow(clippy::too_many_arguments)]
    fn render_event_modeling_endpoint_menu(
        &self,
        row_index: usize,
        at: RelationAt,
        is_source: bool,
        current_key: &str,
        choices: &[(String, String)],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let shown = choices
            .iter()
            .find(|(key, _)| key == current_key)
            .map_or(current_key, |(_, shown)| shown.as_str())
            .to_string();
        let this = cx.entity().downgrade();
        let selected = current_key.to_string();
        let choices = choices.to_vec();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for (frame_index, (key, shown)) in choices.iter().enumerate() {
                let this = this.clone();
                let key = key.clone();
                menu = menu.toggleable_entry(
                    shown.clone(),
                    key == selected,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        let key = key.clone();
                        this.update(cx, |this, cx| {
                            if is_source {
                                this.set_event_modeling_relation_source(at, key, cx);
                            } else {
                                this.set_event_modeling_relation_target(at, frame_index, cx);
                            }
                        })
                        .log_err();
                    },
                );
            }
            menu
        });
        DropdownMenu::new(
            (
                "mermaid-em-relation-end",
                row_index * 2 + usize::from(is_source),
            ),
            SharedString::from(shown),
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
    }

    fn render_event_modeling_data(
        &self,
        form: &EventModelingForm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let last = form.data_blocks.len().saturating_sub(1);
        let names = form
            .data_blocks
            .iter()
            .map(|row| field_text(&row.name, cx))
            .collect::<Vec<_>>();
        let mut section = v_flex()
            .w_full()
            .gap_1()
            .child(section_title(i18n::t!("Data (referenced from frames)")));
        for index in 0..form.data_blocks.len() {
            let (Some(row), Some(block)) = (
                form.data_blocks.get(index),
                form.document.data_blocks.get(index),
            ) else {
                continue;
            };
            let name = names.get(index).cloned().unwrap_or_default();
            let name_is_valid = is_valid_data_name(&name)
                && names.iter().filter(|other| **other == name).count() == 1;
            let value = match &row.value {
                Some(field) => {
                    let is_valid = is_valid_data_value(&field_text(field, cx));
                    field_frame(field, ID_FIELD_WIDTH, is_valid, window, cx)
                        .flex_1()
                        .child(field.clone())
                        .into_any_element()
                }
                None => h_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        Label::new(i18n::tn!(
                            block.value.lines().count(),
                            "{count} line of data (edit multiple lines as text)",
                            "{count} lines of data (edit multiple lines as text)"
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .into_any_element(),
            };
            section = section.child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        field_frame(&row.name, ID_FIELD_WIDTH, name_is_valid, window, cx)
                            .child(row.name.clone()),
                    )
                    .child(value)
                    .child(row_controls_labelled(
                        "mermaid-em-data",
                        index,
                        index == 0,
                        index >= last,
                        i18n::t!("Delete (references to this data are removed too)"),
                        cx.listener(move |this, _, _window, cx| {
                            this.move_event_modeling_data(index, false, cx)
                        }),
                        cx.listener(move |this, _, _window, cx| {
                            this.move_event_modeling_data(index, true, cx)
                        }),
                        cx.listener(move |this, _, _window, cx| {
                            this.remove_event_modeling_data(index, cx)
                        }),
                    )),
            );
        }
        section
            .child(add_button(
                "mermaid-em-add-data",
                i18n::t!("Add Data"),
                cx.listener(|this, _, window, cx| this.add_event_modeling_data(window, cx)),
            ))
            .into_any_element()
    }
}

#[cfg(test)]
#[path = "event_modeling_tests.rs"]
mod event_modeling_tests;
