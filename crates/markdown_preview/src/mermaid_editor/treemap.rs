//! The treemap half of the Mermaid editor's form.
//!
//! A treemap is a forest spelled with indentation, so the form is its outline
//! like a mindmap's: one row per item in the order the fence writes them,
//! pushed right by its depth, with the subtree travelling along when a row is
//! moved. An item with a value is a leaf, which Mermaid never puts anything
//! under, so the buttons that would do that are off on a leaf's row. Below the
//! outline are the `classDef` lines the items' classes point at; renaming one
//! renames the items' references, and removing one clears them.

use editor::Editor;
use gpui::{AnyElement, App, Context, Entity, Focusable as _, Window, rems};
use mermaid_render::mindmap_model::{
    OutlineItem, outline_add_child, outline_indent, outline_move, outline_next_sibling,
    outline_previous_sibling,
};
use mermaid_render::treemap_model::{
    TreemapClassDef, TreemapDocument, TreemapEntry, TreemapItem, is_valid_treemap_class_name,
    is_valid_treemap_name, is_valid_treemap_style, is_valid_treemap_title, is_valid_treemap_value,
    treemap_add_sibling, treemap_outdent, treemap_remove,
};
use ui::{
    ContextMenu, DropdownMenu, DropdownStyle, IconButtonShape, IconPosition, Tooltip, prelude::*,
};
use util::ResultExt as _;

use super::{
    Form, ID_FIELD_WIDTH, LABEL_FIELD_WIDTH, MermaidEditor, NUMBER_FIELD_WIDTH, add_button,
    field_frame, neighbour, row_controls, section_title,
};

/// The texts new rows are added with, so that they are visible in the drawing
/// before anything has been typed into them.
const NEW_ITEM_NAME: &str = i18n::msg!("New item");
const NEW_ITEM_VALUE: &str = "10";
const NEW_CLASS_NAME: &str = "class";
const NEW_CLASS_STYLE: &str = "fill:#f96";

/// How far each level of the forest pushes its rows to the right.
const DEPTH_INDENT_REMS: f32 = 1.;

pub(super) struct TreemapForm {
    /// The document as it was last synced from the fields. Its entries are
    /// not read — the rows carry them — but its class names are what a renamed
    /// class is recognized by.
    pub(super) document: TreemapDocument,
    pub(super) title: Entity<Editor>,
    pub(super) rows: Vec<TreemapRow>,
    pub(super) class_defs: Vec<ClassDefRow>,
}

pub(super) struct TreemapRow {
    /// The item with its depth; the name and the value are read from the
    /// fields, the class is picked from a menu and lives here.
    pub(super) entry: TreemapEntry,
    pub(super) name: Entity<Editor>,
    pub(super) value: Entity<Editor>,
}

impl OutlineItem for TreemapRow {
    fn depth(&self) -> usize {
        self.entry.depth
    }

    fn set_depth(&mut self, depth: usize) {
        self.entry.depth = depth;
    }
}

pub(super) struct ClassDefRow {
    pub(super) name: Entity<Editor>,
    pub(super) style: Entity<Editor>,
}

impl TreemapForm {
    pub(super) fn fields(&self) -> Vec<Entity<Editor>> {
        std::iter::once(self.title.clone())
            .chain(
                self.rows
                    .iter()
                    .flat_map(|row| [row.name.clone(), row.value.clone()]),
            )
            .chain(
                self.class_defs
                    .iter()
                    .flat_map(|row| [row.name.clone(), row.style.clone()]),
            )
            .collect()
    }

    /// The treemap the form currently describes.
    pub(super) fn current_document(&self, cx: &App) -> TreemapDocument {
        self.document_with_renames(cx, |_, _| true)
    }

    /// The treemap the fields describe, with the items carried along only by
    /// the class renames `follow` accepts. A rename it refuses leaves the
    /// stored class name as it was, so the rename is still pending once it
    /// settles.
    fn document_with_renames(
        &self,
        cx: &App,
        follow: impl Fn(&str, &str) -> bool,
    ) -> TreemapDocument {
        let mut class_defs = self
            .document
            .class_defs
            .iter()
            .zip(&self.class_defs)
            .map(|(class_def, row)| TreemapClassDef {
                name: field_text(&row.name, cx),
                style: field_text(&row.style, cx),
                terminated: class_def.terminated,
            })
            .collect::<Vec<_>>();
        for (before, after) in self.document.class_defs.iter().zip(&mut class_defs) {
            if before.name != after.name && !follow(&before.name, &after.name) {
                after.name = before.name.clone();
            }
        }
        // A class that was renamed takes the items that used it along, unless
        // another definition still carries the old name.
        let renames = self
            .document
            .class_defs
            .iter()
            .zip(&class_defs)
            .filter(|(before, after)| before.name != after.name)
            .filter(|(before, _)| class_defs.iter().all(|other| other.name != before.name))
            .map(|(before, after)| (before.name.clone(), after.name.clone()))
            .collect::<Vec<_>>();

        let entries = self
            .rows
            .iter()
            .map(|row| {
                let mut item = row.entry.item.clone();
                item.name = field_text(&row.name, cx);
                let value = field_text(&row.value, cx);
                item.value = (!value.is_empty()).then_some(value);
                if let Some(class) = &item.class
                    && let Some((_, renamed)) = renames.iter().find(|(before, _)| before == class)
                {
                    item.class = Some(renamed.clone());
                }
                item.fit_quote();
                TreemapEntry {
                    depth: row.entry.depth,
                    item,
                }
            })
            .collect();

        TreemapDocument {
            header: self.document.header,
            title: Some(field_text(&self.title, cx)).filter(|title| !title.is_empty()),
            entries,
            class_defs,
            preserved_lines: self.document.preserved_lines.clone(),
        }
    }
}

/// The first thing wrong with what the user typed, in the words shown above the
/// form. Every check lives in `TreemapDocument::structure_error`, which the
/// caller consults right after this; the red frames come from the
/// `is_valid_treemap_*` predicates directly.
pub(super) fn problem(_document: &TreemapDocument) -> Option<SharedString> {
    None
}

fn field_text(field: &Entity<Editor>, cx: &App) -> String {
    field.read(cx).text(cx).trim().to_string()
}

fn field_is_empty(field: &Entity<Editor>, cx: &App) -> bool {
    field.read(cx).text(cx).trim().is_empty()
}

impl MermaidEditor {
    pub(super) fn new_treemap_form(
        &mut self,
        document: TreemapDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> TreemapForm {
        let title = self.new_field(document.title.as_deref().unwrap_or_default(), window, cx);
        let mut rows = Vec::with_capacity(document.entries.len());
        for entry in &document.entries {
            rows.push(self.new_treemap_row(entry.clone(), window, cx));
        }
        let mut class_defs = Vec::with_capacity(document.class_defs.len());
        for class_def in &document.class_defs {
            class_defs.push(ClassDefRow {
                name: self.new_field(&class_def.name, window, cx),
                style: self.new_field(&class_def.style, window, cx),
            });
        }
        TreemapForm {
            document,
            title,
            rows,
            class_defs,
        }
    }

    fn new_treemap_row(
        &mut self,
        entry: TreemapEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> TreemapRow {
        let name = self.new_field(&entry.item.name, window, cx);
        let value = self.new_field(entry.item.value.as_deref().unwrap_or_default(), window, cx);
        TreemapRow { entry, name, value }
    }

    pub(super) fn treemap(&self) -> Option<&TreemapForm> {
        match &self.form {
            Form::Treemap(form) => Some(form),
            _ => None,
        }
    }

    pub(super) fn treemap_mut(&mut self) -> Option<&mut TreemapForm> {
        match &mut self.form {
            Form::Treemap(form) => Some(form),
            _ => None,
        }
    }

    /// Reads the fields into the document and the rows before a change that
    /// is not typed, so that the class names a rename is recognized by and the
    /// rows' classes are the ones the fields show.
    ///
    /// A class name typed onto one that another definition still has or is
    /// still known by, or not yet a class name at all, is not a rename the
    /// items can follow yet: taking them along now would hand them to the
    /// wrong class once the name is typed back.
    fn sync_treemap(&mut self, cx: &mut Context<Self>) {
        let Some(document) = self.treemap().map(|form| {
            let typed = form
                .class_defs
                .iter()
                .map(|row| field_text(&row.name, cx))
                .collect::<Vec<_>>();
            form.document_with_renames(cx, |_, new| {
                is_valid_treemap_class_name(new)
                    && typed.iter().filter(|name| *name == new).count() == 1
                    && form
                        .document
                        .class_defs
                        .iter()
                        .all(|class_def| class_def.name != new)
            })
        }) else {
            return;
        };
        let Some(form) = self.treemap_mut() else {
            return;
        };
        for (row, entry) in form.rows.iter_mut().zip(&document.entries) {
            row.entry.item.class = entry.item.class.clone();
        }
        form.document = document;
    }

    fn treemap_changed(&mut self, cx: &mut Context<Self>) {
        self.schedule_preview(cx);
        cx.notify();
    }

    fn new_treemap_entry() -> TreemapEntry {
        TreemapEntry {
            depth: 0,
            item: TreemapItem::leaf(i18n::tr(NEW_ITEM_NAME), NEW_ITEM_VALUE),
        }
    }

    fn focus_treemap_name(&self, index: Option<usize>, window: &mut Window, cx: &mut App) {
        if let Some(row) = index.and_then(|index| self.treemap()?.rows.get(index)) {
            row.name.focus_handle(cx).focus(window, cx);
        }
    }

    pub(super) fn add_treemap_item(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.treemap().is_none() {
            return;
        }
        let row = self.new_treemap_row(Self::new_treemap_entry(), window, cx);
        let added = self.treemap_mut().map(|form| {
            form.rows.push(row);
            form.rows.len() - 1
        });
        self.focus_treemap_name(added, window, cx);
        self.treemap_changed(cx);
    }

    /// Whether the row at `index` is a leaf: it has a value, and Mermaid puts
    /// nothing under it.
    fn treemap_row_is_leaf(&self, index: usize, cx: &App) -> bool {
        self.treemap()
            .and_then(|form| form.rows.get(index))
            .is_some_and(|row| !field_is_empty(&row.value, cx))
    }

    pub(super) fn add_treemap_child(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.treemap().is_none_or(|form| index >= form.rows.len())
            || self.treemap_row_is_leaf(index, cx)
        {
            return;
        }
        let row = self.new_treemap_row(Self::new_treemap_entry(), window, cx);
        let added = self
            .treemap_mut()
            .and_then(|form| outline_add_child(&mut form.rows, index, row));
        self.focus_treemap_name(added, window, cx);
        self.treemap_changed(cx);
    }

    pub(super) fn add_treemap_sibling(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.treemap().is_none_or(|form| index >= form.rows.len()) {
            return;
        }
        let row = self.new_treemap_row(Self::new_treemap_entry(), window, cx);
        let added = self
            .treemap_mut()
            .and_then(|form| treemap_add_sibling(&mut form.rows, index, row));
        self.focus_treemap_name(added, window, cx);
        self.treemap_changed(cx);
    }

    pub(super) fn remove_treemap_item(&mut self, index: usize, cx: &mut Context<Self>) {
        let removed = self
            .treemap_mut()
            .and_then(|form| treemap_remove(&mut form.rows, index));
        if removed.is_some() {
            self.treemap_changed(cx);
        }
    }

    pub(super) fn move_treemap_item(
        &mut self,
        index: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        let moved = self
            .treemap_mut()
            .and_then(|form| outline_move(&mut form.rows, index, forward));
        if moved.is_some() {
            self.treemap_changed(cx);
        }
    }

    /// Whether the row at `index` can go under the sibling above it, which has
    /// to be a section.
    fn treemap_can_indent(&self, index: usize, cx: &App) -> bool {
        self.treemap()
            .and_then(|form| outline_previous_sibling(&form.rows, index))
            .is_some_and(|previous| !self.treemap_row_is_leaf(previous, cx))
    }

    pub(super) fn indent_treemap_item(&mut self, index: usize, cx: &mut Context<Self>) {
        if !self.treemap_can_indent(index, cx) {
            return;
        }
        let indented = self
            .treemap_mut()
            .is_some_and(|form| outline_indent(&mut form.rows, index));
        if indented {
            self.treemap_changed(cx);
        }
    }

    pub(super) fn outdent_treemap_item(&mut self, index: usize, cx: &mut Context<Self>) {
        let outdented = self
            .treemap_mut()
            .and_then(|form| treemap_outdent(&mut form.rows, index));
        if outdented.is_some() {
            self.treemap_changed(cx);
        }
    }

    pub(super) fn set_treemap_class(
        &mut self,
        index: usize,
        class: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.sync_treemap(cx);
        let Some(row) = self.treemap_mut().and_then(|form| form.rows.get_mut(index)) else {
            return;
        };
        row.entry.item.class = class;
        self.treemap_changed(cx);
    }

    pub(super) fn add_treemap_class_def(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_treemap(cx);
        let Some(form) = self.treemap() else {
            return;
        };
        let taken = form
            .document
            .class_defs
            .iter()
            .map(|class_def| class_def.name.as_str())
            .collect::<Vec<_>>();
        let name = (1..)
            .map(|number| format!("{NEW_CLASS_NAME}{number}"))
            .find(|name| !taken.contains(&name.as_str()))
            .unwrap_or_else(|| NEW_CLASS_NAME.to_string());
        let row = ClassDefRow {
            name: self.new_field(&name, window, cx),
            style: self.new_field(NEW_CLASS_STYLE, window, cx),
        };
        let field = row.name.clone();
        let Some(form) = self.treemap_mut() else {
            return;
        };
        form.document.class_defs.push(TreemapClassDef {
            name,
            style: NEW_CLASS_STYLE.to_string(),
            terminated: true,
        });
        form.class_defs.push(row);
        field.focus_handle(cx).focus(window, cx);
        self.treemap_changed(cx);
    }

    /// Removes a `classDef` line, and the class from the items that used it
    /// unless another line still defines it.
    pub(super) fn remove_treemap_class_def(&mut self, index: usize, cx: &mut Context<Self>) {
        self.sync_treemap(cx);
        let Some(form) = self.treemap_mut() else {
            return;
        };
        if index >= form.class_defs.len() || index >= form.document.class_defs.len() {
            return;
        }
        let removed = form.document.class_defs.remove(index);
        form.class_defs.remove(index);
        let still_defined = form
            .document
            .class_defs
            .iter()
            .any(|class_def| class_def.name == removed.name);
        if !still_defined {
            for row in &mut form.rows {
                if row.entry.item.class.as_deref() == Some(removed.name.as_str()) {
                    row.entry.item.class = None;
                }
            }
        }
        self.treemap_changed(cx);
    }

    pub(super) fn move_treemap_class_def(
        &mut self,
        index: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        self.sync_treemap(cx);
        let Some(form) = self.treemap_mut() else {
            return;
        };
        let length = form.class_defs.len().min(form.document.class_defs.len());
        let Some(other) = neighbour(index, forward, length) else {
            return;
        };
        if index >= length {
            return;
        }
        form.document.class_defs.swap(index, other);
        form.class_defs.swap(index, other);
        self.treemap_changed(cx);
    }

    pub(super) fn render_treemap(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(form) = self.treemap() else {
            return Vec::new();
        };
        let current = form.current_document(cx);
        let class_names = current
            .class_defs
            .iter()
            .map(|class_def| class_def.name.clone())
            .filter(|name| is_valid_treemap_class_name(name))
            .fold(Vec::new(), |mut names, name| {
                if !names.contains(&name) {
                    names.push(name);
                }
                names
            });

        let title = h_flex()
            .w_full()
            .gap_2()
            .child(section_title(i18n::t!("Title")))
            .child(
                field_frame(
                    &form.title,
                    LABEL_FIELD_WIDTH,
                    current.title.as_deref().is_none_or(is_valid_treemap_title),
                    window,
                    cx,
                )
                .flex_1()
                .child(form.title.clone()),
            );

        let mut items = v_flex()
            .w_full()
            .gap_2()
            .child(section_title(i18n::t!(
                "Items (indented rows are nested deeper; an item with a value is a box at the end of the tree, an item without a value is a section around them)"
            )));
        for index in 0..form.rows.len() {
            let (Some(row), Some(entry)) = (form.rows.get(index), current.entries.get(index))
            else {
                continue;
            };
            items = items.child(self.render_treemap_row(
                form,
                index,
                row,
                entry,
                &class_names,
                window,
                cx,
            ));
        }
        items = items.child(add_button(
            "mermaid-treemap-add-item",
            i18n::t!("Add Item"),
            cx.listener(|this, _, window, cx| this.add_treemap_item(window, cx)),
        ));

        let mut classes = v_flex()
            .w_full()
            .gap_2()
            .child(section_title(i18n::t!("Classes (classDef)")));
        for index in 0..form.class_defs.len() {
            let (Some(row), Some(class_def)) =
                (form.class_defs.get(index), current.class_defs.get(index))
            else {
                continue;
            };
            classes = classes.child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        field_frame(
                            &row.name,
                            ID_FIELD_WIDTH,
                            is_valid_treemap_class_name(&class_def.name),
                            window,
                            cx,
                        )
                        .child(row.name.clone()),
                    )
                    .child(
                        field_frame(
                            &row.style,
                            LABEL_FIELD_WIDTH,
                            is_valid_treemap_style(&class_def.style),
                            window,
                            cx,
                        )
                        .flex_1()
                        .child(row.style.clone()),
                    )
                    .child(row_controls(
                        "mermaid-treemap-class",
                        index,
                        index == 0,
                        index + 1 >= form.class_defs.len(),
                        cx.listener(move |this, _, _window, cx| {
                            this.move_treemap_class_def(index, false, cx)
                        }),
                        cx.listener(move |this, _, _window, cx| {
                            this.move_treemap_class_def(index, true, cx)
                        }),
                        cx.listener(move |this, _, _window, cx| {
                            this.remove_treemap_class_def(index, cx)
                        }),
                    )),
            );
        }
        classes = classes.child(add_button(
            "mermaid-treemap-add-class",
            i18n::t!("Add Class"),
            cx.listener(|this, _, window, cx| this.add_treemap_class_def(window, cx)),
        ));

        vec![
            title.into_any_element(),
            items.into_any_element(),
            classes.into_any_element(),
        ]
    }

    #[allow(clippy::too_many_arguments)]
    fn render_treemap_row(
        &self,
        form: &TreemapForm,
        index: usize,
        row: &TreemapRow,
        entry: &TreemapEntry,
        class_names: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let depth = row.entry.depth;
        let is_leaf = entry.item.value.is_some();
        let can_indent = self.treemap_can_indent(index, cx);
        let can_move_up = outline_previous_sibling(&form.rows, index).is_some();
        let can_move_down = outline_next_sibling(&form.rows, index).is_some();

        let button = |name: &str, icon: IconName, tooltip: &'static str, disabled: bool| {
            IconButton::new(
                SharedString::from(format!("mermaid-treemap-{name}-{index}")),
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
                    i18n::t!("Outdent (Move After the Parent)"),
                    depth == 0,
                )
                .on_click(
                    cx.listener(move |this, _, _window, cx| this.outdent_treemap_item(index, cx)),
                ),
            )
            .child(
                button(
                    "indent",
                    IconName::ArrowRight,
                    i18n::t!("Indent (Move into the Item Above That Has No Value)"),
                    !can_indent,
                )
                .on_click(
                    cx.listener(move |this, _, _window, cx| this.indent_treemap_item(index, cx)),
                ),
            )
            .child(
                button("up", IconName::ArrowUp, i18n::t!("Move Up"), !can_move_up).on_click(
                    cx.listener(move |this, _, _window, cx| {
                        this.move_treemap_item(index, false, cx)
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
                    cx.listener(move |this, _, _window, cx| {
                        this.move_treemap_item(index, true, cx)
                    }),
                ),
            )
            .child(
                button(
                    "add-child",
                    IconName::ArrowDownRight,
                    i18n::t!("Add Item Inside (Only Items Without a Value)"),
                    is_leaf,
                )
                .on_click(
                    cx.listener(move |this, _, window, cx| {
                        this.add_treemap_child(index, window, cx)
                    }),
                ),
            )
            .child(
                button(
                    "add-sibling",
                    IconName::Plus,
                    i18n::t!("Add Item Below at the Same Level"),
                    false,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.add_treemap_sibling(index, window, cx)
                })),
            )
            .child(
                button(
                    "remove",
                    IconName::Trash,
                    i18n::t!("Delete This Item and All Items Inside It"),
                    false,
                )
                .on_click(
                    cx.listener(move |this, _, _window, cx| this.remove_treemap_item(index, cx)),
                ),
            );

        h_flex()
            .w_full()
            .gap_1()
            .pl(rems(DEPTH_INDENT_REMS * depth as f32))
            .child(
                field_frame(
                    &row.name,
                    LABEL_FIELD_WIDTH,
                    is_valid_treemap_name(&entry.item),
                    window,
                    cx,
                )
                .flex_1()
                .child(row.name.clone()),
            )
            .child(
                field_frame(
                    &row.value,
                    NUMBER_FIELD_WIDTH,
                    entry
                        .item
                        .value
                        .as_deref()
                        .is_none_or(is_valid_treemap_value),
                    window,
                    cx,
                )
                .child(row.value.clone()),
            )
            .child(self.render_treemap_class_menu(
                index,
                entry.item.class.clone(),
                class_names,
                window,
                cx,
            ))
            .child(controls)
            .into_any_element()
    }

    fn render_treemap_class_menu(
        &self,
        index: usize,
        current: Option<String>,
        class_names: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let mut choices = class_names.to_vec();
        // A class no `classDef` defines draws no differently, but it is what
        // the source says, so it stays on offer rather than vanish.
        if let Some(current) = &current
            && !choices.contains(current)
        {
            choices.push(current.clone());
        }
        let label = SharedString::from(current.clone().map_or_else(
            || i18n::t!("No Class").to_string(),
            |class| format!(":::{class}"),
        ));
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            let options = std::iter::once(None).chain(choices.iter().cloned().map(Some));
            for class in options {
                let this = this.clone();
                let label = class
                    .clone()
                    .unwrap_or_else(|| i18n::t!("None").to_string());
                let checked = class == current;
                menu = menu.toggleable_entry(
                    label,
                    checked,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        let class = class.clone();
                        this.update(cx, |this, cx| {
                            this.set_treemap_class(index, class, cx);
                        })
                        .log_err();
                    },
                );
            }
            menu
        });

        DropdownMenu::new(
            SharedString::from(format!("mermaid-treemap-class-{index}")),
            label,
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
    }

    pub(super) fn render_treemap_toolbar(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> AnyElement {
        div().into_any_element()
    }
}

#[cfg(test)]
#[path = "treemap_tests.rs"]
mod treemap_tests;
