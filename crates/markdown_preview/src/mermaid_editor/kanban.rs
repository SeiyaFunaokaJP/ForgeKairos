//! The kanban board half of the Mermaid editor's form.
//!
//! A board is a row of columns, each holding cards, so the form is one framed
//! group per column with one block per card. The structure — which card is in
//! which column, the brackets, the metadata the form does not show — lives in
//! the document; the fields hold the texts and are read over it whenever the
//! document is needed.

use editor::Editor;
use gpui::{AnyElement, App, Context, Entity, Focusable as _, Window};
use mermaid_render::kanban_model::{
    ASSIGNED_KEY, KANBAN_PRIORITIES, KanbanColumn, KanbanDocument, KanbanNode, PRIORITY_KEY,
    TICKET_KEY, is_valid_kanban_id, is_valid_kanban_metadata_value, is_valid_kanban_title,
};
use ui::{
    ContextMenu, DropdownMenu, DropdownStyle, IconButtonShape, IconPosition, Tooltip, prelude::*,
};
use util::ResultExt as _;

use super::{
    Form, ID_FIELD_WIDTH, LABEL_FIELD_WIDTH, MermaidEditor, add_button, field_frame, neighbour,
    section_title,
};

/// The texts new rows are added with, so that they are visible in the drawing
/// before anything has been typed into them.
const NEW_COLUMN_TITLE: &str = i18n::msg!("New column");
const NEW_CARD_TITLE: &str = i18n::msg!("New card");

pub(super) struct KanbanForm {
    pub(super) document: KanbanDocument,
    pub(super) columns: Vec<ColumnRow>,
}

pub(super) struct ColumnRow {
    pub(super) id: Entity<Editor>,
    pub(super) title: Entity<Editor>,
    pub(super) cards: Vec<CardRow>,
}

pub(super) struct CardRow {
    pub(super) id: Entity<Editor>,
    pub(super) title: Entity<Editor>,
    pub(super) assigned: Entity<Editor>,
    pub(super) ticket: Entity<Editor>,
}

impl KanbanForm {
    pub(super) fn fields(&self) -> Vec<Entity<Editor>> {
        self.columns
            .iter()
            .flat_map(|column| {
                [column.id.clone(), column.title.clone()].into_iter().chain(
                    column.cards.iter().flat_map(|card| {
                        [
                            card.id.clone(),
                            card.title.clone(),
                            card.assigned.clone(),
                            card.ticket.clone(),
                        ]
                    }),
                )
            })
            .collect()
    }

    /// The kanban board the form currently describes: the structure held in
    /// `document` with the text fields' current contents written over it.
    pub(super) fn current_document(&self, cx: &App) -> KanbanDocument {
        let mut document = self.document.clone();
        for (column, row) in document.columns.iter_mut().zip(&self.columns) {
            read_node(&mut column.node, &row.id, &row.title, cx);
            for (card, card_row) in column.cards.iter_mut().zip(&row.cards) {
                read_node(card, &card_row.id, &card_row.title, cx);
                card.set_metadata_value(ASSIGNED_KEY, Some(&field_text(&card_row.assigned, cx)));
                card.set_metadata_value(TICKET_KEY, Some(&field_text(&card_row.ticket, cx)));
            }
        }
        document
    }
}

fn read_node(node: &mut KanbanNode, id: &Entity<Editor>, title: &Entity<Editor>, cx: &App) {
    node.id = field_text(id, cx);
    let title = field_text(title, cx);
    if node.title() != title {
        node.set_title(&title);
    }
    node.fit_label_quoting();
}

/// The first thing wrong with what the user typed, in the words shown above the
/// form. Every check lives in `KanbanDocument::structure_error`, which the
/// caller consults right after this; the red frames come from the
/// `is_valid_kanban_*` predicates directly.
pub(super) fn problem(_document: &KanbanDocument) -> Option<SharedString> {
    None
}

fn field_text(field: &Entity<Editor>, cx: &App) -> String {
    field.read(cx).text(cx).trim().to_string()
}

/// What the priority menu shows for a card's priority. A value Mermaid has no
/// colour for is shown as it is written, so that it is not mistaken for none.
fn priority_label(priority: Option<&str>) -> SharedString {
    match priority {
        None => i18n::t!("No Priority").into(),
        Some(priority) => {
            let label = KANBAN_PRIORITIES
                .iter()
                .find(|(value, _)| *value == priority)
                .map_or(priority, |(_, label)| i18n::tr(label));
            i18n::t!("Priority: {label}").into()
        }
    }
}

impl MermaidEditor {
    pub(super) fn new_kanban_form(
        &mut self,
        document: KanbanDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> KanbanForm {
        let mut columns = Vec::with_capacity(document.columns.len());
        for column in &document.columns {
            let row = self.new_kanban_column_row(column, window, cx);
            columns.push(row);
        }
        KanbanForm { document, columns }
    }

    fn new_kanban_column_row(
        &mut self,
        column: &KanbanColumn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ColumnRow {
        let id = self.new_field(&column.node.id, window, cx);
        let title = self.new_field(&column.node.title(), window, cx);
        let mut cards = Vec::with_capacity(column.cards.len());
        for card in &column.cards {
            cards.push(self.new_kanban_card_row(card, window, cx));
        }
        ColumnRow { id, title, cards }
    }

    fn new_kanban_card_row(
        &mut self,
        card: &KanbanNode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> CardRow {
        CardRow {
            id: self.new_field(&card.id, window, cx),
            title: self.new_field(&card.title(), window, cx),
            assigned: self.new_field(
                &card.metadata_value(ASSIGNED_KEY).unwrap_or_default(),
                window,
                cx,
            ),
            ticket: self.new_field(
                &card.metadata_value(TICKET_KEY).unwrap_or_default(),
                window,
                cx,
            ),
        }
    }

    pub(super) fn kanban(&self) -> Option<&KanbanForm> {
        match &self.form {
            Form::Kanban(form) => Some(form),
            _ => None,
        }
    }

    pub(super) fn kanban_mut(&mut self) -> Option<&mut KanbanForm> {
        match &mut self.form {
            Form::Kanban(form) => Some(form),
            _ => None,
        }
    }

    /// Reads the fields into the document before a structural change, which
    /// moves the document and the rows in step.
    fn sync_kanban(&mut self, cx: &mut Context<Self>) {
        let Some(document) = self.kanban().map(|form| form.current_document(cx)) else {
            return;
        };
        if let Some(form) = self.kanban_mut() {
            form.document = document;
        }
    }

    fn kanban_changed(&mut self, cx: &mut Context<Self>) {
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn add_kanban_column(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_kanban(cx);
        let Some(form) = self.kanban() else {
            return;
        };
        let column = KanbanColumn {
            node: KanbanNode::new(
                form.document.unused_id("column"),
                i18n::tr(NEW_COLUMN_TITLE),
            ),
            cards: Vec::new(),
        };
        let row = self.new_kanban_column_row(&column, window, cx);
        let title = row.title.clone();
        let Some(form) = self.kanban_mut() else {
            return;
        };
        form.document.columns.push(column);
        form.columns.push(row);
        title.focus_handle(cx).focus(window, cx);
        self.kanban_changed(cx);
    }

    pub(super) fn remove_kanban_column(&mut self, index: usize, cx: &mut Context<Self>) {
        self.sync_kanban(cx);
        let Some(form) = self.kanban_mut() else {
            return;
        };
        if index >= form.columns.len() || index >= form.document.columns.len() {
            return;
        }
        form.document.columns.remove(index);
        form.columns.remove(index);
        self.kanban_changed(cx);
    }

    pub(super) fn move_kanban_column(
        &mut self,
        index: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        self.sync_kanban(cx);
        let Some(form) = self.kanban_mut() else {
            return;
        };
        let length = form.columns.len().min(form.document.columns.len());
        let Some(other) = neighbour(index, forward, length) else {
            return;
        };
        if index >= length {
            return;
        }
        form.document.columns.swap(index, other);
        form.columns.swap(index, other);
        self.kanban_changed(cx);
    }

    pub(super) fn add_kanban_card(
        &mut self,
        column: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_kanban(cx);
        let Some(form) = self.kanban() else {
            return;
        };
        if column >= form.columns.len() || column >= form.document.columns.len() {
            return;
        }
        let card = KanbanNode::new(form.document.unused_id("card"), i18n::tr(NEW_CARD_TITLE));
        let row = self.new_kanban_card_row(&card, window, cx);
        let title = row.title.clone();
        let Some(form) = self.kanban_mut() else {
            return;
        };
        let (Some(cards), Some(rows)) = (
            form.document.columns.get_mut(column),
            form.columns.get_mut(column),
        ) else {
            return;
        };
        cards.cards.push(card);
        rows.cards.push(row);
        title.focus_handle(cx).focus(window, cx);
        self.kanban_changed(cx);
    }

    pub(super) fn remove_kanban_card(
        &mut self,
        column: usize,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        self.sync_kanban(cx);
        let Some(form) = self.kanban_mut() else {
            return;
        };
        let (Some(cards), Some(rows)) = (
            form.document.columns.get_mut(column),
            form.columns.get_mut(column),
        ) else {
            return;
        };
        if index >= cards.cards.len() || index >= rows.cards.len() {
            return;
        }
        cards.cards.remove(index);
        rows.cards.remove(index);
        self.kanban_changed(cx);
    }

    /// Moves a card up or down within its column.
    pub(super) fn move_kanban_card(
        &mut self,
        column: usize,
        index: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        self.sync_kanban(cx);
        let Some(form) = self.kanban_mut() else {
            return;
        };
        let (Some(cards), Some(rows)) = (
            form.document.columns.get_mut(column),
            form.columns.get_mut(column),
        ) else {
            return;
        };
        let length = cards.cards.len().min(rows.cards.len());
        let Some(other) = neighbour(index, forward, length) else {
            return;
        };
        if index >= length {
            return;
        }
        cards.cards.swap(index, other);
        rows.cards.swap(index, other);
        self.kanban_changed(cx);
    }

    /// Moves a card to the end of the column to the left or the right of its
    /// own.
    pub(super) fn move_kanban_card_across(
        &mut self,
        column: usize,
        index: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        self.sync_kanban(cx);
        let Some(form) = self.kanban_mut() else {
            return;
        };
        let column_count = form.columns.len().min(form.document.columns.len());
        let Some(target) = neighbour(column, forward, column_count) else {
            return;
        };
        let movable = form
            .document
            .columns
            .get(column)
            .zip(form.columns.get(column))
            .is_some_and(|(cards, rows)| index < cards.cards.len() && index < rows.cards.len());
        if !movable || column >= column_count {
            return;
        }
        let Some(card) = form
            .document
            .columns
            .get_mut(column)
            .map(|cards| cards.cards.remove(index))
        else {
            return;
        };
        let Some(row) = form
            .columns
            .get_mut(column)
            .map(|rows| rows.cards.remove(index))
        else {
            return;
        };
        if let (Some(cards), Some(rows)) = (
            form.document.columns.get_mut(target),
            form.columns.get_mut(target),
        ) {
            cards.cards.push(card);
            rows.cards.push(row);
        }
        self.kanban_changed(cx);
    }

    pub(super) fn set_kanban_priority(
        &mut self,
        column: usize,
        index: usize,
        priority: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        self.sync_kanban(cx);
        let Some(card) = self
            .kanban_mut()
            .and_then(|form| form.document.columns.get_mut(column)?.cards.get_mut(index))
        else {
            return;
        };
        card.set_metadata_value(PRIORITY_KEY, priority);
        self.kanban_changed(cx);
    }

    pub(super) fn render_kanban(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(form) = self.kanban() else {
            return Vec::new();
        };
        let current = form.current_document(cx);
        let ids = current
            .nodes()
            .map(|node| node.id.clone())
            .collect::<Vec<_>>();
        let is_unique = |id: &str| ids.iter().filter(|other| other.as_str() == id).count() <= 1;

        let mut list = v_flex().w_full().gap_2().child(section_title(i18n::t!(
            "Columns and Cards (columns higher up are drawn further left)"
        )));
        let column_count = form.columns.len();
        for index in 0..column_count {
            let (Some(row), Some(column)) = (form.columns.get(index), current.columns.get(index))
            else {
                continue;
            };
            let mut cards = v_flex().w_full().gap_2().pl_4();
            for card_index in 0..row.cards.len() {
                let (Some(card_row), Some(card)) =
                    (row.cards.get(card_index), column.cards.get(card_index))
                else {
                    continue;
                };
                cards = cards.child(self.render_kanban_card(
                    index,
                    card_index,
                    row.cards.len(),
                    column_count,
                    card_row,
                    card,
                    is_unique(&card.id),
                    window,
                    cx,
                ));
            }
            cards = cards.child(add_button(
                SharedString::from(format!("mermaid-kanban-add-card-{index}")),
                i18n::t!("Add Card"),
                cx.listener(move |this, _, window, cx| this.add_kanban_card(index, window, cx)),
            ));

            let column_button = |name: &str, icon: IconName, tooltip: &'static str, disabled| {
                IconButton::new(
                    SharedString::from(format!("mermaid-kanban-column-{name}-{index}")),
                    icon,
                )
                .shape(IconButtonShape::Square)
                .icon_size(IconSize::XSmall)
                .disabled(disabled)
                .tooltip(Tooltip::text(tooltip))
            };
            let controls =
                h_flex()
                    .flex_none()
                    .gap_px()
                    .child(
                        column_button(
                            "left",
                            IconName::ArrowLeft,
                            i18n::t!("Move Left"),
                            index == 0,
                        )
                        .on_click(cx.listener(
                            move |this, _, _window, cx| this.move_kanban_column(index, false, cx),
                        )),
                    )
                    .child(
                        column_button(
                            "right",
                            IconName::ArrowRight,
                            i18n::t!("Move Right"),
                            index + 1 >= column_count,
                        )
                        .on_click(cx.listener(
                            move |this, _, _window, cx| this.move_kanban_column(index, true, cx),
                        )),
                    )
                    .child(
                        column_button(
                            "remove",
                            IconName::Trash,
                            i18n::t!("Delete Column and Its Cards"),
                            false,
                        )
                        .on_click(cx.listener(
                            move |this, _, _window, cx| this.remove_kanban_column(index, cx),
                        )),
                    );
            list = list.child(
                v_flex()
                    .w_full()
                    .gap_1()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .child(section_title(i18n::t!("Column")))
                            .child(
                                field_frame(
                                    &row.id,
                                    ID_FIELD_WIDTH,
                                    is_valid_kanban_id(&column.node.id)
                                        && is_unique(&column.node.id),
                                    window,
                                    cx,
                                )
                                .child(row.id.clone()),
                            )
                            .child(
                                field_frame(
                                    &row.title,
                                    LABEL_FIELD_WIDTH,
                                    is_valid_kanban_title(&column.node),
                                    window,
                                    cx,
                                )
                                .flex_1()
                                .child(row.title.clone()),
                            )
                            .child(controls),
                    )
                    .child(cards),
            );
        }

        list = list.child(add_button(
            "mermaid-kanban-add-column",
            i18n::t!("Add Column"),
            cx.listener(|this, _, window, cx| this.add_kanban_column(window, cx)),
        ));
        vec![list.into_any_element()]
    }

    #[allow(clippy::too_many_arguments)]
    fn render_kanban_card(
        &self,
        column: usize,
        index: usize,
        length: usize,
        column_count: usize,
        row: &CardRow,
        card: &KanbanNode,
        id_is_unique: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let button = |name: &str, icon: IconName, tooltip: &'static str, disabled: bool| {
            IconButton::new(
                SharedString::from(format!("mermaid-kanban-card-{name}-{column}-{index}")),
                icon,
            )
            .shape(IconButtonShape::Square)
            .icon_size(IconSize::XSmall)
            .disabled(disabled)
            .tooltip(Tooltip::text(tooltip))
        };
        let controls = h_flex()
            .flex_none()
            .gap_px()
            .child(
                button(
                    "left",
                    IconName::ArrowLeft,
                    i18n::t!("Move to Left Column"),
                    column == 0,
                )
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.move_kanban_card_across(column, index, false, cx)
                })),
            )
            .child(
                button(
                    "right",
                    IconName::ArrowRight,
                    i18n::t!("Move to Right Column"),
                    column + 1 >= column_count,
                )
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.move_kanban_card_across(column, index, true, cx)
                })),
            )
            .child(
                button("up", IconName::ArrowUp, i18n::t!("Move Up"), index == 0).on_click(
                    cx.listener(move |this, _, _window, cx| {
                        this.move_kanban_card(column, index, false, cx)
                    }),
                ),
            )
            .child(
                button(
                    "down",
                    IconName::ArrowDown,
                    i18n::t!("Move Down"),
                    index + 1 >= length,
                )
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.move_kanban_card(column, index, true, cx)
                })),
            )
            .child(
                button("remove", IconName::Trash, i18n::t!("Delete Card"), false).on_click(
                    cx.listener(move |this, _, _window, cx| {
                        this.remove_kanban_card(column, index, cx)
                    }),
                ),
            );

        let assigned = card.metadata_value(ASSIGNED_KEY).unwrap_or_default();
        let ticket = card.metadata_value(TICKET_KEY).unwrap_or_default();
        v_flex()
            .w_full()
            .gap_1()
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        field_frame(
                            &row.id,
                            ID_FIELD_WIDTH,
                            is_valid_kanban_id(&card.id) && id_is_unique,
                            window,
                            cx,
                        )
                        .child(row.id.clone()),
                    )
                    .child(
                        field_frame(
                            &row.title,
                            LABEL_FIELD_WIDTH,
                            is_valid_kanban_title(card),
                            window,
                            cx,
                        )
                        .flex_1()
                        .child(row.title.clone()),
                    )
                    .child(controls),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .pl_4()
                    .child(section_title(i18n::t!("Assignee")))
                    .child(
                        field_frame(
                            &row.assigned,
                            ID_FIELD_WIDTH,
                            is_valid_kanban_metadata_value(&assigned),
                            window,
                            cx,
                        )
                        .child(row.assigned.clone()),
                    )
                    .child(section_title(i18n::t!("Ticket")))
                    .child(
                        field_frame(
                            &row.ticket,
                            ID_FIELD_WIDTH,
                            is_valid_kanban_metadata_value(&ticket),
                            window,
                            cx,
                        )
                        .child(row.ticket.clone()),
                    )
                    .child(self.render_kanban_priority_menu(
                        column,
                        index,
                        card.metadata_value(PRIORITY_KEY),
                        window,
                        cx,
                    )),
            )
            .into_any_element()
    }

    fn render_kanban_priority_menu(
        &self,
        column: usize,
        index: usize,
        current: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let label = priority_label(current.as_deref());
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            let choices = std::iter::once((None, i18n::t!("None"))).chain(
                KANBAN_PRIORITIES
                    .iter()
                    .map(|(value, label)| (Some(*value), i18n::tr(label))),
            );
            for (value, label) in choices {
                let this = this.clone();
                menu = menu.toggleable_entry(
                    label,
                    current.as_deref() == value,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        this.update(cx, |this, cx| {
                            this.set_kanban_priority(column, index, value, cx);
                        })
                        .log_err();
                    },
                );
            }
            menu
        });

        DropdownMenu::new(
            SharedString::from(format!("mermaid-kanban-priority-{column}-{index}")),
            label,
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
    }

    pub(super) fn render_kanban_toolbar(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> AnyElement {
        div().into_any_element()
    }
}

#[cfg(test)]
#[path = "kanban_tests.rs"]
mod kanban_tests;
