//! The sequence diagram half of the Mermaid editor's form.
//!
//! A sequence diagram has two halves that cannot be merged: the participants,
//! whose declaration order is the left-to-right column order, and everything
//! that happens, which is one ordered list — a message, a note, the start of a
//! block, its branch, its `end` — because a message that changed places with
//! the `end` above it would be read as happening inside the block.
//!
//! A `box` wraps a *run* of declarations, so a column that joins one has to
//! stand next to the others: changing a participant's group moves its row to
//! the end of that group's run rather than leaving a diagram
//! [`SequenceDocument::structure_error`] would refuse.

use editor::Editor;
use gpui::{AnyElement, App, Context, Entity, Window};
use mermaid_render::sequence_model::{
    BOX_FILL_PRESETS, SequenceArrow, SequenceAutonumber, SequenceBlock, SequenceBlockKind,
    SequenceBranch, SequenceBranchKind, SequenceDocument, SequenceGroupBox, SequenceItem,
    SequenceMessage, SequenceNote, SequenceNotePlacement, SequenceParticipant,
    SequenceParticipantKind, apply_order, box_fill_label, is_valid_participant_id,
};
use ui::{
    Checkbox, ContextMenu, DropdownMenu, DropdownStyle, IconPosition, ToggleState, prelude::*,
};
use util::ResultExt as _;

use super::{
    Form, ID_FIELD_WIDTH, LABEL_FIELD_WIDTH, MermaidEditor, NUMBER_FIELD_WIDTH, add_button,
    field_frame, neighbour, row_controls, row_controls_labelled, section_title,
};

pub(super) struct SequenceForm {
    pub(super) document: SequenceDocument,
    pub(super) title: Entity<Editor>,
    pub(super) boxes: Vec<BoxRow>,
    pub(super) participants: Vec<ParticipantRow>,
    pub(super) items: Vec<ItemRow>,
}

pub(super) struct ParticipantRow {
    pub(super) id: Entity<Editor>,
    pub(super) alias: Entity<Editor>,
}

pub(super) struct BoxRow {
    pub(super) name: Entity<Editor>,
}

pub(super) enum ItemRow {
    /// A message, a note, a block start or a block branch: one line of text.
    Text(Entity<Editor>),
    Autonumber {
        start: Entity<Editor>,
        step: Entity<Editor>,
    },
    /// `end`, `activate`, `deactivate`, `create` and `destroy` have nothing to
    /// type into.
    Fixed,
}

impl SequenceForm {
    pub(super) fn fields(&self) -> Vec<Entity<Editor>> {
        std::iter::once(self.title.clone())
            .chain(self.boxes.iter().map(|row| row.name.clone()))
            .chain(
                self.participants
                    .iter()
                    .zip(&self.document.participants)
                    .flat_map(|(row, participant)| {
                        // A column with a config object has no description
                        // Mermaid could write, so its field is not drawn. A
                        // created column's is drawn on its `create` row, and
                        // is reached there.
                        if participant.preserved_settings.is_some()
                            || self.document.is_created(&participant.id)
                        {
                            vec![row.id.clone()]
                        } else {
                            vec![row.id.clone(), row.alias.clone()]
                        }
                    }),
            )
            .chain(
                self.items
                    .iter()
                    .zip(&self.document.items)
                    .flat_map(|(row, item)| match (row, item) {
                        (ItemRow::Text(field), _) => vec![field.clone()],
                        (ItemRow::Autonumber { start, step }, _) => {
                            vec![start.clone(), step.clone()]
                        }
                        (ItemRow::Fixed, SequenceItem::Create(id)) => self
                            .document
                            .participant_index(id)
                            .and_then(|column| {
                                let participant = self.document.participants.get(column)?;
                                let row = self.participants.get(column)?;
                                participant
                                    .preserved_settings
                                    .is_none()
                                    .then(|| row.alias.clone())
                            })
                            .into_iter()
                            .collect(),
                        (ItemRow::Fixed, _) => Vec::new(),
                    }),
            )
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

        for (group, row) in document.boxes.iter_mut().zip(&self.boxes) {
            // A name of nothing but spaces reads as an unnamed box everywhere
            // else in the form, so it has to write one back as well.
            let name = row.name.read(cx).text(cx).trim().to_string();
            group.name = (!name.is_empty()).then_some(name);
        }

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
                    SequenceItem::Activate(id)
                    | SequenceItem::Deactivate(id)
                    | SequenceItem::Create(id)
                    | SequenceItem::Destroy(id) => {
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
        return Some(i18n::t!("Add at least one participant.").into());
    }
    for participant in &document.participants {
        if participant.id.is_empty() {
            return Some(i18n::t!("A participant has an empty ID.").into());
        }
        if !is_valid_participant_id(&participant.id) {
            return Some(
                i18n::t!(
                    "The ID \"{id}\" is not valid (no symbols, and it cannot start with a digit).",
                    id = participant.id
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

/// Who a new `destroy` row names. Mermaid requires the message right after a
/// `destroy` to involve the column it ends, and that message is usually the
/// last one written, so its receiver — or failing that its sender — is the
/// likely target. A column already destroyed is skipped, because a second
/// `destroy` of it is refused.
pub(super) fn default_destroy_target(document: &SequenceDocument) -> Option<String> {
    let destroyed = document
        .items
        .iter()
        .filter_map(|item| match item {
            SequenceItem::Destroy(id) => Some(id.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let last_message = document.items.iter().rev().find_map(|item| match item {
        SequenceItem::Message(message) => Some(message),
        _ => None,
    });
    let candidates = last_message
        .into_iter()
        .flat_map(|message| [message.to.as_str(), message.from.as_str()])
        .chain(
            document
                .participants
                .iter()
                .map(|participant| participant.id.as_str()),
        );
    let mut fallback = None;
    for id in candidates {
        if document.participant_index(id).is_none() {
            continue;
        }
        fallback.get_or_insert(id);
        if !destroyed.contains(&id) {
            return Some(id.to_string());
        }
    }
    fallback.map(str::to_string)
}

/// The columns the `create` row at `item_index` can name: its own, and every
/// column that no other `create` declares and that is in no `box` — a created
/// column is declared by its statement, and a box can only hold columns
/// declared at the top.
pub(super) fn create_target_choices(document: &SequenceDocument, item_index: usize) -> Vec<String> {
    let current = match document.items.get(item_index) {
        Some(SequenceItem::Create(id)) => Some(id.as_str()),
        _ => None,
    };
    let created_elsewhere = document
        .items
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != item_index)
        .filter_map(|(_, item)| match item {
            SequenceItem::Create(id) => Some(id.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    document
        .participants
        .iter()
        .filter(|participant| {
            Some(participant.id.as_str()) == current
                || (participant.box_index.is_none()
                    && !created_elsewhere.contains(&participant.id.as_str()))
        })
        .map(|participant| participant.id.clone())
        .collect()
}

impl SequenceForm {
    /// Moves the participants and their rows together into `order`.
    fn reorder_participants(&mut self, order: &[usize]) -> bool {
        if self.participants.len() != self.document.participants.len() {
            return false;
        }
        self.document.reorder_participants(order) && apply_order(&mut self.participants, order)
    }

    /// Keeps the list of boxes in the order the fence will draw them, so that
    /// the list the user sees is the one the reopened fence has.
    fn normalize_box_order(&mut self) {
        let order = self.document.canonical_box_order();
        if order.iter().enumerate().all(|(index, old)| index == *old)
            || self.boxes.len() != self.document.boxes.len()
        {
            return;
        }
        if self.document.reorder_boxes(&order) {
            apply_order(&mut self.boxes, &order);
        }
    }
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
        let mut boxes = Vec::with_capacity(document.boxes.len());
        for group in &document.boxes {
            let name = group.name.clone().unwrap_or_default();
            boxes.push(BoxRow {
                name: self.new_field(&name, window, cx),
            });
        }
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
        let mut form = SequenceForm {
            document,
            title,
            boxes,
            participants,
            items,
        };
        // An empty box written between two others is drawn, and written back,
        // after them; the list starts out in that order rather than moving
        // it on the first edit.
        form.normalize_box_order();
        form
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
            SequenceItem::BlockEnd
            | SequenceItem::Activate(_)
            | SequenceItem::Deactivate(_)
            | SequenceItem::Create(_)
            | SequenceItem::Destroy(_) => ItemRow::Fixed,
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
        let document = form.current_document(cx);
        let id = document.unused_participant_id(i18n::t!("NewParticipant"));
        // Nothing may be declared at the top of the diagram after a `create`d
        // column, so a new row goes in front of the first of those.
        let at = document
            .participants
            .iter()
            .position(|participant| document.is_created(&participant.id))
            .unwrap_or(document.participants.len());
        let row = ParticipantRow {
            id: self.new_field(&id, window, cx),
            alias: self.new_field("", window, cx),
        };
        let Some(form) = self.sequence_mut() else {
            return;
        };
        if at > form.document.participants.len() || at > form.participants.len() {
            return;
        }
        form.document.participants.insert(
            at,
            SequenceParticipant {
                id,
                alias: None,
                kind: SequenceParticipantKind::Participant,
                wrap: false,
                box_index: None,
                preserved_settings: None,
            },
        );
        form.participants.insert(at, row);
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
        form.normalize_box_order();
        self.schedule_preview(cx);
        cx.notify();
    }

    /// Moves a column one step, or over a whole `box` for a column in none;
    /// see [`SequenceDocument::participant_move_order`] for the steps it
    /// refuses.
    pub(super) fn move_participant(&mut self, index: usize, forward: bool, cx: &mut Context<Self>) {
        let Some(form) = self.sequence() else {
            return;
        };
        let Some(order) = form
            .current_document(cx)
            .participant_move_order(index, forward)
        else {
            return;
        };
        let Some(form) = self.sequence_mut() else {
            return;
        };
        if !form.reorder_participants(&order) {
            return;
        }
        form.normalize_box_order();
        self.schedule_preview(cx);
        cx.notify();
    }

    /// Moves a `box` one step through the list, taking its columns with it.
    pub(super) fn move_sequence_box(
        &mut self,
        index: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.sequence_mut() else {
            return;
        };
        let Some((participant_order, box_order)) = form.document.box_move_order(index, forward)
        else {
            return;
        };
        if form.boxes.len() != form.document.boxes.len()
            || !form.reorder_participants(&participant_order)
        {
            return;
        }
        if form.document.reorder_boxes(&box_order) {
            apply_order(&mut form.boxes, &box_order);
        }
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn set_sequence_box_wrap(
        &mut self,
        index: usize,
        wrap: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.sequence_mut() else {
            return;
        };
        let Some(group) = form.document.boxes.get_mut(index) else {
            return;
        };
        group.wrap = wrap;
        self.schedule_preview(cx);
        cx.notify();
    }

    /// Points the `create` row at `index` at the column `id`, moving the
    /// columns so that every created one still comes last, in the order of
    /// the statements — which is the only order the fence can hold.
    pub(super) fn set_create_target(&mut self, index: usize, id: String, cx: &mut Context<Self>) {
        let Some(form) = self.sequence() else {
            return;
        };
        let mut document = form.current_document(cx);
        let Some(SequenceItem::Create(target)) = document.items.get_mut(index) else {
            return;
        };
        *target = id.clone();
        let order = document.created_last_order();
        let Some(form) = self.sequence_mut() else {
            return;
        };
        let Some(SequenceItem::Create(target)) = form.document.items.get_mut(index) else {
            return;
        };
        *target = id;
        form.reorder_participants(&order);
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

    pub(super) fn add_sequence_box(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = i18n::t!("Group").to_string();
        let row = BoxRow {
            name: self.new_field(&name, window, cx),
        };
        let Some(form) = self.sequence_mut() else {
            return;
        };
        form.document.boxes.push(SequenceGroupBox {
            name: Some(name),
            fill: None,
            wrap: false,
        });
        form.boxes.push(row);
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn remove_sequence_box(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(form) = self.sequence_mut() else {
            return;
        };
        if index >= form.document.boxes.len() || index >= form.boxes.len() {
            return;
        }
        form.document.boxes.remove(index);
        form.boxes.remove(index);
        // `box_index` is a position in the list, so every index past the one
        // that went away now names a different box.
        for participant in &mut form.document.participants {
            participant.box_index = match participant.box_index {
                Some(current) if current == index => None,
                Some(current) if current > index => Some(current - 1),
                other => other,
            };
        }
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn set_sequence_box_fill(
        &mut self,
        index: usize,
        fill: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.sequence_mut() else {
            return;
        };
        let Some(group) = form.document.boxes.get_mut(index) else {
            return;
        };
        group.fill = fill;
        self.schedule_preview(cx);
        cx.notify();
    }

    /// Puts the participant at `index` into `box_index`, moving its row so that
    /// the group stays one unbroken run of declarations.
    ///
    /// A column that joins a box has to sit next to the box's other members,
    /// and one that leaves has to get out from between them, so both cases move
    /// the row to just after the last member of the box that is changing size.
    /// A box the column is the first member of has no such row, so the move
    /// falls back to the group it is leaving: staying put would otherwise cut
    /// that group's run in two.
    pub(super) fn set_participant_box(
        &mut self,
        index: usize,
        box_index: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.sequence_mut() else {
            return;
        };
        if index >= form.participants.len() {
            return;
        }
        if box_index.is_some_and(|box_index| box_index >= form.document.boxes.len()) {
            return;
        }
        let Some(participant) = form.document.participants.get_mut(index) else {
            return;
        };
        let previous = participant.box_index;
        if previous == box_index {
            return;
        }
        participant.box_index = box_index;

        let last_member = |anchor: usize| {
            form.document
                .participants
                .iter()
                .enumerate()
                .filter(|(other, participant)| {
                    *other != index && participant.box_index == Some(anchor)
                })
                .map(|(other, _)| other)
                .max()
        };
        let last = box_index
            .and_then(last_member)
            .or_else(|| previous.and_then(last_member));
        // Taking the row out first shifts every later index down by one,
        // which is why a row moving down lands on the anchor's own index.
        if let Some(last) = last {
            let target = if index < last { last } else { last + 1 };
            if target != index {
                let participant = form.document.participants.remove(index);
                form.document.participants.insert(target, participant);
                let row = form.participants.remove(index);
                form.participants.insert(target, row);
            }
        }
        form.normalize_box_order();
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn add_sequence_item(
        &mut self,
        kind: AddKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(kind, AddKind::Create) {
            self.add_created_participant(window, cx);
            return;
        }
        let Some(form) = self.sequence() else {
            return;
        };
        let document = form.current_document(cx);
        let first = document.participants.first().map(|one| one.id.clone());
        let item = match kind {
            AddKind::Message
            | AddKind::Note
            | AddKind::Activate
            | AddKind::Deactivate
            | AddKind::Destroy => {
                let Some(first) = first else {
                    self.message = Some(i18n::t!("Add a participant first.").into());
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
                        text: i18n::t!("Memo").to_string(),
                        wrap: false,
                    }),
                    AddKind::Activate => SequenceItem::Activate(first),
                    AddKind::Destroy => {
                        SequenceItem::Destroy(default_destroy_target(&document).unwrap_or(first))
                    }
                    _ => SequenceItem::Deactivate(first),
                }
            }
            // `create` is handled above: it declares a column of its own.
            AddKind::Create => return,
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

    /// `create` declares a column of its own, so the button adds the
    /// participant together with the statement — at the end of the list,
    /// because the top of the diagram can no longer declare anything that
    /// comes after a created column.
    fn add_created_participant(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.sequence() else {
            return;
        };
        let id = form
            .current_document(cx)
            .unused_participant_id(i18n::t!("NewParticipant"));
        let row = ParticipantRow {
            id: self.new_field(&id, window, cx),
            alias: self.new_field("", window, cx),
        };
        let item = SequenceItem::Create(id.clone());
        let item_row = self.new_sequence_row(&item, window, cx);
        let Some(form) = self.sequence_mut() else {
            return;
        };
        form.document.participants.push(SequenceParticipant {
            id,
            alias: None,
            kind: SequenceParticipantKind::Participant,
            wrap: false,
            box_index: None,
            preserved_settings: None,
        });
        form.participants.push(row);
        form.document.items.push(item);
        form.items.push(item_row);
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
            .child(section_title(i18n::t!("Title")))
            .child(
                field_frame(&form.title, LABEL_FIELD_WIDTH, true, window, cx)
                    .flex_1()
                    .child(form.title.clone()),
            )
            .into_any_element();

        vec![
            title,
            self.render_sequence_boxes(form, window, cx),
            self.render_participants(form, window, cx),
            self.render_sequence_items(form, &ids, window, cx),
        ]
    }

    pub(super) fn render_sequence_boxes(
        &self,
        form: &SequenceForm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut section = v_flex()
            .w_full()
            .gap_1()
            .child(section_title(i18n::t!("box (Group of Participants)")));
        for index in 0..form.boxes.len() {
            let (Some(row), Some(group)) = (form.boxes.get(index), form.document.boxes.get(index))
            else {
                continue;
            };
            section = section.child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        Label::new(box_number_label(index))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        field_frame(&row.name, LABEL_FIELD_WIDTH, true, window, cx)
                            .flex_1()
                            .child(row.name.clone()),
                    )
                    .child(self.render_box_fill(index, group.fill.as_deref(), window, cx))
                    .child(
                        Checkbox::new(("mermaid-sequence-box-wrap", index), group.wrap.into())
                            .label(i18n::t!("Wrap"))
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(
                                move |this, state: &ToggleState, _window, cx| {
                                    let wrap = *state == ToggleState::Selected;
                                    this.set_sequence_box_wrap(index, wrap, cx);
                                },
                            )),
                    )
                    .child(row_controls_labelled(
                        "mermaid-sequence-box",
                        index,
                        form.document.box_move_order(index, false).is_none(),
                        form.document.box_move_order(index, true).is_none(),
                        i18n::t!("Delete (the participants inside are kept)"),
                        cx.listener(move |this, _, _window, cx| {
                            this.move_sequence_box(index, false, cx)
                        }),
                        cx.listener(move |this, _, _window, cx| {
                            this.move_sequence_box(index, true, cx)
                        }),
                        cx.listener(move |this, _, _window, cx| {
                            this.remove_sequence_box(index, cx)
                        }),
                    )),
            );
        }
        section
            .child(add_button(
                "mermaid-sequence-add-box",
                i18n::t!("Add box"),
                cx.listener(|this, _, window, cx| this.add_sequence_box(window, cx)),
            ))
            .into_any_element()
    }

    /// The `box` a participant belongs to. Mermaid writes a box as a run of
    /// declarations, so choosing one here also moves the row into that run.
    pub(super) fn render_participant_box(
        &self,
        form: &SequenceForm,
        index: usize,
        current: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let names = form
            .document
            .boxes
            .iter()
            .enumerate()
            .map(|(box_index, group)| box_menu_label(box_index, group.name.as_deref()))
            .collect::<Vec<_>>();
        let label = current
            .and_then(|box_index| names.get(box_index).cloned())
            .unwrap_or_else(|| i18n::t!("No Box").to_string());
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            let mut choices = vec![(None, i18n::t!("No Box").to_string())];
            choices.extend(
                names
                    .iter()
                    .enumerate()
                    .map(|(box_index, name)| (Some(box_index), name.clone())),
            );
            for (choice, name) in choices {
                let this = this.clone();
                menu = menu.toggleable_entry(
                    name,
                    choice == current,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        this.update(cx, |this, cx| this.set_participant_box(index, choice, cx))
                            .log_err();
                    },
                );
            }
            menu.separator()
                .entry(i18n::t!("New box…"), None, move |window, cx| {
                    this.update(cx, |this, cx| {
                        this.add_sequence_box(window, cx);
                        let Some(form) = this.sequence() else {
                            return;
                        };
                        let Some(added) = form.document.boxes.len().checked_sub(1) else {
                            return;
                        };
                        this.set_participant_box(index, Some(added), cx);
                    })
                    .log_err();
                })
        });

        DropdownMenu::new(
            ("mermaid-sequence-participant-box", index),
            SharedString::from(label),
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
    }

    pub(super) fn render_box_fill(
        &self,
        index: usize,
        current: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let current = current.map(str::to_string);
        // A fill written by hand — `rgb(…)` say — is not one of the presets, so
        // it is offered alongside them rather than being lost as soon as the
        // menu is opened.
        let custom = current.clone().filter(|fill| {
            !BOX_FILL_PRESETS
                .iter()
                .any(|(preset, _)| *preset == Some(fill.as_str()))
        });
        let label = box_fill_label(current.as_deref());
        let selected = current;
        let menu =
            ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
                let mut choices: Vec<(Option<String>, String)> = Vec::new();
                if let Some(fill) = &custom {
                    choices.push((Some(fill.clone()), fill.clone()));
                }
                choices.extend(BOX_FILL_PRESETS.iter().map(|(preset, label)| {
                    (preset.map(str::to_string), i18n::tr(label).to_string())
                }));
                for (fill, label) in choices {
                    let this = this.clone();
                    menu = menu.toggleable_entry(
                        label,
                        fill == selected,
                        IconPosition::Start,
                        None,
                        move |_window, cx| {
                            let fill = fill.clone();
                            this.update(cx, |this, cx| this.set_sequence_box_fill(index, fill, cx))
                                .log_err();
                        },
                    );
                }
                menu
            });

        DropdownMenu::new(
            ("mermaid-sequence-box-fill", index),
            SharedString::from(label),
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
    }

    pub(super) fn render_participants(
        &self,
        form: &SequenceForm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // The moves are worked out on what the rows say now, which is what
        // `move_participant` looks at too.
        let current = form.current_document(cx);
        let mut section = v_flex().w_full().gap_1().child(section_title(i18n::t!(
            "Participants (listed top to bottom, drawn left to right)"
        )));
        for index in 0..form.participants.len() {
            let (Some(row), Some(participant)) = (
                form.participants.get(index),
                form.document.participants.get(index),
            ) else {
                continue;
            };
            // A `create`d column is declared by its statement in the list
            // below, and a declaration inside a `box` would be a second one.
            // Its description is edited on that row too, beside the statement
            // that writes it.
            let created = form.document.is_created(&participant.id);
            let settings = participant.preserved_settings.clone();
            let has_settings = settings.is_some();
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
                    .when(!created && !has_settings, |this| {
                        this.child(
                            field_frame(&row.alias, LABEL_FIELD_WIDTH, true, window, cx)
                                .flex_1()
                                .child(row.alias.clone()),
                        )
                    })
                    // Mermaid only takes a config object after `participant`
                    // and with no `as`, so the object stands in for both the
                    // description and the kind menu.
                    .when_some(settings, |this, settings| {
                        this.child(
                            div().flex_1().min_w_0().child(
                                Label::new(i18n::t!("Settings @{{{settings}}}"))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                        )
                    })
                    .when(created && !has_settings, |this| this.child(div().flex_1()))
                    .when(!has_settings, |this| {
                        this.child(self.render_participant_kind(
                            index,
                            participant.kind,
                            window,
                            cx,
                        ))
                    })
                    .when(created, |this| {
                        this.child(
                            Label::new(i18n::t!("Created by create"))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .when(!created, |this| {
                        this.child(self.render_participant_box(
                            form,
                            index,
                            participant.box_index,
                            window,
                            cx,
                        ))
                    })
                    .child(row_controls(
                        "mermaid-sequence-participant",
                        index,
                        current.participant_move_order(index, false).is_none(),
                        current.participant_move_order(index, true).is_none(),
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
                i18n::t!("Add Participant"),
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
        let mut section = v_flex()
            .w_full()
            .gap_1()
            .child(section_title(i18n::t!("Interactions")));

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
                        .label(i18n::t!("Start"))
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
                        .label(i18n::t!("End"))
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
                            Label::new(i18n::t!("Note"))
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
                    Label::new(i18n::t!("end (End of Block)"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
                (SequenceItem::Activate(id), _) => h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        Label::new(i18n::t!("Activation"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(self.render_lifeline_target(index, id, ids, window, cx)),
                (SequenceItem::Deactivate(id), _) => h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        Label::new(i18n::t!("Deactivation"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(self.render_lifeline_target(index, id, ids, window, cx)),
                (SequenceItem::Create(id), _) => {
                    let alias = form
                        .document
                        .participant_index(id)
                        .or_else(|| ids.iter().position(|current| current == id))
                        .and_then(|column| {
                            let participant = form.document.participants.get(column)?;
                            let row = form.participants.get(column)?;
                            participant
                                .preserved_settings
                                .is_none()
                                .then(|| row.alias.clone())
                        });
                    h_flex()
                        .w_full()
                        .gap_1()
                        .child(
                            Label::new(i18n::t!("Participant Creation"))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(self.render_create_target(index, id, window, cx))
                        .when_some(alias, |this, alias| {
                            this.child(
                                Label::new(i18n::t!("Display Name (as)"))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                field_frame(&alias, LABEL_FIELD_WIDTH, true, window, cx)
                                    .flex_1()
                                    .child(alias.clone()),
                            )
                        })
                }
                (SequenceItem::Destroy(id), _) => h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        Label::new(i18n::t!("Participant Destruction"))
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
                        Label::new(i18n::t!("Autonumber Start"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        field_frame(start, NUMBER_FIELD_WIDTH, true, window, cx)
                            .child(start.clone()),
                    )
                    .child(
                        Label::new(i18n::t!("Step"))
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
                        .label(i18n::t!("Show Numbers"))
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
            ("message", i18n::t!("Add Message"), AddKind::Message),
            ("note", i18n::t!("Add Note"), AddKind::Note),
            ("block", i18n::t!("Add Block Start"), AddKind::BlockStart),
            ("branch", i18n::t!("Add Block Branch"), AddKind::BlockBranch),
            ("end", i18n::t!("Add Block End"), AddKind::BlockEnd),
            ("activate", i18n::t!("Add Activation"), AddKind::Activate),
            (
                "deactivate",
                i18n::t!("Add Deactivation"),
                AddKind::Deactivate,
            ),
            (
                "create",
                i18n::t!("Add Participant Creation"),
                AddKind::Create,
            ),
            (
                "destroy",
                i18n::t!("Add Participant Destruction"),
                AddKind::Destroy,
            ),
            (
                "autonumber",
                i18n::t!("Add Autonumber"),
                AddKind::Autonumber,
            ),
        ] {
            buttons = buttons.child(add_button(
                SharedString::from(format!("mermaid-sequence-add-{id}")),
                label,
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
                SequenceItem::Activate(target)
                | SequenceItem::Deactivate(target)
                | SequenceItem::Create(target)
                | SequenceItem::Destroy(target) => *target = id,
                _ => {}
            },
        )
    }

    /// The column a `create` row declares. Choosing another one moves the
    /// columns so that the created ones stay last; see
    /// [`MermaidEditor::set_create_target`].
    pub(super) fn render_create_target(
        &self,
        index: usize,
        current: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(form) = self.sequence() else {
            return div().into_any_element();
        };
        let choices = create_target_choices(&form.current_document(cx), index);
        let this = cx.entity().downgrade();
        let selected = current.to_string();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for id in choices.clone() {
                let this = this.clone();
                let is_selected = id == selected;
                menu = menu.toggleable_entry(
                    id.clone(),
                    is_selected,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        let id = id.clone();
                        this.update(cx, |this, cx| this.set_create_target(index, id, cx))
                            .log_err();
                    },
                );
            }
            menu
        });

        DropdownMenu::new(
            SharedString::from(format!("mermaid-sequence-create-{index}")),
            SharedString::from(current.to_string()),
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
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
    Create,
    Destroy,
    Autonumber,
}

pub(super) fn names_participant(item: &SequenceItem, id: &str) -> bool {
    match item {
        SequenceItem::Message(message) => message.from == id || message.to == id,
        SequenceItem::Note(note) => note.from == id || note.to == id,
        SequenceItem::Activate(target)
        | SequenceItem::Deactivate(target)
        | SequenceItem::Create(target)
        | SequenceItem::Destroy(target) => target == id,
        SequenceItem::BlockStart(_)
        | SequenceItem::BlockBranch(_)
        | SequenceItem::BlockEnd
        | SequenceItem::Autonumber(_) => false,
    }
}

/// How the `box` list names the row whose name field is still empty.
pub(super) fn box_number_label(index: usize) -> String {
    format!("box {}", index + 1)
}

/// How the membership menu names a box: its own name, or its place in the list
/// when it has none, because an unnamed box still has to be pickable.
pub(super) fn box_menu_label(index: usize, name: Option<&str>) -> String {
    match name.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => name.to_string(),
        None => i18n::t!("{label} (Unnamed)", label = box_number_label(index)),
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
