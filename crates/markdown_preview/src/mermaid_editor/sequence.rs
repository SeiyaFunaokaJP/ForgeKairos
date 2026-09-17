//! The sequence diagram half of the Mermaid editor's form.
//!
//! A sequence diagram has two halves that cannot be merged: the participants,
//! whose declaration order is the left-to-right column order, and everything
//! that happens, which is one ordered list — a message, a note, the start of a
//! block, its branch, its `end` — because a message that changed places with
//! the `end` above it would be read as happening inside the block.
//!
//! `box` groupings are shown but not edited here: a box wraps a *run* of
//! declarations, so moving one column out of the middle of a box cannot be
//! expressed without moving the others too, and
//! [`SequenceDocument::structure_error`] refuses that rather than guessing.

use editor::Editor;
use gpui::{AnyElement, App, Context, Entity, Window};
use mermaid_render::sequence_model::{
    SequenceArrow, SequenceAutonumber, SequenceBlock, SequenceBlockKind, SequenceBranch,
    SequenceBranchKind, SequenceDocument, SequenceItem, SequenceMessage, SequenceNote,
    SequenceNotePlacement, SequenceParticipant, SequenceParticipantKind, is_valid_participant_id,
};
use ui::{
    Checkbox, ContextMenu, DropdownMenu, DropdownStyle, IconPosition, ToggleState, prelude::*,
};
use util::ResultExt as _;

use super::{
    Form, ID_FIELD_WIDTH, LABEL_FIELD_WIDTH, MermaidEditor, NUMBER_FIELD_WIDTH, add_button,
    field_frame, neighbour, row_controls, section_title,
};

pub(super) struct SequenceForm {
    pub(super) document: SequenceDocument,
    pub(super) title: Entity<Editor>,
    pub(super) participants: Vec<ParticipantRow>,
    pub(super) items: Vec<ItemRow>,
}

pub(super) struct ParticipantRow {
    pub(super) id: Entity<Editor>,
    pub(super) alias: Entity<Editor>,
}

pub(super) enum ItemRow {
    /// A message, a note, a block start or a block branch: one line of text.
    Text(Entity<Editor>),
    Autonumber {
        start: Entity<Editor>,
        step: Entity<Editor>,
    },
    /// `end`, `activate` and `deactivate` have nothing to type into.
    Fixed,
}

impl SequenceForm {
    pub(super) fn fields(&self) -> Vec<Entity<Editor>> {
        std::iter::once(self.title.clone())
            .chain(
                self.participants
                    .iter()
                    .flat_map(|row| [row.id.clone(), row.alias.clone()]),
            )
            .chain(self.items.iter().flat_map(|row| match row {
                ItemRow::Text(field) => vec![field.clone()],
                ItemRow::Autonumber { start, step } => vec![start.clone(), step.clone()],
                ItemRow::Fixed => Vec::new(),
            }))
            .collect()
    }

    /// The diagram the form currently describes, with every message and note
    /// pointed at the participant ids as they are now spelled.
    pub(super) fn current_document(&self, cx: &App) -> SequenceDocument {
        let mut document = self.document.clone();
        let title = self.title.read(cx).text(cx);
        document.title = if title.is_empty() {
            document.title.take().filter(|title| title.is_empty())
        } else {
            Some(title)
        };

        let mut renames: Vec<(String, String)> = Vec::new();
        for (participant, row) in document.participants.iter_mut().zip(&self.participants) {
            let id = row.id.read(cx).text(cx).trim().to_string();
            if id != participant.id {
                renames.push((std::mem::replace(&mut participant.id, id.clone()), id));
            }
            let alias = row.alias.read(cx).text(cx);
            participant.alias = (!alias.is_empty()).then_some(alias);
        }

        for (item, row) in document.items.iter_mut().zip(&self.items) {
            match (item, row) {
                (SequenceItem::Message(message), ItemRow::Text(field)) => {
                    message.text = field.read(cx).text(cx);
                }
                (SequenceItem::Note(note), ItemRow::Text(field)) => {
                    note.text = field.read(cx).text(cx);
                }
                (SequenceItem::BlockStart(block), ItemRow::Text(field)) => {
                    block.label = field.read(cx).text(cx);
                }
                (SequenceItem::BlockBranch(branch), ItemRow::Text(field)) => {
                    branch.label = field.read(cx).text(cx);
                }
                (SequenceItem::Autonumber(autonumber), ItemRow::Autonumber { start, step }) => {
                    autonumber.start = parse_number(&start.read(cx).text(cx));
                    autonumber.step = parse_number(&step.read(cx).text(cx));
                }
                _ => {}
            }
        }

        if !renames.is_empty() {
            let renamed = |id: &str| {
                renames
                    .iter()
                    .find(|(old, _)| old == id)
                    .map(|(_, new)| new.clone())
            };
            for item in &mut document.items {
                match item {
                    SequenceItem::Message(message) => {
                        if let Some(id) = renamed(&message.from) {
                            message.from = id;
                        }
                        if let Some(id) = renamed(&message.to) {
                            message.to = id;
                        }
                    }
                    SequenceItem::Note(note) => {
                        if let Some(id) = renamed(&note.from) {
                            note.from = id;
                        }
                        if let Some(id) = renamed(&note.to) {
                            note.to = id;
                        }
                    }
                    SequenceItem::Activate(id) | SequenceItem::Deactivate(id) => {
                        if let Some(new) = renamed(id) {
                            *id = new;
                        }
                    }
                    SequenceItem::BlockStart(_)
                    | SequenceItem::BlockBranch(_)
                    | SequenceItem::BlockEnd
                    | SequenceItem::Autonumber(_) => {}
                }
            }
        }
        document
    }
}

/// `autonumber` counts with a plain decimal; an empty field means the statement
/// leaves that number out.
pub(super) fn parse_number(text: &str) -> Option<f64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    text.parse::<f64>().ok().filter(|value| value.is_finite())
}

/// The first thing wrong with what the user typed. The structural rules — a
/// message naming a participant that is not declared, an unclosed block — come
/// from [`SequenceDocument::structure_error`] instead.
pub(super) fn problem(document: &SequenceDocument) -> Option<SharedString> {
    // `sequenceDiagram` with no column is not a diagram Mermaid draws, so a
    // sequence chosen from the kind picker and confirmed straight away would
    // leave a fence the preview can only report an error about.
    if document.participants.is_empty() {
        return Some("参加者を 1 つ以上追加してください。".into());
    }
    for participant in &document.participants {
        if participant.id.is_empty() {
            return Some("ID が空の参加者があります。".into());
        }
        if !is_valid_participant_id(&participant.id) {
            return Some(
                format!(
                    "ID「{}」は使えません (記号と先頭の数字は不可)。",
                    participant.id
                )
                .into(),
            );
        }
    }
    None
}

/// How deeply each item sits inside blocks, so that the list can be drawn with
/// the nesting the diagram has. An `end` is drawn at the depth of the block it
/// closes.
pub(super) fn item_depths(items: &[SequenceItem]) -> Vec<usize> {
    let mut depths = Vec::with_capacity(items.len());
    let mut depth = 0usize;
    for item in items {
        match item {
            SequenceItem::BlockStart(_) => {
                depths.push(depth);
                depth += 1;
            }
            SequenceItem::BlockBranch(_) => depths.push(depth.saturating_sub(1)),
            SequenceItem::BlockEnd => {
                depth = depth.saturating_sub(1);
                depths.push(depth);
            }
            _ => depths.push(depth),
        }
    }
    depths
}

/// The branch keywords any block offers, which is what the branch row's menu
/// lists. The block a branch sits in decides which one is legal, and
/// [`SequenceDocument::structure_error`] says so when it is not.
pub(super) fn branch_kinds() -> Vec<SequenceBranchKind> {
    let mut kinds = Vec::new();
    for block in SequenceBlockKind::ALL {
        if let Some(branch) = block.branch()
            && !kinds.contains(&branch)
        {
            kinds.push(branch);
        }
    }
    kinds
}

impl MermaidEditor {
    pub(super) fn new_sequence_form(
        &mut self,
        document: SequenceDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> SequenceForm {
        let title = self.new_field(
            document.title.clone().unwrap_or_default().as_str(),
            window,
            cx,
        );
        let mut participants = Vec::with_capacity(document.participants.len());
        for participant in &document.participants {
            let (id, alias) = (
                participant.id.clone(),
                participant.alias.clone().unwrap_or_default(),
            );
            participants.push(ParticipantRow {
                id: self.new_field(&id, window, cx),
                alias: self.new_field(&alias, window, cx),
            });
        }
        let mut items = Vec::with_capacity(document.items.len());
        for item in &document.items {
            items.push(self.new_sequence_row(item, window, cx));
        }
        SequenceForm {
            document,
            title,
            participants,
            items,
        }
    }

    pub(super) fn new_sequence_row(
        &mut self,
        item: &SequenceItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ItemRow {
        match item {
            SequenceItem::Message(message) => {
                let text = message.text.clone();
                ItemRow::Text(self.new_field(&text, window, cx))
            }
            SequenceItem::Note(note) => {
                let text = note.text.clone();
                ItemRow::Text(self.new_field(&text, window, cx))
            }
            SequenceItem::BlockStart(block) => {
                let label = block.label.clone();
                ItemRow::Text(self.new_field(&label, window, cx))
            }
            SequenceItem::BlockBranch(branch) => {
                let label = branch.label.clone();
                ItemRow::Text(self.new_field(&label, window, cx))
            }
            SequenceItem::Autonumber(autonumber) => {
                let start = number_text(autonumber.start);
                let step = number_text(autonumber.step);
                ItemRow::Autonumber {
                    start: self.new_field(&start, window, cx),
                    step: self.new_field(&step, window, cx),
                }
            }
            SequenceItem::BlockEnd | SequenceItem::Activate(_) | SequenceItem::Deactivate(_) => {
                ItemRow::Fixed
            }
        }
    }

    pub(super) fn sequence(&self) -> Option<&SequenceForm> {
        match &self.form {
            Form::Sequence(form) => Some(form),
            _ => None,
        }
    }

    pub(super) fn sequence_mut(&mut self) -> Option<&mut SequenceForm> {
        match &mut self.form {
            Form::Sequence(form) => Some(form),
            _ => None,
        }
    }

    pub(super) fn add_participant(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.sequence() else {
            return;
        };
        let id = form.current_document(cx).unused_participant_id("参加者");
        let row = ParticipantRow {
            id: self.new_field(&id, window, cx),
            alias: self.new_field("", window, cx),
        };
        let Some(form) = self.sequence_mut() else {
            return;
        };
        form.document.participants.push(SequenceParticipant {
            id,
            alias: None,
            kind: SequenceParticipantKind::Participant,
            wrap: false,
            box_index: None,
        });
        form.participants.push(row);
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn remove_participant(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(form) = self.sequence_mut() else {
            return;
        };
        if index >= form.participants.len() || index >= form.document.participants.len() {
            return;
        }
        let removed = form.document.participants.remove(index).id;
        form.participants.remove(index);
        // A message or a note left pointing at the column would declare the
        // participant again, bringing the column back where it was deleted.
        let orphaned = form
            .document
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| names_participant(item, &removed))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        for index in orphaned.into_iter().rev() {
            form.document.items.remove(index);
            if index < form.items.len() {
                form.items.remove(index);
            }
        }
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn move_participant(&mut self, index: usize, forward: bool, cx: &mut Context<Self>) {
        let Some(form) = self.sequence_mut() else {
            return;
        };
        let Some(other) = neighbour(index, forward, form.participants.len()) else {
            return;
        };
        if other >= form.document.participants.len() || index >= form.document.participants.len() {
            return;
        }
        form.document.participants.swap(index, other);
        form.participants.swap(index, other);
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn set_participant_kind(
        &mut self,
        index: usize,
        kind: SequenceParticipantKind,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.sequence_mut() else {
            return;
        };
        let Some(participant) = form.document.participants.get_mut(index) else {
            return;
        };
        participant.kind = kind;
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn add_sequence_item(
        &mut self,
        kind: AddKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.sequence() else {
            return;
        };
        let document = form.current_document(cx);
        let first = document.participants.first().map(|one| one.id.clone());
        let item = match kind {
            AddKind::Message | AddKind::Note | AddKind::Activate | AddKind::Deactivate => {
                let Some(first) = first else {
                    self.message = Some("先に参加者を追加してください。".into());
                    cx.notify();
                    return;
                };
                let second = document
                    .participants
                    .get(1)
                    .map(|one| one.id.clone())
                    .unwrap_or_else(|| first.clone());
                match kind {
                    AddKind::Message => SequenceItem::Message(SequenceMessage {
                        from: first,
                        to: second,
                        arrow: SequenceArrow::Solid,
                        activate: false,
                        deactivate: false,
                        text: String::new(),
                        wrap: false,
                    }),
                    AddKind::Note => SequenceItem::Note(SequenceNote {
                        placement: SequenceNotePlacement::RightOf,
                        from: first.clone(),
                        to: first,
                        text: "メモ".to_string(),
                        wrap: false,
                    }),
                    AddKind::Activate => SequenceItem::Activate(first),
                    _ => SequenceItem::Deactivate(first),
                }
            }
            AddKind::BlockStart => SequenceItem::BlockStart(SequenceBlock {
                kind: SequenceBlockKind::Loop,
                label: String::new(),
                wrap: false,
            }),
            AddKind::BlockBranch => SequenceItem::BlockBranch(SequenceBranch {
                kind: SequenceBranchKind::Else,
                label: String::new(),
                wrap: false,
            }),
            AddKind::BlockEnd => SequenceItem::BlockEnd,
            AddKind::Autonumber => SequenceItem::Autonumber(SequenceAutonumber {
                start: None,
                step: None,
                visible: true,
            }),
        };
        let row = self.new_sequence_row(&item, window, cx);
        let Some(form) = self.sequence_mut() else {
            return;
        };
        form.document.items.push(item);
        form.items.push(row);
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn remove_sequence_item(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(form) = self.sequence_mut() else {
            return;
        };
        if index >= form.document.items.len() || index >= form.items.len() {
            return;
        }
        form.document.items.remove(index);
        form.items.remove(index);
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn move_sequence_item(
        &mut self,
        index: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.sequence_mut() else {
            return;
        };
        let Some(other) = neighbour(index, forward, form.document.items.len()) else {
            return;
        };
        if other >= form.items.len() || index >= form.items.len() {
            return;
        }
        form.document.items.swap(index, other);
        form.items.swap(index, other);
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn update_sequence_item(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut SequenceItem),
    ) {
        let Some(form) = self.sequence_mut() else {
            return;
        };
        let Some(item) = form.document.items.get_mut(index) else {
            return;
        };
        edit(item);
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn render_sequence(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(form) = self.sequence() else {
            return Vec::new();
        };
        let ids = form
            .participants
            .iter()
            .map(|row| row.id.read(cx).text(cx).trim().to_string())
            .collect::<Vec<_>>();

        let title = h_flex()
            .w_full()
            .gap_2()
            .child(section_title("タイトル"))
            .child(
                field_frame(&form.title, LABEL_FIELD_WIDTH, true, window, cx)
                    .flex_1()
                    .child(form.title.clone()),
            )
            .into_any_element();

        vec![
            title,
            self.render_participants(form, window, cx),
            self.render_sequence_items(form, &ids, window, cx),
        ]
    }

    pub(super) fn render_participants(
        &self,
        form: &SequenceForm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let last = form.participants.len().saturating_sub(1);
        let mut section = v_flex()
            .w_full()
            .gap_1()
            .child(section_title("参加者 (上から順に左の列)"));
        for index in 0..form.participants.len() {
            let (Some(row), Some(participant)) = (
                form.participants.get(index),
                form.document.participants.get(index),
            ) else {
                continue;
            };
            let box_name = participant
                .box_index
                .and_then(|box_index| form.document.boxes.get(box_index))
                .map(|group| group.name.clone().unwrap_or_default());
            section = section.child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        field_frame(
                            &row.id,
                            ID_FIELD_WIDTH,
                            is_valid_participant_id(row.id.read(cx).text(cx).trim()),
                            window,
                            cx,
                        )
                        .child(row.id.clone()),
                    )
                    .child(
                        field_frame(&row.alias, LABEL_FIELD_WIDTH, true, window, cx)
                            .flex_1()
                            .child(row.alias.clone()),
                    )
                    .child(self.render_participant_kind(index, participant.kind, window, cx))
                    .children(box_name.map(|name| {
                        Label::new(format!("box: {name}"))
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                    }))
                    .child(row_controls(
                        "mermaid-sequence-participant",
                        index,
                        index == 0,
                        index >= last,
                        cx.listener(move |this, _, _window, cx| {
                            this.move_participant(index, false, cx)
                        }),
                        cx.listener(move |this, _, _window, cx| {
                            this.move_participant(index, true, cx)
                        }),
                        cx.listener(move |this, _, _window, cx| this.remove_participant(index, cx)),
                    )),
            );
        }
        section
            .child(add_button(
                "mermaid-sequence-add-participant",
                "参加者を追加",
                cx.listener(|this, _, window, cx| this.add_participant(window, cx)),
            ))
            .into_any_element()
    }

    pub(super) fn render_sequence_items(
        &self,
        form: &SequenceForm,
        ids: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let depths = item_depths(&form.document.items);
        let last = form.document.items.len().saturating_sub(1);
        let mut section = v_flex().w_full().gap_1().child(section_title("やりとり"));

        for index in 0..form.document.items.len() {
            let Some(item) = form.document.items.get(index) else {
                continue;
            };
            let row = form.items.get(index);
            let controls = row_controls(
                "mermaid-sequence-item",
                index,
                index == 0,
                index >= last,
                cx.listener(move |this, _, _window, cx| this.move_sequence_item(index, false, cx)),
                cx.listener(move |this, _, _window, cx| this.move_sequence_item(index, true, cx)),
                cx.listener(move |this, _, _window, cx| this.remove_sequence_item(index, cx)),
            );
            let body = match (item, row) {
                (SequenceItem::Message(message), Some(ItemRow::Text(field))) => h_flex()
                    .w_full()
                    .gap_1()
                    .child(self.render_message_endpoint(
                        index,
                        true,
                        &message.from,
                        ids,
                        window,
                        cx,
                    ))
                    .child(self.render_arrow(index, message.arrow, window, cx))
                    .child(self.render_message_endpoint(index, false, &message.to, ids, window, cx))
                    .child(
                        field_frame(field, LABEL_FIELD_WIDTH, true, window, cx)
                            .flex_1()
                            .child(field.clone()),
                    )
                    .child(
                        Checkbox::new(
                            ("mermaid-sequence-activate", index),
                            message.activate.into(),
                        )
                        .label("開始")
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(
                            move |this, state: &ToggleState, _window, cx| {
                                let activate = *state == ToggleState::Selected;
                                this.update_sequence_item(index, cx, move |item| {
                                    if let SequenceItem::Message(message) = item {
                                        message.activate = activate;
                                    }
                                });
                            },
                        )),
                    )
                    .child(
                        Checkbox::new(
                            ("mermaid-sequence-deactivate", index),
                            message.deactivate.into(),
                        )
                        .label("終了")
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(
                            move |this, state: &ToggleState, _window, cx| {
                                let deactivate = *state == ToggleState::Selected;
                                this.update_sequence_item(index, cx, move |item| {
                                    if let SequenceItem::Message(message) = item {
                                        message.deactivate = deactivate;
                                    }
                                });
                            },
                        )),
                    ),
                (SequenceItem::Note(note), Some(ItemRow::Text(field))) => {
                    let spans = note.placement == SequenceNotePlacement::Over;
                    h_flex()
                        .w_full()
                        .gap_1()
                        .child(
                            Label::new("ノート")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(self.render_note_side(index, note.placement, window, cx))
                        .child(self.render_note_end(index, true, &note.from, ids, window, cx))
                        // `Note left of` / `right of` sit at one column, so the
                        // second end is only offered for a note that spans.
                        .when(spans, |this| {
                            this.child(
                                self.render_note_end(index, false, &note.to, ids, window, cx),
                            )
                        })
                        .child(
                            field_frame(field, LABEL_FIELD_WIDTH, true, window, cx)
                                .flex_1()
                                .child(field.clone()),
                        )
                }
                (SequenceItem::BlockStart(block), Some(ItemRow::Text(field))) => h_flex()
                    .w_full()
                    .gap_1()
                    .child(self.render_block_kind(index, block.kind, window, cx))
                    .child(
                        field_frame(field, LABEL_FIELD_WIDTH, true, window, cx)
                            .flex_1()
                            .child(field.clone()),
                    ),
                (SequenceItem::BlockBranch(branch), Some(ItemRow::Text(field))) => h_flex()
                    .w_full()
                    .gap_1()
                    .child(self.render_branch_kind(index, branch.kind, window, cx))
                    .child(
                        field_frame(field, LABEL_FIELD_WIDTH, true, window, cx)
                            .flex_1()
                            .child(field.clone()),
                    ),
                (SequenceItem::BlockEnd, _) => h_flex().w_full().gap_1().child(
                    Label::new("end (ブロックの終わり)")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
                (SequenceItem::Activate(id), _) => h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        Label::new("活性化")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(self.render_lifeline_target(index, id, ids, window, cx)),
                (SequenceItem::Deactivate(id), _) => h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        Label::new("活性化の終了")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(self.render_lifeline_target(index, id, ids, window, cx)),
                (
                    SequenceItem::Autonumber(autonumber),
                    Some(ItemRow::Autonumber { start, step }),
                ) => h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        Label::new("自動番号 開始")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        field_frame(start, NUMBER_FIELD_WIDTH, true, window, cx)
                            .child(start.clone()),
                    )
                    .child(
                        Label::new("刻み")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        field_frame(step, NUMBER_FIELD_WIDTH, true, window, cx).child(step.clone()),
                    )
                    .child(
                        Checkbox::new(
                            ("mermaid-sequence-autonumber", index),
                            autonumber.visible.into(),
                        )
                        .label("番号を表示")
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(
                            move |this, state: &ToggleState, _window, cx| {
                                let visible = *state == ToggleState::Selected;
                                this.update_sequence_item(index, cx, move |item| {
                                    if let SequenceItem::Autonumber(autonumber) = item {
                                        autonumber.visible = visible;
                                    }
                                });
                            },
                        )),
                    ),
                _ => h_flex().w_full(),
            };
            let indent = depths.get(index).copied().unwrap_or_default().min(8);
            section = section.child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(div().flex_none().w(Rems(indent as f32)))
                    .child(body.flex_1())
                    .child(controls),
            );
        }

        let mut buttons = h_flex().w_full().flex_wrap().gap_1();
        for (id, label, kind) in [
            ("message", "メッセージ", AddKind::Message),
            ("note", "ノート", AddKind::Note),
            ("block", "ブロック開始", AddKind::BlockStart),
            ("branch", "分岐", AddKind::BlockBranch),
            ("end", "ブロック終了", AddKind::BlockEnd),
            ("activate", "活性化", AddKind::Activate),
            ("deactivate", "活性化の終了", AddKind::Deactivate),
            ("autonumber", "自動番号", AddKind::Autonumber),
        ] {
            buttons = buttons.child(add_button(
                SharedString::from(format!("mermaid-sequence-add-{id}")),
                SharedString::from(format!("{label}を追加")),
                cx.listener(move |this, _, window, cx| this.add_sequence_item(kind, window, cx)),
            ));
        }

        section.child(buttons).into_any_element()
    }

    pub(super) fn render_participant_kind(
        &self,
        index: usize,
        current: SequenceParticipantKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for kind in SequenceParticipantKind::ALL {
                let this = this.clone();
                menu = menu.toggleable_entry(
                    kind.label(),
                    kind == current,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        this.update(cx, |this, cx| this.set_participant_kind(index, kind, cx))
                            .log_err();
                    },
                );
            }
            menu
        });

        DropdownMenu::new(
            ("mermaid-sequence-participant-kind", index),
            current.label(),
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
    }

    pub(super) fn render_message_endpoint(
        &self,
        index: usize,
        is_from: bool,
        current: &str,
        ids: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.render_participant_menu(
            SharedString::from(format!(
                "mermaid-sequence-endpoint-{index}-{}",
                usize::from(is_from)
            )),
            index,
            current,
            ids,
            window,
            cx,
            move |item, id| {
                if let SequenceItem::Message(message) = item {
                    if is_from {
                        message.from = id;
                    } else {
                        message.to = id;
                    }
                }
            },
        )
    }

    pub(super) fn render_note_end(
        &self,
        index: usize,
        is_from: bool,
        current: &str,
        ids: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.render_participant_menu(
            SharedString::from(format!(
                "mermaid-sequence-note-end-{index}-{}",
                usize::from(is_from)
            )),
            index,
            current,
            ids,
            window,
            cx,
            move |item, id| {
                if let SequenceItem::Note(note) = item {
                    if is_from {
                        // A note that does not span sits at one column, so both
                        // ends move together.
                        if note.placement == SequenceNotePlacement::Over {
                            note.from = id;
                        } else {
                            note.to = id.clone();
                            note.from = id;
                        }
                    } else {
                        note.to = id;
                    }
                }
            },
        )
    }

    pub(super) fn render_lifeline_target(
        &self,
        index: usize,
        current: &str,
        ids: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.render_participant_menu(
            SharedString::from(format!("mermaid-sequence-lifeline-{index}")),
            index,
            current,
            ids,
            window,
            cx,
            move |item, id| match item {
                SequenceItem::Activate(target) | SequenceItem::Deactivate(target) => *target = id,
                _ => {}
            },
        )
    }

    /// A dropdown of the declared participants that writes the chosen id onto
    /// the item through `apply`.
    pub(super) fn render_participant_menu(
        &self,
        element_id: SharedString,
        index: usize,
        current: &str,
        ids: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
        apply: impl Fn(&mut SequenceItem, String) + Clone + 'static,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let selected = current.to_string();
        let choices = ids.to_vec();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for id in choices.clone() {
                let this = this.clone();
                let apply = apply.clone();
                let is_selected = id == selected;
                menu = menu.toggleable_entry(
                    id.clone(),
                    is_selected,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        let (id, apply) = (id.clone(), apply.clone());
                        this.update(cx, |this, cx| {
                            this.update_sequence_item(index, cx, move |item| apply(item, id));
                        })
                        .log_err();
                    },
                );
            }
            menu
        });

        DropdownMenu::new(element_id, SharedString::from(current.to_string()), menu)
            .style(DropdownStyle::Outlined)
            .trigger_size(ButtonSize::Compact)
            .into_any_element()
    }

    pub(super) fn render_arrow(
        &self,
        index: usize,
        current: SequenceArrow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for arrow in SequenceArrow::ALL {
                let this = this.clone();
                menu = menu.toggleable_entry(
                    format!("{}  {}", arrow.operator(), arrow.label()),
                    arrow == current,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        this.update(cx, |this, cx| {
                            this.update_sequence_item(index, cx, move |item| {
                                if let SequenceItem::Message(message) = item {
                                    message.arrow = arrow;
                                }
                            });
                        })
                        .log_err();
                    },
                );
            }
            menu
        });

        DropdownMenu::new(
            ("mermaid-sequence-arrow", index),
            SharedString::from(current.operator()),
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
    }

    pub(super) fn render_note_side(
        &self,
        index: usize,
        current: SequenceNotePlacement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for placement in SequenceNotePlacement::ALL {
                let this = this.clone();
                menu = menu.toggleable_entry(
                    placement.label(),
                    placement == current,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        this.update(cx, |this, cx| {
                            this.update_sequence_item(index, cx, move |item| {
                                if let SequenceItem::Note(note) = item {
                                    note.placement = placement;
                                    // A note that no longer spans has to come
                                    // back to one column, or the emitter would
                                    // drop the second end without saying so.
                                    if placement != SequenceNotePlacement::Over {
                                        note.to = note.from.clone();
                                    }
                                }
                            });
                        })
                        .log_err();
                    },
                );
            }
            menu
        });

        DropdownMenu::new(("mermaid-sequence-note-side", index), current.label(), menu)
            .style(DropdownStyle::Outlined)
            .trigger_size(ButtonSize::Compact)
            .into_any_element()
    }

    pub(super) fn render_block_kind(
        &self,
        index: usize,
        current: SequenceBlockKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for kind in SequenceBlockKind::ALL {
                let this = this.clone();
                menu = menu.toggleable_entry(
                    kind.label(),
                    kind == current,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        this.update(cx, |this, cx| {
                            this.update_sequence_item(index, cx, move |item| {
                                if let SequenceItem::BlockStart(block) = item {
                                    block.kind = kind;
                                }
                            });
                        })
                        .log_err();
                    },
                );
            }
            menu
        });

        DropdownMenu::new(("mermaid-sequence-block", index), current.label(), menu)
            .style(DropdownStyle::Outlined)
            .trigger_size(ButtonSize::Compact)
            .into_any_element()
    }

    pub(super) fn render_branch_kind(
        &self,
        index: usize,
        current: SequenceBranchKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for kind in branch_kinds() {
                let this = this.clone();
                menu = menu.toggleable_entry(
                    kind.label(),
                    kind == current,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        this.update(cx, |this, cx| {
                            this.update_sequence_item(index, cx, move |item| {
                                if let SequenceItem::BlockBranch(branch) = item {
                                    branch.kind = kind;
                                }
                            });
                        })
                        .log_err();
                    },
                );
            }
            menu
        });

        DropdownMenu::new(("mermaid-sequence-branch", index), current.label(), menu)
            .style(DropdownStyle::Outlined)
            .trigger_size(ButtonSize::Compact)
            .into_any_element()
    }
}

#[derive(Clone, Copy)]
pub(super) enum AddKind {
    Message,
    Note,
    BlockStart,
    BlockBranch,
    BlockEnd,
    Activate,
    Deactivate,
    Autonumber,
}

pub(super) fn names_participant(item: &SequenceItem, id: &str) -> bool {
    match item {
        SequenceItem::Message(message) => message.from == id || message.to == id,
        SequenceItem::Note(note) => note.from == id || note.to == id,
        SequenceItem::Activate(target) | SequenceItem::Deactivate(target) => target == id,
        SequenceItem::BlockStart(_)
        | SequenceItem::BlockBranch(_)
        | SequenceItem::BlockEnd
        | SequenceItem::Autonumber(_) => false,
    }
}

/// `autonumber` writes whole numbers as whole numbers, so the field shows what
/// the fence will hold rather than `1.0`.
pub(super) fn number_text(value: Option<f64>) -> String {
    match value {
        Some(value) if value.fract() == 0.0 => format!("{value:.0}"),
        Some(value) => format!("{value}"),
        None => String::new(),
    }
}
