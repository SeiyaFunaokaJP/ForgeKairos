//! The tree view half of the Mermaid editor's form.
//!
//! A tree view is a title and an outline of names, so the form is a title field
//! and one row per name in the order the fence writes them, pushed right by
//! its depth. It works like the mindmap's outline, except that the top level
//! holds any number of names: every row can be moved, removed and given a
//! sibling. The outline operations live in [`mermaid_render::mindmap_model`].
//!
//! The row buttons are shared with the Ishikawa diagram's form, whose causes
//! are the same kind of outline.

use std::rc::Rc;

use editor::Editor;
use gpui::{AnyElement, App, Context, Entity, Focusable as _, Window, rems};
use mermaid_render::flowchart_model::PreservedLine;
use mermaid_render::mindmap_model::{
    OutlineItem, outline_add_child, outline_add_sibling, outline_indent, outline_move,
    outline_next_sibling, outline_outdent, outline_previous_sibling, outline_remove,
};
use mermaid_render::tree_view_model::{
    TreeViewDocument, TreeViewEntry, is_valid_tree_view_name, is_valid_tree_view_title,
};
use ui::{IconButtonShape, Tooltip, prelude::*};

use super::{Form, LABEL_FIELD_WIDTH, MermaidEditor, add_button, field_frame, section_title};

/// The name a node is added with, so that a new row is visible in the drawing
/// before anything has been typed into it.
const NEW_NAME: &str = "新しいノード";

/// How far each level of the tree pushes its rows to the right.
pub(super) const DEPTH_INDENT_REMS: f32 = 1.;

pub(super) struct TreeViewForm {
    pub(super) preserved_lines: Vec<PreservedLine>,
    pub(super) title: Entity<Editor>,
    pub(super) rows: Vec<TreeViewRow>,
}

pub(super) struct TreeViewRow {
    /// The name as it was last synced from the field, with its depth. The
    /// quote lives only here; the text is read from the field.
    pub(super) entry: TreeViewEntry,
    pub(super) name: Entity<Editor>,
}

impl OutlineItem for TreeViewRow {
    fn depth(&self) -> usize {
        self.entry.depth
    }

    fn set_depth(&mut self, depth: usize) {
        self.entry.depth = depth;
    }
}

impl TreeViewRow {
    /// The name this row describes, with the field's current text. A quoted
    /// name keeps the whitespace around it, so the text is taken as it is.
    fn current_entry(&self, cx: &App) -> TreeViewEntry {
        let mut entry = self.entry.clone();
        entry.name = self.name.read(cx).text(cx);
        entry.fit_quote();
        entry
    }
}

impl TreeViewForm {
    pub(super) fn fields(&self) -> Vec<Entity<Editor>> {
        std::iter::once(self.title.clone())
            .chain(self.rows.iter().map(|row| row.name.clone()))
            .collect()
    }

    /// The tree view the form currently describes.
    pub(super) fn current_document(&self, cx: &App) -> TreeViewDocument {
        let title = self.title.read(cx).text(cx).trim().to_string();
        let mut entries = self
            .rows
            .iter()
            .map(|row| row.current_entry(cx))
            .collect::<Vec<_>>();
        clamp_outline(&mut entries);
        TreeViewDocument {
            title: (!title.is_empty()).then_some(title),
            entries,
            preserved_lines: self.preserved_lines.clone(),
        }
    }
}

/// Clamps the depths to an outline: the first item at the top level and none
/// more than one level below the one before it. The row operations only ever
/// produce one, but a row that slipped would otherwise be written back
/// somewhere else than where it is drawn.
pub(super) fn clamp_outline<Item: OutlineItem>(items: &mut [Item]) {
    let mut previous = 0;
    for item in items {
        let depth = item.depth().clamp(1, previous + 1);
        item.set_depth(depth);
        previous = depth;
    }
}

/// The first thing wrong with what the user typed, in the words shown above the
/// form. Every check lives in `TreeViewDocument::structure_error`, which the
/// caller consults right after this; the red frames come from the
/// `is_valid_tree_view_*` predicates directly.
pub(super) fn problem(_document: &TreeViewDocument) -> Option<SharedString> {
    None
}

/// What a row button does to the outline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OutlineEdit {
    Outdent,
    Indent,
    MoveUp,
    MoveDown,
    AddChild,
    AddSibling,
    Remove,
}

impl OutlineEdit {
    pub(super) fn adds_a_row(self) -> bool {
        matches!(self, Self::AddChild | Self::AddSibling)
    }
}

/// The row buttons of an outline in which every row sits at depth 1 or deeper,
/// so that each of them can be moved, removed and given a sibling.
pub(super) fn outline_row_controls<Row: OutlineItem>(
    prefix: &str,
    rows: &[Row],
    index: usize,
    add_child_tooltip: &'static str,
    on_edit: impl Fn(&OutlineEdit, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let depth = rows.get(index).map_or(1, OutlineItem::depth);
    let has_previous_sibling = outline_previous_sibling(rows, index).is_some();
    let has_next_sibling = outline_next_sibling(rows, index).is_some();
    let on_edit = Rc::new(on_edit);
    let button =
        |name: &str, icon: IconName, tooltip: &'static str, disabled: bool, edit: OutlineEdit| {
            let on_edit = on_edit.clone();
            IconButton::new(SharedString::from(format!("{prefix}-{name}-{index}")), icon)
                .shape(IconButtonShape::Square)
                .icon_size(IconSize::XSmall)
                .disabled(disabled)
                .tooltip(Tooltip::text(tooltip))
                .on_click(move |_, window, cx| on_edit(&edit, window, cx))
        };
    h_flex()
        .flex_none()
        .gap_px()
        .child(button(
            "outdent",
            IconName::ArrowLeft,
            "一段浅く (親の次へ)",
            depth < 2,
            OutlineEdit::Outdent,
        ))
        .child(button(
            "indent",
            IconName::ArrowRight,
            "一段深く (上の兄弟の子へ)",
            !has_previous_sibling,
            OutlineEdit::Indent,
        ))
        .child(button(
            "up",
            IconName::ArrowUp,
            "上へ移動",
            !has_previous_sibling,
            OutlineEdit::MoveUp,
        ))
        .child(button(
            "down",
            IconName::ArrowDown,
            "下へ移動",
            !has_next_sibling,
            OutlineEdit::MoveDown,
        ))
        .child(button(
            "add-child",
            IconName::ArrowDownRight,
            add_child_tooltip,
            false,
            OutlineEdit::AddChild,
        ))
        .child(button(
            "add-sibling",
            IconName::Plus,
            "下に同じ段を追加",
            false,
            OutlineEdit::AddSibling,
        ))
        .child(button(
            "remove",
            IconName::Trash,
            "この行とその下の行をすべて削除",
            false,
            OutlineEdit::Remove,
        ))
}

/// Applies `edit` to the row at `index`. `new_row` is the row an adding edit
/// inserts. Returns whether the outline changed and where the added row went.
pub(super) fn apply_outline_edit<Row: OutlineItem>(
    rows: &mut Vec<Row>,
    index: usize,
    edit: OutlineEdit,
    new_row: Option<Row>,
) -> (bool, Option<usize>) {
    if index >= rows.len() {
        return (false, None);
    }
    match edit {
        OutlineEdit::Outdent => (outline_outdent(rows, index).is_some(), None),
        OutlineEdit::Indent => (outline_indent(rows, index), None),
        OutlineEdit::MoveUp => (outline_move(rows, index, false).is_some(), None),
        OutlineEdit::MoveDown => (outline_move(rows, index, true).is_some(), None),
        OutlineEdit::Remove => (outline_remove(rows, index).is_some(), None),
        OutlineEdit::AddChild => {
            let added = new_row.and_then(|row| outline_add_child(rows, index, row));
            (added.is_some(), added)
        }
        OutlineEdit::AddSibling => {
            let added = new_row.and_then(|row| outline_add_sibling(rows, index, row));
            (added.is_some(), added)
        }
    }
}

impl MermaidEditor {
    pub(super) fn new_tree_view_form(
        &mut self,
        document: TreeViewDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> TreeViewForm {
        let title = self.new_field(document.title.as_deref().unwrap_or_default(), window, cx);
        let rows = document
            .entries
            .into_iter()
            .map(|entry| self.new_tree_view_row(entry, window, cx))
            .collect();
        TreeViewForm {
            preserved_lines: document.preserved_lines,
            title,
            rows,
        }
    }

    fn new_tree_view_row(
        &mut self,
        entry: TreeViewEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> TreeViewRow {
        let name = self.new_field(&entry.name, window, cx);
        TreeViewRow { entry, name }
    }

    pub(super) fn tree_view(&self) -> Option<&TreeViewForm> {
        match &self.form {
            Form::TreeView(form) => Some(form),
            _ => None,
        }
    }

    pub(super) fn tree_view_mut(&mut self) -> Option<&mut TreeViewForm> {
        match &mut self.form {
            Form::TreeView(form) => Some(form),
            _ => None,
        }
    }

    fn tree_view_changed(&mut self, cx: &mut Context<Self>) {
        self.schedule_preview(cx);
        cx.notify();
    }

    /// Adds a name at the end of the top level.
    pub(super) fn add_tree_view_top_level(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tree_view().is_none() {
            return;
        }
        let row = self.new_tree_view_row(TreeViewEntry::new(NEW_NAME), window, cx);
        let focus = row.name.focus_handle(cx);
        if let Some(form) = self.tree_view_mut() {
            form.rows.push(row);
        }
        focus.focus(window, cx);
        self.tree_view_changed(cx);
    }

    pub(super) fn edit_tree_view(
        &mut self,
        index: usize,
        edit: OutlineEdit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tree_view().is_none_or(|form| index >= form.rows.len()) {
            return;
        }
        let new_row = edit
            .adds_a_row()
            .then(|| self.new_tree_view_row(TreeViewEntry::new(NEW_NAME), window, cx));
        let Some(form) = self.tree_view_mut() else {
            return;
        };
        let (changed, added) = apply_outline_edit(&mut form.rows, index, edit, new_row);
        if let Some(row) = added.and_then(|added| form.rows.get(added)) {
            row.name.focus_handle(cx).focus(window, cx);
        }
        if changed {
            self.tree_view_changed(cx);
        }
    }

    pub(super) fn render_tree_view(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(form) = self.tree_view() else {
            return Vec::new();
        };
        let title = form.title.read(cx).text(cx);
        let title = title.trim();
        let title_row = h_flex()
            .w_full()
            .gap_2()
            .child(section_title("タイトル"))
            .child(
                field_frame(
                    &form.title,
                    LABEL_FIELD_WIDTH,
                    title.is_empty() || is_valid_tree_view_title(title),
                    window,
                    cx,
                )
                .flex_1()
                .child(form.title.clone()),
            );

        let mut list = v_flex()
            .w_full()
            .gap_2()
            .child(section_title("ノード (右にずれているほど深い階層)"));
        for (index, row) in form.rows.iter().enumerate() {
            let entry = row.current_entry(cx);
            let controls = outline_row_controls(
                "mermaid-tree-view",
                &form.rows,
                index,
                "子を追加",
                cx.listener(move |this, edit: &OutlineEdit, window, cx| {
                    this.edit_tree_view(index, *edit, window, cx)
                }),
            );
            list = list.child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .pl(rems(
                        DEPTH_INDENT_REMS * row.entry.depth.saturating_sub(1) as f32,
                    ))
                    .child(
                        field_frame(
                            &row.name,
                            LABEL_FIELD_WIDTH,
                            is_valid_tree_view_name(&entry),
                            window,
                            cx,
                        )
                        .flex_1()
                        .child(row.name.clone()),
                    )
                    .child(controls),
            );
        }
        list = list.child(add_button(
            "mermaid-tree-view-add-top-level",
            "最上位にノードを追加",
            cx.listener(|this, _, window, cx| this.add_tree_view_top_level(window, cx)),
        ));

        vec![title_row.into_any_element(), list.into_any_element()]
    }

    pub(super) fn render_tree_view_toolbar(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> AnyElement {
        div().into_any_element()
    }
}

#[cfg(test)]
#[path = "tree_view_tests.rs"]
mod tree_view_tests;
