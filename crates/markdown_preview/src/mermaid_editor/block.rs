//! The block diagram half of the Mermaid editor's form.
//!
//! A block diagram is laid out from the order its blocks are written in, so
//! the form's main list is that order: one row per block, empty cell, arrow
//! block, group or comment, pushed right when it sits inside a group. The rows
//! move with their children, and go into or out of a group with the arrow
//! buttons. The edges and the styling statements refer to blocks by id, so
//! their ends are picked from menus, and renaming a block carries into them.
//! The operations on the outline live in [`mermaid_render::block_model`],
//! whose tests pin them down without a window.

use editor::Editor;
use gpui::{AnyElement, App, Context, Entity, Focusable as _, Window, rems};
use mermaid_render::block_model::{
    BLOCK_LINKS, BlockArrow, BlockArrowDirection, BlockColumns, BlockDocument, BlockEdge,
    BlockEntry, BlockGroup, BlockItem, BlockItemKind, BlockNode, BlockOutlineItem, BlockShape,
    BlockStatement, arrow_directions_text, block_can_indent, block_indent, block_outdent,
    block_release_children, block_remove, convert_block_item, is_valid_arrow_label,
    is_valid_block_class_name, is_valid_block_comment, is_valid_block_css, is_valid_block_id,
    is_valid_block_label, is_valid_edge_label, parse_arrow_directions, parse_block_width,
};
use mermaid_render::mindmap_model::{
    OutlineItem, outline_move, outline_next_sibling, outline_previous_sibling,
};
use ui::{
    ContextMenu, DropdownMenu, DropdownStyle, IconButtonShape, IconPosition, Tooltip, prelude::*,
};
use util::ResultExt as _;

use super::{
    Form, ID_FIELD_WIDTH, LABEL_FIELD_WIDTH, MermaidEditor, NUMBER_FIELD_WIDTH, add_button,
    field_frame, row_controls, section_title,
};

/// The texts new rows are added with, so that they are visible in the drawing
/// before anything has been typed into them.
const NEW_BLOCK_LABEL: &str = i18n::msg!("New block");
const NEW_ARROW_LABEL: &str = i18n::msg!("Arrow");
const NEW_STYLE: &str = "fill:#f9f,stroke:#333";

/// How far each level of groups pushes its rows to the right.
const DEPTH_INDENT_REMS: f32 = 1.;

pub(super) struct BlockForm {
    /// Everything but the entries, which live in the rows. The statements'
    /// references are the ids as the rows' entries spell them.
    pub(super) document: BlockDocument,
    pub(super) columns: Entity<Editor>,
    pub(super) entries: Vec<EntryRow>,
    /// One per statement of `document`, including the comments, which have no
    /// fields.
    pub(super) statements: Vec<StatementRow>,
}

pub(super) struct EntryRow {
    /// The entry as it was last synced from the fields. The kind, the shape,
    /// the depth and the line it shares live only here.
    pub(super) entry: BlockEntry,
    pub(super) id: Entity<Editor>,
    pub(super) label: Entity<Editor>,
    pub(super) width: Entity<Editor>,
    /// An arrow's directions, a group's columns or a comment's text.
    pub(super) extra: Entity<Editor>,
}

impl OutlineItem for EntryRow {
    fn depth(&self) -> usize {
        self.entry.depth
    }

    fn set_depth(&mut self, depth: usize) {
        self.entry.depth = depth;
    }
}

impl BlockOutlineItem for EntryRow {
    fn is_group(&self) -> bool {
        matches!(self.entry.item, BlockItem::Group(_))
    }
}

/// The fields of one statement: an edge's label, a `classDef`'s name and
/// style, a `class`'s name, a `style`'s style.
pub(super) struct StatementRow {
    pub(super) first: Option<Entity<Editor>>,
    pub(super) second: Option<Entity<Editor>>,
}

fn field_text(field: &Entity<Editor>, cx: &App) -> String {
    field.read(cx).text(cx)
}

fn trimmed(field: &Entity<Editor>, cx: &App) -> String {
    field_text(field, cx).trim().to_string()
}

/// A width field's value. Text that is not a width is held as a zero width,
/// which the document refuses with the reason.
fn width_value(field: &Entity<Editor>, cx: &App) -> Option<u32> {
    parse_block_width(&field_text(field, cx)).unwrap_or(Some(0))
}

/// A columns field's value: empty for none, and zero for text that is not a
/// column count, which the document refuses.
fn columns_value(text: &str) -> Option<BlockColumns> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(BlockColumns::from_text(text).unwrap_or(BlockColumns::Count(0)))
}

fn is_valid_columns_text(text: &str) -> bool {
    let text = text.trim();
    text.is_empty() || BlockColumns::from_text(text).is_some()
}

impl EntryRow {
    /// The entry this row describes, with the fields' current text.
    fn current_entry(&self, cx: &App) -> BlockEntry {
        let mut entry = self.entry.clone();
        match &mut entry.item {
            BlockItem::Node(node) => {
                node.id = trimmed(&self.id, cx);
                node.label = field_text(&self.label, cx);
                node.width = width_value(&self.width, cx);
            }
            BlockItem::Arrow(arrow) => {
                arrow.id = trimmed(&self.id, cx);
                arrow.label = field_text(&self.label, cx);
                arrow.directions =
                    parse_arrow_directions(&field_text(&self.extra, cx)).unwrap_or_default();
                arrow.width = width_value(&self.width, cx);
            }
            BlockItem::Space { width } => *width = width_value(&self.width, cx),
            BlockItem::Group(group) => {
                let id = trimmed(&self.id, cx);
                group.id = (!id.is_empty()).then_some(id);
                let label = field_text(&self.label, cx);
                group.label = (!label.is_empty()).then_some(label);
                group.width = width_value(&self.width, cx);
                group.columns = columns_value(&field_text(&self.extra, cx));
            }
            BlockItem::Comment(text) => *text = trimmed(&self.extra, cx),
        }
        entry
    }
}

impl BlockForm {
    pub(super) fn fields(&self) -> Vec<Entity<Editor>> {
        let entries = self.entries.iter().flat_map(|row| {
            let fields: Vec<&Entity<Editor>> = match &row.entry.item {
                BlockItem::Node(node) if node.shape.is_some() => {
                    vec![&row.id, &row.label, &row.width]
                }
                BlockItem::Node(_) => vec![&row.id, &row.width],
                BlockItem::Arrow(_) => vec![&row.id, &row.label, &row.extra, &row.width],
                BlockItem::Space { .. } => vec![&row.width],
                BlockItem::Group(_) => vec![&row.id, &row.label, &row.width, &row.extra],
                BlockItem::Comment(_) => vec![&row.extra],
            };
            fields.into_iter().cloned().collect::<Vec<_>>()
        });
        let statements = self
            .statements
            .iter()
            .flat_map(|row| row.first.iter().chain(row.second.iter()).cloned());
        std::iter::once(self.columns.clone())
            .chain(entries)
            .chain(statements)
            .collect()
    }

    /// The block diagram the form currently describes, with every statement
    /// pointed at the ids as the fields spell them now.
    pub(super) fn current_document(&self, cx: &App) -> BlockDocument {
        self.document_with_renames(cx, |_, _| true).0
    }

    /// The document the fields describe, with the statements carried along
    /// only by the renames `follow` accepts. Also returns, per row, whether
    /// its id was renamed without the statements following.
    fn document_with_renames(
        &self,
        cx: &App,
        follow: impl Fn(&str, &str) -> bool,
    ) -> (BlockDocument, Vec<bool>) {
        let mut document = self.document.clone();
        document.columns = columns_value(&field_text(&self.columns, cx));
        let mut renames = Vec::new();
        let mut held_back = Vec::with_capacity(self.entries.len());
        document.entries = self
            .entries
            .iter()
            .map(|row| {
                let entry = row.current_entry(cx);
                let held = match (row.entry.item.id(), entry.item.id()) {
                    (Some(old), Some(new)) if old != new => {
                        if follow(old, new) {
                            renames.push((old.to_string(), new.to_string()));
                            false
                        } else {
                            true
                        }
                    }
                    (Some(_), None) => true,
                    _ => false,
                };
                held_back.push(held);
                entry
            })
            .collect();
        for (statement, row) in document.statements.iter_mut().zip(&self.statements) {
            let first = row.first.as_ref().map(|field| field_text(field, cx));
            let second = row.second.as_ref().map(|field| field_text(field, cx));
            match statement {
                BlockStatement::Edge(edge) => {
                    edge.label = first.filter(|label| !label.is_empty());
                }
                BlockStatement::ClassDef { name, css } => {
                    if let Some(first) = first {
                        *name = first.trim().to_string();
                    }
                    if let Some(second) = second {
                        *css = second.trim().to_string();
                    }
                }
                BlockStatement::Class { class_name, .. } => {
                    if let Some(first) = first {
                        *class_name = first.trim().to_string();
                    }
                }
                BlockStatement::Style { css, .. } => {
                    if let Some(first) = first {
                        *css = first.trim().to_string();
                    }
                }
                BlockStatement::Comment(_) => {}
            }
        }
        document.rename_references(&renames);
        (document, held_back)
    }
}

/// The first thing wrong with what the user typed, in the words shown above the
/// form. Every check lives in `BlockDocument::structure_error`, which the
/// caller consults right after this; the red frames come from the
/// `is_valid_block_*` predicates directly.
pub(super) fn problem(_document: &BlockDocument) -> Option<SharedString> {
    None
}

fn shape_label(shape: Option<BlockShape>) -> &'static str {
    match shape {
        Some(shape) => shape.label(),
        None => i18n::t!("No Shape (Shows the ID)"),
    }
}

fn link_label(link: &str) -> SharedString {
    match BLOCK_LINKS.iter().find(|(spelling, _)| *spelling == link) {
        Some((spelling, name)) => format!("{spelling} {}", i18n::tr(name)).into(),
        None => link.to_string().into(),
    }
}

impl MermaidEditor {
    pub(super) fn new_block_form(
        &mut self,
        document: BlockDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> BlockForm {
        let columns = self.new_field(
            &document
                .columns
                .map(|columns| columns.text())
                .unwrap_or_default(),
            window,
            cx,
        );
        let mut entries = Vec::with_capacity(document.entries.len());
        for entry in &document.entries {
            entries.push(self.new_block_row(entry.clone(), window, cx));
        }
        let mut statements = Vec::with_capacity(document.statements.len());
        for statement in &document.statements {
            statements.push(self.new_statement_row(statement, window, cx));
        }
        BlockForm {
            document: BlockDocument {
                entries: Vec::new(),
                ..document
            },
            columns,
            entries,
            statements,
        }
    }

    fn new_block_row(
        &mut self,
        entry: BlockEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> EntryRow {
        let (id, label, width, extra) = match &entry.item {
            BlockItem::Node(node) => (
                node.id.clone(),
                node.label.clone(),
                node.width,
                String::new(),
            ),
            BlockItem::Arrow(arrow) => (
                arrow.id.clone(),
                arrow.label.clone(),
                arrow.width,
                arrow_directions_text(&arrow.directions),
            ),
            BlockItem::Space { width } => (String::new(), String::new(), *width, String::new()),
            BlockItem::Group(group) => (
                group.id.clone().unwrap_or_default(),
                group.label.clone().unwrap_or_default(),
                group.width,
                group
                    .columns
                    .map(|columns| columns.text())
                    .unwrap_or_default(),
            ),
            BlockItem::Comment(text) => (String::new(), String::new(), None, text.clone()),
        };
        let width = width.map(|width| width.to_string()).unwrap_or_default();
        EntryRow {
            entry,
            id: self.new_field(&id, window, cx),
            label: self.new_field(&label, window, cx),
            width: self.new_field(&width, window, cx),
            extra: self.new_field(&extra, window, cx),
        }
    }

    fn new_statement_row(
        &mut self,
        statement: &BlockStatement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> StatementRow {
        let (first, second) = match statement {
            BlockStatement::Edge(edge) => (Some(edge.label.clone().unwrap_or_default()), None),
            BlockStatement::ClassDef { name, css } => (Some(name.clone()), Some(css.clone())),
            BlockStatement::Class { class_name, .. } => (Some(class_name.clone()), None),
            BlockStatement::Style { css, .. } => (Some(css.clone()), None),
            BlockStatement::Comment(_) => (None, None),
        };
        StatementRow {
            first: first.map(|text| self.new_field(&text, window, cx)),
            second: second.map(|text| self.new_field(&text, window, cx)),
        }
    }

    pub(super) fn block_form(&self) -> Option<&BlockForm> {
        match &self.form {
            Form::Block(form) => Some(form),
            _ => None,
        }
    }

    pub(super) fn block_form_mut(&mut self) -> Option<&mut BlockForm> {
        match &mut self.form {
            Form::Block(form) => Some(form),
            _ => None,
        }
    }

    /// Reads the fields into the rows and the statements before a change that
    /// picks an id or moves rows, so that a rename typed a moment ago has
    /// already reached every statement that names the block.
    ///
    /// A rename still being typed — to no id, to one that cannot be written, or
    /// to one another row has — is left out: carried into the statements now,
    /// the statements of the two blocks would be merged, and typing the id
    /// back would take both along. The row keeps its old id until the rename
    /// settles.
    fn sync_block(&mut self, cx: &mut Context<Self>) {
        let Some((document, held_back)) = self.block_form().map(|form| {
            let typed = form.current_document(cx).ids();
            form.document_with_renames(cx, |_, new| {
                is_valid_block_id(new) && typed.iter().filter(|id| *id == new).count() == 1
            })
        }) else {
            return;
        };
        let Some(form) = self.block_form_mut() else {
            return;
        };
        for ((row, entry), held) in form
            .entries
            .iter_mut()
            .zip(document.entries.iter())
            .zip(held_back)
        {
            let mut entry = entry.clone();
            if held {
                let old = row.entry.item.id().map(str::to_string);
                match &mut entry.item {
                    BlockItem::Node(BlockNode { id, .. })
                    | BlockItem::Arrow(BlockArrow { id, .. }) => {
                        if let Some(old) = old {
                            *id = old;
                        }
                    }
                    BlockItem::Group(group) => group.id = old,
                    BlockItem::Space { .. } | BlockItem::Comment(_) => {}
                }
            }
            row.entry = entry;
        }
        form.document = BlockDocument {
            entries: Vec::new(),
            ..document
        };
    }

    fn block_changed(&mut self, cx: &mut Context<Self>) {
        self.schedule_preview(cx);
        cx.notify();
    }

    /// The ids the form offers for edges and styles: the ones the fields hold
    /// now that could be written as an id.
    fn block_ids(&self, cx: &App) -> Vec<String> {
        self.block_form()
            .map(|form| {
                form.current_document(cx)
                    .ids()
                    .into_iter()
                    .filter(|id| is_valid_block_id(id))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(super) fn add_block_item(
        &mut self,
        kind: BlockItemKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_block(cx);
        let Some(form) = self.block_form() else {
            return;
        };
        let current = form.current_document(cx);
        let id = current.unused_id(kind.id_prefix());
        let item = match kind {
            BlockItemKind::Node => BlockItem::Node(BlockNode {
                id,
                shape: Some(BlockShape::Square),
                label: i18n::tr(NEW_BLOCK_LABEL).to_string(),
                width: None,
            }),
            BlockItemKind::Arrow => BlockItem::Arrow(BlockArrow {
                id,
                label: i18n::tr(NEW_ARROW_LABEL).to_string(),
                directions: vec![BlockArrowDirection::Right],
                width: None,
            }),
            BlockItemKind::Space => BlockItem::Space { width: None },
            BlockItemKind::Group => BlockItem::Group(BlockGroup {
                id: Some(id),
                label: None,
                width: None,
                columns: None,
            }),
        };
        let row = self.new_block_row(BlockEntry::new(0, item), window, cx);
        let focus = if kind == BlockItemKind::Space {
            row.width.clone()
        } else {
            row.id.clone()
        };
        if let Some(form) = self.block_form_mut() {
            form.entries.push(row);
        }
        focus.focus_handle(cx).focus(window, cx);
        self.block_changed(cx);
    }

    /// Removes the row at `index` with the rows inside it, and every statement
    /// that would be left naming one of them.
    pub(super) fn remove_block_entry(&mut self, index: usize, cx: &mut Context<Self>) {
        self.sync_block(cx);
        let Some(form) = self.block_form_mut() else {
            return;
        };
        let removed = block_remove(&mut form.entries, index);
        if removed.is_empty() {
            return;
        }
        let ids = removed
            .iter()
            .filter_map(|row| row.entry.item.id().map(str::to_string))
            .collect::<Vec<_>>();
        for statement in form.document.remove_references(&ids).into_iter().rev() {
            if statement < form.statements.len() {
                form.statements.remove(statement);
            }
        }
        self.block_changed(cx);
    }

    pub(super) fn move_block_entry(&mut self, index: usize, forward: bool, cx: &mut Context<Self>) {
        let moved = self
            .block_form_mut()
            .and_then(|form| outline_move(&mut form.entries, index, forward));
        if moved.is_some() {
            self.block_changed(cx);
        }
    }

    pub(super) fn indent_block_entry(&mut self, index: usize, cx: &mut Context<Self>) {
        let indented = self
            .block_form_mut()
            .is_some_and(|form| block_indent(&mut form.entries, index));
        if indented {
            self.block_changed(cx);
        }
    }

    pub(super) fn outdent_block_entry(&mut self, index: usize, cx: &mut Context<Self>) {
        let outdented = self
            .block_form_mut()
            .and_then(|form| block_outdent(&mut form.entries, index));
        if outdented.is_some() {
            self.block_changed(cx);
        }
    }

    /// Turns the row at `index` into an item of `kind`. A group that stops
    /// being one lets its children out where they are.
    pub(super) fn set_block_kind(
        &mut self,
        index: usize,
        kind: BlockItemKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_block(cx);
        let Some(form) = self.block_form() else {
            return;
        };
        let Some(entry) = form.entries.get(index).map(|row| row.entry.clone()) else {
            return;
        };
        if BlockItemKind::of(&entry.item) == Some(kind) {
            return;
        }
        let fallback = form.current_document(cx).unused_id(kind.id_prefix());
        let converted = BlockEntry {
            item: convert_block_item(&entry.item, kind, fallback),
            ..entry.clone()
        };
        let row = self.new_block_row(converted, window, cx);
        let Some(form) = self.block_form_mut() else {
            return;
        };
        if matches!(entry.item, BlockItem::Group(_)) {
            block_release_children(&mut form.entries, index);
        }
        if let Some(slot) = form.entries.get_mut(index) {
            *slot = row;
        }
        self.block_changed(cx);
    }

    pub(super) fn set_block_shape(
        &mut self,
        index: usize,
        shape: Option<BlockShape>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_block(cx);
        let Some(row) = self
            .block_form_mut()
            .and_then(|form| form.entries.get_mut(index))
        else {
            return;
        };
        let BlockItem::Node(node) = &mut row.entry.item else {
            return;
        };
        node.shape = shape;
        // A block that had no brackets was drawn with its id, so that is what
        // it keeps showing until a label is typed.
        if shape.is_some() && node.label.is_empty() {
            node.label = node.id.clone();
            let (label, text) = (row.label.clone(), node.id.clone());
            label.update(cx, |label, cx| label.set_text(text, window, cx));
        }
        self.block_changed(cx);
    }

    pub(super) fn add_block_statement(
        &mut self,
        statement: BlockStatement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_block(cx);
        let row = self.new_statement_row(&statement, window, cx);
        let focus = row.first.clone();
        let Some(form) = self.block_form_mut() else {
            return;
        };
        form.document.statements.push(statement);
        form.statements.push(row);
        if let Some(focus) = focus {
            focus.focus_handle(cx).focus(window, cx);
        }
        self.block_changed(cx);
    }

    pub(super) fn add_block_edge(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ids = self.block_ids(cx);
        let Some(from) = ids.first().cloned() else {
            self.message = Some(i18n::t!("Add a block first.").into());
            cx.notify();
            return;
        };
        let to = ids.get(1).cloned().unwrap_or_else(|| from.clone());
        self.add_block_statement(
            BlockStatement::Edge(BlockEdge {
                from,
                to,
                label: None,
                link: "-->".to_string(),
            }),
            window,
            cx,
        );
    }

    pub(super) fn add_block_style(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.block_ids(cx).first().cloned() else {
            self.message = Some(i18n::t!("Add a block first.").into());
            cx.notify();
            return;
        };
        self.add_block_statement(
            BlockStatement::Style {
                target,
                css: NEW_STYLE.to_string(),
            },
            window,
            cx,
        );
    }

    fn block_class_names(&self, cx: &App) -> Vec<String> {
        self.block_form()
            .map(|form| {
                form.current_document(cx)
                    .statements
                    .iter()
                    .filter_map(|statement| match statement {
                        BlockStatement::ClassDef { name, .. } => Some(name.clone()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(super) fn add_block_class_def(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let taken = self.block_class_names(cx);
        let name = (1..=taken.len() + 1)
            .map(|number| format!("class{number}"))
            .find(|name| !taken.contains(name))
            .unwrap_or_else(|| "class1".to_string());
        self.add_block_statement(
            BlockStatement::ClassDef {
                name,
                css: NEW_STYLE.to_string(),
            },
            window,
            cx,
        );
    }

    pub(super) fn add_block_class(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.block_ids(cx).first().cloned() else {
            self.message = Some(i18n::t!("Add a block first.").into());
            cx.notify();
            return;
        };
        let class_name = self
            .block_class_names(cx)
            .first()
            .cloned()
            .unwrap_or_else(|| "class1".to_string());
        self.add_block_statement(
            BlockStatement::Class {
                targets: vec![target],
                class_name,
            },
            window,
            cx,
        );
    }

    pub(super) fn remove_block_statement(&mut self, index: usize, cx: &mut Context<Self>) {
        self.sync_block(cx);
        let Some(form) = self.block_form_mut() else {
            return;
        };
        if index >= form.statements.len() || index >= form.document.statements.len() {
            return;
        }
        form.document.statements.remove(index);
        form.statements.remove(index);
        self.block_changed(cx);
    }

    /// Swaps the edge at `index` with the edge before or after it, stepping
    /// over the statements of other kinds between them.
    pub(super) fn move_block_edge(&mut self, index: usize, forward: bool, cx: &mut Context<Self>) {
        self.sync_block(cx);
        let Some(form) = self.block_form_mut() else {
            return;
        };
        let is_edge = |statement: &BlockStatement| matches!(statement, BlockStatement::Edge(_));
        let other = if forward {
            form.document
                .statements
                .iter()
                .enumerate()
                .skip(index + 1)
                .find(|(_, statement)| is_edge(statement))
                .map(|(other, _)| other)
        } else {
            form.document
                .statements
                .iter()
                .enumerate()
                .take(index)
                .rev()
                .find(|(_, statement)| is_edge(statement))
                .map(|(other, _)| other)
        };
        let Some(other) = other else {
            return;
        };
        if index >= form.statements.len() || other >= form.statements.len() {
            return;
        }
        form.document.statements.swap(index, other);
        form.statements.swap(index, other);
        self.block_changed(cx);
    }

    pub(super) fn update_block_statement(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut BlockStatement),
    ) {
        self.sync_block(cx);
        let Some(statement) = self
            .block_form_mut()
            .and_then(|form| form.document.statements.get_mut(index))
        else {
            return;
        };
        edit(statement);
        self.block_changed(cx);
    }

    pub(super) fn render_block_toolbar(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(form) = self.block_form() else {
            return div().into_any_element();
        };
        let text = field_text(&form.columns, cx);
        h_flex()
            .gap_2()
            .child(section_title(i18n::t!("Number of Columns")))
            .child(
                field_frame(
                    &form.columns,
                    NUMBER_FIELD_WIDTH,
                    is_valid_columns_text(&text),
                    window,
                    cx,
                )
                .child(form.columns.clone()),
            )
            .child(section_title(i18n::t!(
                "Leave empty to put everything in one row (auto also works)"
            )))
            .into_any_element()
    }

    pub(super) fn render_block(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(form) = self.block_form() else {
            return Vec::new();
        };
        let current = form.current_document(cx);
        let ids = current
            .ids()
            .into_iter()
            .filter(|id| is_valid_block_id(id))
            .collect::<Vec<_>>();
        vec![
            self.render_block_entries(form, window, cx),
            self.render_block_edges(form, &current, &ids, window, cx),
            self.render_block_styles(form, &current, &ids, window, cx),
        ]
    }

    fn render_block_entries(
        &self,
        form: &BlockForm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut list = v_flex().w_full().gap_2().child(section_title(i18n::t!(
            "Blocks (laid out from the top left in the order written; indented rows are nested deeper in groups)"
        )));
        for index in 0..form.entries.len() {
            if let Some(row) = form.entries.get(index) {
                list = list.child(self.render_block_row(form, index, row, window, cx));
            }
        }
        let mut buttons = h_flex().w_full().flex_wrap().gap_1();
        for kind in BlockItemKind::ALL {
            buttons = buttons.child(add_button(
                SharedString::from(format!("mermaid-block-add-{}", kind.id_prefix())),
                i18n::t!("Add {label}", label = kind.label()),
                cx.listener(move |this, _, window, cx| this.add_block_item(kind, window, cx)),
            ));
        }
        list.child(buttons).into_any_element()
    }

    fn render_block_row(
        &self,
        form: &BlockForm,
        index: usize,
        row: &EntryRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entry = row.current_entry(cx);
        let depth = entry.depth;
        let is_group = matches!(entry.item, BlockItem::Group(_));
        let can_indent = block_can_indent(&form.entries, index);
        let can_move_up = outline_previous_sibling(&form.entries, index).is_some();
        let can_move_down = outline_next_sibling(&form.entries, index).is_some();

        let button = |name: &str, icon: IconName, tooltip: &'static str, disabled: bool| {
            IconButton::new(
                SharedString::from(format!("mermaid-block-{name}-{index}")),
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
                    "outdent",
                    IconName::ArrowLeft,
                    i18n::t!("Move Out of the Group (After the Group)"),
                    depth == 0,
                )
                .on_click(
                    cx.listener(move |this, _, _window, cx| this.outdent_block_entry(index, cx)),
                ),
            )
            .child(
                button(
                    "indent",
                    IconName::ArrowRight,
                    i18n::t!("Move into the Group Above"),
                    !can_indent,
                )
                .on_click(
                    cx.listener(move |this, _, _window, cx| this.indent_block_entry(index, cx)),
                ),
            )
            .child(
                button("up", IconName::ArrowUp, i18n::t!("Move Up"), !can_move_up).on_click(
                    cx.listener(move |this, _, _window, cx| {
                        this.move_block_entry(index, false, cx)
                    }),
                ),
            )
            .child(
                button(
                    "down",
                    IconName::ArrowDown,
                    i18n::t!("Move Down"),
                    !can_move_down,
                )
                .on_click(
                    cx.listener(move |this, _, _window, cx| this.move_block_entry(index, true, cx)),
                ),
            )
            .child(
                button(
                    "remove",
                    IconName::Trash,
                    if is_group {
                        i18n::t!(
                            "Delete This Group and Its Contents (connected edges and styles are deleted too)"
                        )
                    } else {
                        i18n::t!("Delete (connected edges and styles are deleted too)")
                    },
                    false,
                )
                .on_click(
                    cx.listener(move |this, _, _window, cx| this.remove_block_entry(index, cx)),
                ),
            );

        let id_field = |is_valid: bool, window: &mut Window, cx: &mut Context<Self>| {
            field_frame(&row.id, ID_FIELD_WIDTH, is_valid, window, cx).child(row.id.clone())
        };
        let width_fields = |window: &mut Window, cx: &mut Context<Self>| {
            h_flex()
                .flex_none()
                .gap_1()
                .child(section_title(i18n::t!("Width")))
                .child(
                    field_frame(
                        &row.width,
                        NUMBER_FIELD_WIDTH,
                        parse_block_width(&field_text(&row.width, cx)).is_some(),
                        window,
                        cx,
                    )
                    .child(row.width.clone()),
                )
        };

        let mut first_line = h_flex().w_full().gap_1();
        let mut second_line = None;
        match &entry.item {
            BlockItem::Node(node) => {
                first_line = first_line
                    .child(self.render_block_kind_menu(index, BlockItemKind::Node, window, cx))
                    .child(id_field(is_valid_block_id(&node.id), window, cx));
                first_line = if node.shape.is_some() {
                    first_line.child(
                        field_frame(
                            &row.label,
                            LABEL_FIELD_WIDTH,
                            is_valid_block_label(&node.label),
                            window,
                            cx,
                        )
                        .flex_1()
                        .child(row.label.clone()),
                    )
                } else {
                    first_line.child(
                        div()
                            .flex_1()
                            .child(section_title(i18n::t!("Shows the ID as is"))),
                    )
                };
                second_line = Some(
                    h_flex()
                        .w_full()
                        .gap_1()
                        .pl_4()
                        .child(self.render_block_shape_menu(index, node.shape, window, cx))
                        .child(width_fields(window, cx)),
                );
            }
            BlockItem::Arrow(arrow) => {
                first_line = first_line
                    .child(self.render_block_kind_menu(index, BlockItemKind::Arrow, window, cx))
                    .child(id_field(is_valid_block_id(&arrow.id), window, cx))
                    .child(
                        field_frame(
                            &row.label,
                            LABEL_FIELD_WIDTH,
                            is_valid_arrow_label(&arrow.label),
                            window,
                            cx,
                        )
                        .flex_1()
                        .child(row.label.clone()),
                    );
                second_line = Some(
                    h_flex()
                        .w_full()
                        .gap_1()
                        .pl_4()
                        .child(section_title(i18n::tc!("mermaid block arrow", "Direction")))
                        .child(
                            field_frame(
                                &row.extra,
                                ID_FIELD_WIDTH,
                                !arrow.directions.is_empty(),
                                window,
                                cx,
                            )
                            .child(row.extra.clone()),
                        )
                        .child(section_title(i18n::t!(
                            "right / left / up / down / x / y, separated by commas"
                        )))
                        .child(width_fields(window, cx)),
                );
            }
            BlockItem::Space { .. } => {
                first_line = first_line
                    .child(self.render_block_kind_menu(index, BlockItemKind::Space, window, cx))
                    .child(width_fields(window, cx))
                    .child(div().flex_1());
            }
            BlockItem::Group(group) => {
                first_line = first_line
                    .child(self.render_block_kind_menu(index, BlockItemKind::Group, window, cx))
                    .child(id_field(
                        group.id.as_deref().is_none_or(is_valid_block_id),
                        window,
                        cx,
                    ))
                    .child(
                        field_frame(
                            &row.label,
                            LABEL_FIELD_WIDTH,
                            group.label.as_deref().is_none_or(is_valid_block_label),
                            window,
                            cx,
                        )
                        .flex_1()
                        .child(row.label.clone()),
                    );
                second_line = Some(
                    h_flex()
                        .w_full()
                        .gap_1()
                        .pl_4()
                        .child(section_title(i18n::t!("ID and label are optional")))
                        .child(width_fields(window, cx))
                        .child(section_title(i18n::t!("Number of Columns")))
                        .child(
                            field_frame(
                                &row.extra,
                                NUMBER_FIELD_WIDTH,
                                is_valid_columns_text(&field_text(&row.extra, cx)),
                                window,
                                cx,
                            )
                            .child(row.extra.clone()),
                        ),
                );
            }
            BlockItem::Comment(text) => {
                first_line = first_line
                    .child(
                        div()
                            .flex_none()
                            .w(rems(4.))
                            .child(section_title(i18n::t!("Comment"))),
                    )
                    .child(
                        field_frame(
                            &row.extra,
                            LABEL_FIELD_WIDTH,
                            is_valid_block_comment(text),
                            window,
                            cx,
                        )
                        .flex_1()
                        .child(row.extra.clone()),
                    );
            }
        }

        v_flex()
            .w_full()
            .gap_1()
            .pl(rems(DEPTH_INDENT_REMS * depth as f32))
            .child(first_line.child(controls))
            .children(second_line)
            .into_any_element()
    }

    fn render_block_kind_menu(
        &self,
        index: usize,
        current: BlockItemKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for kind in BlockItemKind::ALL {
                let this = this.clone();
                menu = menu.toggleable_entry(
                    kind.label(),
                    kind == current,
                    IconPosition::Start,
                    None,
                    move |window, cx| {
                        this.update(cx, |this, cx| this.set_block_kind(index, kind, window, cx))
                            .log_err();
                    },
                );
            }
            menu
        });
        DropdownMenu::new(
            SharedString::from(format!("mermaid-block-kind-{index}")),
            current.label(),
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
    }

    fn render_block_shape_menu(
        &self,
        index: usize,
        current: Option<BlockShape>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for shape in std::iter::once(None).chain(BlockShape::ALL.map(Some)) {
                let this = this.clone();
                menu = menu.toggleable_entry(
                    shape_label(shape),
                    shape == current,
                    IconPosition::Start,
                    None,
                    move |window, cx| {
                        this.update(cx, |this, cx| {
                            this.set_block_shape(index, shape, window, cx)
                        })
                        .log_err();
                    },
                );
            }
            menu
        });
        DropdownMenu::new(
            SharedString::from(format!("mermaid-block-shape-{index}")),
            shape_label(current),
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
    }

    /// A menu of the block ids, for an edge end or a `style` target.
    fn render_block_id_menu(
        &self,
        element_id: SharedString,
        current: &str,
        ids: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
        pick: impl Fn(&mut Self, String, &mut Context<Self>) + Clone + 'static,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let selected = current.to_string();
        let choices = ids.to_vec();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for id in choices.clone() {
                let this = this.clone();
                let pick = pick.clone();
                let is_selected = id == selected;
                menu = menu.toggleable_entry(
                    id.clone(),
                    is_selected,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        let id = id.clone();
                        let pick = pick.clone();
                        this.update(cx, move |this, cx| pick(this, id, cx))
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

    fn render_block_edges(
        &self,
        form: &BlockForm,
        current: &BlockDocument,
        ids: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let edge_indices = current
            .statements
            .iter()
            .enumerate()
            .filter(|(_, statement)| matches!(statement, BlockStatement::Edge(_)))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let mut section = v_flex()
            .w_full()
            .gap_1()
            .child(section_title(i18n::t!("Edges")));
        for (position, &index) in edge_indices.iter().enumerate() {
            let (Some(BlockStatement::Edge(edge)), Some(row)) =
                (current.statements.get(index), form.statements.get(index))
            else {
                continue;
            };
            let Some(label) = row.first.as_ref() else {
                continue;
            };
            section = section.child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(self.render_block_id_menu(
                        format!("mermaid-block-edge-from-{index}").into(),
                        &edge.from,
                        ids,
                        window,
                        cx,
                        move |this, id, cx| {
                            this.update_block_statement(index, cx, move |statement| {
                                if let BlockStatement::Edge(edge) = statement {
                                    edge.from = id;
                                }
                            })
                        },
                    ))
                    .child(self.render_block_link_menu(index, &edge.link, window, cx))
                    .child(self.render_block_id_menu(
                        format!("mermaid-block-edge-to-{index}").into(),
                        &edge.to,
                        ids,
                        window,
                        cx,
                        move |this, id, cx| {
                            this.update_block_statement(index, cx, move |statement| {
                                if let BlockStatement::Edge(edge) = statement {
                                    edge.to = id;
                                }
                            })
                        },
                    ))
                    .child(
                        field_frame(
                            label,
                            LABEL_FIELD_WIDTH,
                            is_valid_edge_label(&field_text(label, cx)),
                            window,
                            cx,
                        )
                        .flex_1()
                        .child(label.clone()),
                    )
                    .child(row_controls(
                        "mermaid-block-edge",
                        index,
                        position == 0,
                        position + 1 >= edge_indices.len(),
                        cx.listener(move |this, _, _window, cx| {
                            this.move_block_edge(index, false, cx)
                        }),
                        cx.listener(move |this, _, _window, cx| {
                            this.move_block_edge(index, true, cx)
                        }),
                        cx.listener(move |this, _, _window, cx| {
                            this.remove_block_statement(index, cx)
                        }),
                    )),
            );
        }
        section
            .child(add_button(
                "mermaid-block-add-edge",
                i18n::t!("Add Edge"),
                cx.listener(|this, _, window, cx| this.add_block_edge(window, cx)),
            ))
            .into_any_element()
    }

    fn render_block_link_menu(
        &self,
        index: usize,
        current: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let selected = current.to_string();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            // A diagram may spell a link the menu does not offer (`==>`), and
            // it stays selectable so that looking at the menu changes nothing.
            let mut offered = BLOCK_LINKS
                .iter()
                .map(|(spelling, _)| spelling.to_string())
                .collect::<Vec<_>>();
            if !offered.contains(&selected) {
                offered.push(selected.clone());
            }
            for link in offered {
                let this = this.clone();
                let is_selected = link == selected;
                menu = menu.toggleable_entry(
                    link_label(&link),
                    is_selected,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        let link = link.clone();
                        this.update(cx, |this, cx| {
                            this.update_block_statement(index, cx, move |statement| {
                                if let BlockStatement::Edge(edge) = statement {
                                    edge.link = link;
                                }
                            })
                        })
                        .log_err();
                    },
                );
            }
            menu
        });
        DropdownMenu::new(
            SharedString::from(format!("mermaid-block-link-{index}")),
            link_label(current),
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
    }

    fn render_block_class_targets_menu(
        &self,
        index: usize,
        targets: &[String],
        ids: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let assigned = targets.to_vec();
        let choices = ids.to_vec();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for id in choices.clone() {
                let this = this.clone();
                let is_selected = assigned.contains(&id);
                menu = menu.toggleable_entry(
                    id.clone(),
                    is_selected,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        let id = id.clone();
                        this.update(cx, |this, cx| {
                            this.update_block_statement(index, cx, move |statement| {
                                if let BlockStatement::Class { targets, .. } = statement {
                                    match targets.iter().position(|target| *target == id) {
                                        Some(position) => {
                                            targets.remove(position);
                                        }
                                        None => targets.push(id),
                                    }
                                }
                            })
                        })
                        .log_err();
                    },
                );
            }
            menu
        });
        let label = if targets.is_empty() {
            i18n::t!("Blocks to Apply To").to_string()
        } else {
            targets.join(",")
        };
        DropdownMenu::new(
            SharedString::from(format!("mermaid-block-class-targets-{index}")),
            label,
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
    }

    fn render_block_styles(
        &self,
        form: &BlockForm,
        current: &BlockDocument,
        ids: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut section = v_flex().w_full().gap_1().child(section_title(i18n::t!(
            "Styles (classDef defines a class and class applies it; style styles one block directly)"
        )));
        for (index, (statement, row)) in current.statements.iter().zip(&form.statements).enumerate()
        {
            let keyword = match statement {
                BlockStatement::ClassDef { .. } => "classDef",
                BlockStatement::Class { .. } => "class",
                BlockStatement::Style { .. } => "style",
                BlockStatement::Edge(_) | BlockStatement::Comment(_) => continue,
            };
            let mut line = h_flex()
                .w_full()
                .gap_1()
                .child(div().flex_none().w(rems(4.)).child(section_title(keyword)));
            match statement {
                BlockStatement::ClassDef { name, css } => {
                    if let (Some(first), Some(second)) = (&row.first, &row.second) {
                        line = line
                            .child(
                                field_frame(
                                    first,
                                    ID_FIELD_WIDTH,
                                    is_valid_block_class_name(name),
                                    window,
                                    cx,
                                )
                                .child(first.clone()),
                            )
                            .child(
                                field_frame(
                                    second,
                                    LABEL_FIELD_WIDTH,
                                    is_valid_block_css(css),
                                    window,
                                    cx,
                                )
                                .flex_1()
                                .child(second.clone()),
                            );
                    }
                }
                BlockStatement::Class {
                    targets,
                    class_name,
                } => {
                    line = line.child(
                        self.render_block_class_targets_menu(index, targets, ids, window, cx),
                    );
                    if let Some(first) = &row.first {
                        line = line.child(
                            field_frame(
                                first,
                                ID_FIELD_WIDTH,
                                is_valid_block_class_name(class_name),
                                window,
                                cx,
                            )
                            .flex_1()
                            .child(first.clone()),
                        );
                    }
                }
                BlockStatement::Style { target, css } => {
                    line = line.child(self.render_block_id_menu(
                        format!("mermaid-block-style-target-{index}").into(),
                        target,
                        ids,
                        window,
                        cx,
                        move |this, id, cx| {
                            this.update_block_statement(index, cx, move |statement| {
                                if let BlockStatement::Style { target, .. } = statement {
                                    *target = id;
                                }
                            })
                        },
                    ));
                    if let Some(first) = &row.first {
                        line = line.child(
                            field_frame(
                                first,
                                LABEL_FIELD_WIDTH,
                                is_valid_block_css(css),
                                window,
                                cx,
                            )
                            .flex_1()
                            .child(first.clone()),
                        );
                    }
                }
                BlockStatement::Edge(_) | BlockStatement::Comment(_) => {}
            }
            section = section.child(
                line.child(
                    IconButton::new(
                        SharedString::from(format!("mermaid-block-style-remove-{index}")),
                        IconName::Trash,
                    )
                    .shape(IconButtonShape::Square)
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text(i18n::t!("Delete")))
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.remove_block_statement(index, cx)
                    })),
                ),
            );
        }
        section
            .child(
                h_flex()
                    .w_full()
                    .flex_wrap()
                    .gap_1()
                    .child(add_button(
                        "mermaid-block-add-style",
                        i18n::t!("Add style"),
                        cx.listener(|this, _, window, cx| this.add_block_style(window, cx)),
                    ))
                    .child(add_button(
                        "mermaid-block-add-class-def",
                        i18n::t!("Add classDef"),
                        cx.listener(|this, _, window, cx| this.add_block_class_def(window, cx)),
                    ))
                    .child(add_button(
                        "mermaid-block-add-class",
                        i18n::t!("Add class"),
                        cx.listener(|this, _, window, cx| this.add_block_class(window, cx)),
                    )),
            )
            .into_any_element()
    }
}

#[cfg(test)]
#[path = "block_tests.rs"]
mod block_tests;
