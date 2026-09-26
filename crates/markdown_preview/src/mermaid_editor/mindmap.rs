//! The mindmap half of the Mermaid editor's form.
//!
//! A mindmap is a tree spelled with indentation, so the form is its outline:
//! one row per node in the order the fence writes them, pushed right by its
//! depth. The rows are what move — a node's subtree travels with it when it is
//! moved, indented or outdented — and the operations themselves live in the
//! model ([`mermaid_render::mindmap_model`]), which the tests there pin down
//! without a window.

use editor::Editor;
use gpui::{AnyElement, App, Context, Entity, Focusable as _, Window, rems};
use mermaid_render::flowchart_model::PreservedLine;
use mermaid_render::mindmap_model::{
    MindmapDocument, MindmapNode, MindmapOutlineEntry, MindmapShape, OutlineItem,
    is_valid_mindmap_classes, is_valid_mindmap_icon, is_valid_mindmap_label, outline_add_child,
    outline_add_sibling, outline_indent, outline_move, outline_next_sibling, outline_outdent,
    outline_previous_sibling, outline_remove,
};
use ui::{
    ContextMenu, DropdownMenu, DropdownStyle, IconButtonShape, IconPosition, Tooltip, prelude::*,
};
use util::ResultExt as _;

use super::{
    Form, ID_FIELD_WIDTH, LABEL_FIELD_WIDTH, MermaidEditor, add_button, field_frame, section_title,
};

/// The label a node is added with, so that a new row is visible in the drawing
/// before anything has been typed into it.
const NEW_NODE_LABEL: &str = "新しいノード";
const NEW_ROOT_LABEL: &str = "中心テーマ";

/// How far each level of the tree pushes its rows to the right.
const DEPTH_INDENT_REMS: f32 = 1.;

pub(super) struct MindmapForm {
    pub(super) preserved_lines: Vec<PreservedLine>,
    pub(super) rows: Vec<MindmapRow>,
}

pub(super) struct MindmapRow {
    /// The node as it was last synced from the fields, with its depth. The
    /// shape and the quoting live only here; the texts are read from the
    /// fields.
    pub(super) entry: MindmapOutlineEntry,
    pub(super) label: Entity<Editor>,
    pub(super) icon: Entity<Editor>,
    pub(super) classes: Entity<Editor>,
}

impl OutlineItem for MindmapRow {
    fn depth(&self) -> usize {
        self.entry.depth
    }

    fn set_depth(&mut self, depth: usize) {
        self.entry.depth = depth;
    }
}

impl MindmapRow {
    /// The node this row describes, with the fields' current text.
    fn current_node(&self, cx: &App) -> MindmapNode {
        let mut node = self.entry.node.clone();
        let label = self.label.read(cx).text(cx);
        // A node without brackets is read from a trimmed line, so whitespace
        // around its label can never be written; with brackets it can, and a
        // label that has it keeps it.
        node.label = if node.shape == MindmapShape::Default {
            label.trim().to_string()
        } else {
            label
        };
        node.icon = field_value(&self.icon, cx);
        node.classes = field_value(&self.classes, cx);
        node.fit_label_quoting();
        node
    }
}

impl MindmapForm {
    pub(super) fn fields(&self) -> Vec<Entity<Editor>> {
        self.rows
            .iter()
            .flat_map(|row| [row.label.clone(), row.icon.clone(), row.classes.clone()])
            .collect()
    }

    /// The map the form currently describes.
    pub(super) fn current_document(&self, cx: &App) -> MindmapDocument {
        // The row operations only ever produce a tree, but the depths are
        // clamped anyway: an outline that failed to build would otherwise have
        // to be written back as an empty map.
        let mut previous: Option<usize> = None;
        let entries = self
            .rows
            .iter()
            .map(|row| {
                let depth = match previous {
                    None => 0,
                    Some(previous) => row.entry.depth.clamp(1, previous + 1),
                };
                previous = Some(depth);
                MindmapOutlineEntry {
                    depth,
                    node: row.current_node(cx),
                }
            })
            .collect::<Vec<_>>();
        MindmapDocument::from_outline(entries, self.preserved_lines.clone())
            .log_err()
            .unwrap_or_else(MindmapDocument::empty)
    }
}

/// The first thing wrong with what the user typed, in the words shown above the
/// form. Every node is checked by `MindmapDocument::structure_error`, which the
/// caller consults right after this; the red frames come from the
/// `is_valid_mindmap_*` predicates directly.
pub(super) fn problem(_document: &MindmapDocument) -> Option<SharedString> {
    None
}

/// A field's text, or `None` when it is empty, which is how a node goes without
/// an icon or a class line.
fn field_value(field: &Entity<Editor>, cx: &App) -> Option<String> {
    let text = field.read(cx).text(cx).trim().to_string();
    (!text.is_empty()).then_some(text)
}

impl MermaidEditor {
    pub(super) fn new_mindmap_form(
        &mut self,
        document: MindmapDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> MindmapForm {
        let outline = document.outline();
        let mut rows = Vec::with_capacity(outline.len());
        for entry in outline {
            rows.push(self.new_mindmap_row(entry, window, cx));
        }
        MindmapForm {
            preserved_lines: document.preserved_lines,
            rows,
        }
    }

    fn new_mindmap_row(
        &mut self,
        entry: MindmapOutlineEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> MindmapRow {
        let label = self.new_field(&entry.node.label, window, cx);
        let icon = self.new_field(entry.node.icon.as_deref().unwrap_or_default(), window, cx);
        let classes = self.new_field(
            entry.node.classes.as_deref().unwrap_or_default(),
            window,
            cx,
        );
        MindmapRow {
            entry,
            label,
            icon,
            classes,
        }
    }

    pub(super) fn mindmap(&self) -> Option<&MindmapForm> {
        match &self.form {
            Form::Mindmap(form) => Some(form),
            _ => None,
        }
    }

    pub(super) fn mindmap_mut(&mut self) -> Option<&mut MindmapForm> {
        match &mut self.form {
            Form::Mindmap(form) => Some(form),
            _ => None,
        }
    }

    fn new_mindmap_entry(label: &str) -> MindmapOutlineEntry {
        MindmapOutlineEntry {
            depth: 0,
            node: MindmapNode::new(label),
        }
    }

    fn mindmap_changed(&mut self, cx: &mut Context<Self>) {
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn add_mindmap_root(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.mindmap().is_none_or(|form| !form.rows.is_empty()) {
            return;
        }
        let row = self.new_mindmap_row(Self::new_mindmap_entry(NEW_ROOT_LABEL), window, cx);
        if let Some(form) = self.mindmap_mut() {
            form.rows.push(row);
        }
        self.mindmap_changed(cx);
    }

    pub(super) fn add_mindmap_child(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.mindmap().is_none_or(|form| index >= form.rows.len()) {
            return;
        }
        let row = self.new_mindmap_row(Self::new_mindmap_entry(NEW_NODE_LABEL), window, cx);
        let added = self
            .mindmap_mut()
            .and_then(|form| outline_add_child(&mut form.rows, index, row));
        self.focus_mindmap_label(added, window, cx);
        self.mindmap_changed(cx);
    }

    pub(super) fn add_mindmap_sibling(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .mindmap()
            .is_none_or(|form| form.rows.get(index).is_none_or(|row| row.entry.depth == 0))
        {
            return;
        }
        let row = self.new_mindmap_row(Self::new_mindmap_entry(NEW_NODE_LABEL), window, cx);
        let added = self
            .mindmap_mut()
            .and_then(|form| outline_add_sibling(&mut form.rows, index, row));
        self.focus_mindmap_label(added, window, cx);
        self.mindmap_changed(cx);
    }

    /// Puts the cursor into a row that was just added, so that its label can be
    /// typed straight away.
    fn focus_mindmap_label(&self, index: Option<usize>, window: &mut Window, cx: &mut App) {
        if let Some(row) = index.and_then(|index| self.mindmap()?.rows.get(index)) {
            row.label.focus_handle(cx).focus(window, cx);
        }
    }

    pub(super) fn remove_mindmap_node(&mut self, index: usize, cx: &mut Context<Self>) {
        let removed = self
            .mindmap_mut()
            .and_then(|form| outline_remove(&mut form.rows, index));
        if removed.is_some() {
            self.mindmap_changed(cx);
        }
    }

    pub(super) fn move_mindmap_node(
        &mut self,
        index: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        let moved = self
            .mindmap_mut()
            .and_then(|form| outline_move(&mut form.rows, index, forward));
        if moved.is_some() {
            self.mindmap_changed(cx);
        }
    }

    pub(super) fn indent_mindmap_node(&mut self, index: usize, cx: &mut Context<Self>) {
        let indented = self
            .mindmap_mut()
            .is_some_and(|form| outline_indent(&mut form.rows, index));
        if indented {
            self.mindmap_changed(cx);
        }
    }

    pub(super) fn outdent_mindmap_node(&mut self, index: usize, cx: &mut Context<Self>) {
        let outdented = self
            .mindmap_mut()
            .and_then(|form| outline_outdent(&mut form.rows, index));
        if outdented.is_some() {
            self.mindmap_changed(cx);
        }
    }

    pub(super) fn set_mindmap_shape(
        &mut self,
        index: usize,
        shape: MindmapShape,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.mindmap() else {
            return;
        };
        // Taken from the field, so that the quoting the new brackets need is
        // decided for the label as it is now rather than as it was opened.
        let Some(label) = form.rows.get(index).map(|row| row.label.read(cx).text(cx)) else {
            return;
        };
        let Some(row) = self.mindmap_mut().and_then(|form| form.rows.get_mut(index)) else {
            return;
        };
        row.entry.node.label = label;
        row.entry.node.set_shape(shape);
        self.mindmap_changed(cx);
    }

    pub(super) fn render_mindmap(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(form) = self.mindmap() else {
            return Vec::new();
        };
        let mut list = v_flex()
            .w_full()
            .gap_2()
            .child(section_title("ノード (右にずれているほど深い階層)"));

        for index in 0..form.rows.len() {
            if let Some(row) = form.rows.get(index) {
                list = list.child(self.render_mindmap_row(form, index, row, window, cx));
            }
        }

        if form.rows.is_empty() {
            list = list.child(add_button(
                "mermaid-mindmap-add-root",
                "中心ノードを追加",
                cx.listener(|this, _, window, cx| this.add_mindmap_root(window, cx)),
            ));
        }
        vec![list.into_any_element()]
    }

    fn render_mindmap_row(
        &self,
        form: &MindmapForm,
        index: usize,
        row: &MindmapRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let node = row.current_node(cx);
        let depth = row.entry.depth;
        let is_root = depth == 0;
        let can_indent = outline_previous_sibling(&form.rows, index).is_some();
        let can_outdent = depth >= 2;
        let can_move_up = outline_previous_sibling(&form.rows, index).is_some();
        let can_move_down = outline_next_sibling(&form.rows, index).is_some();

        let button = |name: &str, icon: IconName, tooltip: &'static str, disabled: bool| {
            IconButton::new(
                SharedString::from(format!("mermaid-mindmap-{name}-{index}")),
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
                    "一段浅く (親の次へ)",
                    !can_outdent,
                )
                .on_click(
                    cx.listener(move |this, _, _window, cx| this.outdent_mindmap_node(index, cx)),
                ),
            )
            .child(
                button(
                    "indent",
                    IconName::ArrowRight,
                    "一段深く (上の兄弟の子へ)",
                    !can_indent,
                )
                .on_click(
                    cx.listener(move |this, _, _window, cx| this.indent_mindmap_node(index, cx)),
                ),
            )
            .child(
                button("up", IconName::ArrowUp, "上へ移動", !can_move_up).on_click(cx.listener(
                    move |this, _, _window, cx| this.move_mindmap_node(index, false, cx),
                )),
            )
            .child(
                button("down", IconName::ArrowDown, "下へ移動", !can_move_down).on_click(
                    cx.listener(move |this, _, _window, cx| {
                        this.move_mindmap_node(index, true, cx)
                    }),
                ),
            )
            .child(
                button("add-child", IconName::ArrowDownRight, "子を追加", false).on_click(
                    cx.listener(move |this, _, window, cx| {
                        this.add_mindmap_child(index, window, cx)
                    }),
                ),
            )
            .child(
                button("add-sibling", IconName::Plus, "下に兄弟を追加", is_root).on_click(
                    cx.listener(move |this, _, window, cx| {
                        this.add_mindmap_sibling(index, window, cx)
                    }),
                ),
            )
            .child(
                button(
                    "remove",
                    IconName::Trash,
                    "このノードとその下のノードをすべて削除",
                    is_root,
                )
                .on_click(
                    cx.listener(move |this, _, _window, cx| this.remove_mindmap_node(index, cx)),
                ),
            );

        v_flex()
            .w_full()
            .gap_1()
            .pl(rems(DEPTH_INDENT_REMS * depth as f32))
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        field_frame(
                            &row.label,
                            LABEL_FIELD_WIDTH,
                            is_valid_mindmap_label(&node),
                            window,
                            cx,
                        )
                        .flex_1()
                        .child(row.label.clone()),
                    )
                    .child(self.render_mindmap_shape_menu(index, node.shape, window, cx))
                    .child(controls),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .pl_4()
                    .child(section_title("アイコン"))
                    .child(
                        field_frame(
                            &row.icon,
                            ID_FIELD_WIDTH,
                            node.icon.as_deref().is_none_or(is_valid_mindmap_icon),
                            window,
                            cx,
                        )
                        .child(row.icon.clone()),
                    )
                    .child(section_title("クラス"))
                    .child(
                        field_frame(
                            &row.classes,
                            ID_FIELD_WIDTH,
                            node.classes.as_deref().is_none_or(is_valid_mindmap_classes),
                            window,
                            cx,
                        )
                        .child(row.classes.clone()),
                    ),
            )
            .into_any_element()
    }

    fn render_mindmap_shape_menu(
        &self,
        index: usize,
        current: MindmapShape,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for shape in MindmapShape::ALL {
                let this = this.clone();
                menu = menu.toggleable_entry(
                    shape.label(),
                    shape == current,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        this.update(cx, |this, cx| {
                            this.set_mindmap_shape(index, shape, cx);
                        })
                        .log_err();
                    },
                );
            }
            menu
        });

        DropdownMenu::new(
            SharedString::from(format!("mermaid-mindmap-shape-{index}")),
            current.label(),
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
    }
}

#[cfg(test)]
#[path = "mindmap_tests.rs"]
mod mindmap_tests;
