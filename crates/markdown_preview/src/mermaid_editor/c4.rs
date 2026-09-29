//! The C4 diagram half of the Mermaid editor's form.
//!
//! A C4 diagram is a list of elements, some of them inside boundaries that
//! can nest, and a list of relationships between the elements. The form shows
//! the elements and boundaries as one indented list in the order they are
//! written, because that order is where each one sits, and the relationships
//! below it. A relationship names its ends by alias, so both ends are picked
//! from a menu, and renaming an element carries into every relationship and
//! style update that names it.
//!
//! What a macro's arguments mean depends on the macro, so the form only edits
//! the label, technology, description and type where the macro has them; a
//! text set with `$name="…"` is shown as such and left alone.

use std::rc::Rc;

use editor::Editor;
use gpui::{AnyElement, App, Context, ElementId, Entity, Focusable as _, Window, rems};
use mermaid_render::c4_model::{
    C4_BOUNDARY_MACROS, C4_OFFERED_RELATION_MACROS, C4DiagramType, C4Document, C4Field,
    C4FieldValue, C4Item, C4Statement, c4_macro_label, is_valid_c4_alias, is_valid_c4_text,
    is_valid_c4_title,
};
use ui::{
    ContextMenu, DropdownMenu, DropdownStyle, IconButtonShape, IconPosition, Tooltip, prelude::*,
};
use util::ResultExt as _;

use super::{
    Form, ID_FIELD_WIDTH, LABEL_FIELD_WIDTH, MermaidEditor, add_button, field_frame,
    row_controls_labelled, section_title,
};

/// The texts new rows are added with, so that they are visible in the drawing
/// before anything has been typed into them.
const NEW_ELEMENT_LABEL: &str = i18n::msg!("New element");
const NEW_BOUNDARY_LABEL: &str = i18n::msg!("New boundary");
const NEW_RELATION_LABEL: &str = i18n::msg!("Relationship");
const DEPTH_INDENT_REMS: f32 = 1.;

pub(super) struct C4Form {
    pub(super) document: C4Document,
    pub(super) title: Entity<Editor>,
    /// One entry per item of `document.items`: the fields of an element, a
    /// boundary or a relationship, and `None` for the lines the form does not
    /// show.
    pub(super) rows: Vec<Option<C4Row>>,
}

pub(super) struct C4Row {
    pub(super) alias: Entity<Editor>,
    pub(super) label: Entity<Editor>,
    pub(super) technology: Entity<Editor>,
    pub(super) description: Entity<Editor>,
    pub(super) boundary_type: Entity<Editor>,
}

impl C4Row {
    fn field(&self, field: C4Field) -> &Entity<Editor> {
        match field {
            C4Field::Label => &self.label,
            C4Field::Technology => &self.technology,
            C4Field::Description => &self.description,
            C4Field::Type => &self.boundary_type,
        }
    }
}

type MenuAction = Rc<dyn Fn(&mut MermaidEditor, &mut Window, &mut Context<MermaidEditor>)>;

impl C4Form {
    pub(super) fn fields(&self) -> Vec<Entity<Editor>> {
        let mut declarations = Vec::new();
        let mut relations = Vec::new();
        for (item, row) in self.document.items.iter().zip(&self.rows) {
            let (Some(row), Some(statement)) = (row, statement_of(item)) else {
                continue;
            };
            let is_relation = matches!(item, C4Item::Relation(_));
            let fields = if is_relation {
                &mut relations
            } else {
                declarations.push(row.alias.clone());
                &mut declarations
            };
            for field in C4Field::ALL {
                if matches!(statement.field(field), C4FieldValue::Value(_)) {
                    fields.push(row.field(field).clone());
                }
            }
        }
        std::iter::once(self.title.clone())
            .chain(declarations)
            .chain(relations)
            .collect()
    }

    /// The items with the text fields' current contents written over them and
    /// every reference pointed at the aliases as they are now spelled. The
    /// items stay where they are, so the rows still line up with them.
    fn current_items(&self, cx: &App) -> C4Document {
        let mut document = self.document.clone();
        let mut renames: Vec<(String, String)> = Vec::new();
        let mut unchanged: Vec<String> = Vec::new();
        for (item, row) in document.items.iter_mut().zip(&self.rows) {
            let Some(row) = row else {
                continue;
            };
            let statement = match item {
                C4Item::Element(statement) | C4Item::BoundaryStart(statement) => {
                    let alias = row.alias.read(cx).text(cx).trim().to_string();
                    match statement.alias() {
                        Some(old) if old != alias => {
                            renames.push((old.to_string(), alias.clone()));
                            statement.set_alias(&alias);
                        }
                        Some(old) => unchanged.push(old.to_string()),
                        None => {}
                    }
                    statement
                }
                C4Item::Relation(statement) => statement,
                _ => continue,
            };
            for field in C4Field::ALL {
                if matches!(statement.field(field), C4FieldValue::Value(_)) {
                    statement.set_field(field, &row.field(field).read(cx).text(cx));
                }
            }
        }
        // A structural change bakes a half-typed alias into the document. A
        // rename onto an alias that is still taken, or away from one another
        // declaration still has, cannot say whose relationships are meant, so
        // they stay with the alias they name rather than move to the wrong
        // element.
        let declared = document
            .items
            .iter()
            .filter_map(|item| match item {
                C4Item::Element(statement) | C4Item::BoundaryStart(statement) => {
                    statement.alias().map(str::to_string)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        renames.retain(|(old, new)| {
            !unchanged.contains(old) && declared.iter().filter(|alias| *alias == new).count() == 1
        });
        document.rename_references(&renames);
        document
    }

    /// The C4 diagram the form currently describes.
    pub(super) fn current_document(&self, cx: &App) -> C4Document {
        let mut document = self.current_items(cx);
        let title = self.title.read(cx).text(cx);
        if title != document.title().unwrap_or_default() {
            let title = title.trim();
            document.set_title((!title.is_empty()).then_some(title));
        }
        document
    }
}

fn statement_of(item: &C4Item) -> Option<&C4Statement> {
    match item {
        C4Item::Element(statement)
        | C4Item::BoundaryStart(statement)
        | C4Item::Relation(statement) => Some(statement),
        _ => None,
    }
}

fn statement_of_mut(item: &mut C4Item) -> Option<&mut C4Statement> {
    match item {
        C4Item::Element(statement)
        | C4Item::BoundaryStart(statement)
        | C4Item::Relation(statement) => Some(statement),
        _ => None,
    }
}

/// Puts the rows in the order the document's items were moved into: `order`
/// holds each remaining item's old index.
fn reorder<T>(rows: &mut Vec<Option<T>>, order: &[usize]) {
    let mut old = std::mem::take(rows);
    *rows = order
        .iter()
        .map(|index| old.get_mut(*index).and_then(Option::take))
        .collect();
}

/// The first thing wrong with what the user typed, in the words shown above the
/// form. Every other check lives in `C4Document::structure_error`, which the
/// caller consults right after this; the red frames come from the
/// `is_valid_c4_*` predicates directly.
pub(super) fn problem(document: &C4Document) -> Option<SharedString> {
    // `merman` 0.8 refuses a C4 diagram with nothing in it, so a C4 chosen
    // from the kind picker and confirmed straight away would leave a fence the
    // preview can only report an error about.
    let has_declaration = document
        .items
        .iter()
        .any(|item| matches!(item, C4Item::Element(_) | C4Item::BoundaryStart(_)));
    (!has_declaration).then(|| i18n::t!("Add at least one element or boundary.").into())
}

/// What the kind menu of an element, a boundary or a relationship says.
fn macro_menu_label(name: &str) -> SharedString {
    let label = c4_macro_label(name);
    if label.is_empty() {
        name.to_string().into()
    } else {
        format!("{label} ({name})").into()
    }
}

impl MermaidEditor {
    pub(super) fn new_c4_form(
        &mut self,
        document: C4Document,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> C4Form {
        let title = self.new_field(document.title().unwrap_or_default(), window, cx);
        let mut rows = Vec::with_capacity(document.items.len());
        for item in &document.items {
            let row = statement_of(item).map(|statement| self.new_c4_row(statement, window, cx));
            rows.push(row);
        }
        C4Form {
            document,
            title,
            rows,
        }
    }

    fn new_c4_row(
        &mut self,
        statement: &C4Statement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> C4Row {
        let text = |field: C4Field| match statement.field(field) {
            C4FieldValue::Value(value) => value,
            C4FieldValue::Locked | C4FieldValue::Unavailable => String::new(),
        };
        let (label, technology, description, boundary_type) = (
            text(C4Field::Label),
            text(C4Field::Technology),
            text(C4Field::Description),
            text(C4Field::Type),
        );
        C4Row {
            alias: self.new_field(statement.alias().unwrap_or_default(), window, cx),
            label: self.new_field(&label, window, cx),
            technology: self.new_field(&technology, window, cx),
            description: self.new_field(&description, window, cx),
            boundary_type: self.new_field(&boundary_type, window, cx),
        }
    }

    pub(super) fn c4(&self) -> Option<&C4Form> {
        match &self.form {
            Form::C4(form) => Some(form),
            _ => None,
        }
    }

    pub(super) fn c4_mut(&mut self) -> Option<&mut C4Form> {
        match &mut self.form {
            Form::C4(form) => Some(form),
            _ => None,
        }
    }

    /// Reads the fields into the document before a structural change, which
    /// moves the document and the rows in step. A title typed into an empty
    /// field adds a line, which gets an empty row of its own.
    fn sync_c4(&mut self, cx: &mut Context<Self>) {
        let Some(document) = self.c4().map(|form| form.current_document(cx)) else {
            return;
        };
        let Some(form) = self.c4_mut() else {
            return;
        };
        let title_index = |document: &C4Document| {
            document
                .items
                .iter()
                .position(|item| matches!(item, C4Item::Title(_)))
        };
        match (title_index(&form.document), title_index(&document)) {
            (None, Some(added)) => form.rows.insert(added.min(form.rows.len()), None),
            (Some(removed), None) if removed < form.rows.len() => {
                form.rows.remove(removed);
            }
            _ => {}
        }
        form.document = document;
    }

    fn c4_changed(&mut self, cx: &mut Context<Self>) {
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn set_c4_diagram_type(
        &mut self,
        diagram_type: C4DiagramType,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.c4_mut() else {
            return;
        };
        form.document.diagram_type = diagram_type;
        self.c4_changed(cx);
    }

    pub(super) fn add_c4_element(
        &mut self,
        parent: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_c4(cx);
        let Some(form) = self.c4() else {
            return;
        };
        let alias = form.document.unused_alias("element");
        let statement = C4Statement::declaration(
            form.document.diagram_type.default_element_macro(),
            &alias,
            i18n::tr(NEW_ELEMENT_LABEL),
        );
        let at = form.document.insertion_point(parent);
        let row = self.new_c4_row(&statement, window, cx);
        let focus = row.label.clone();
        let Some(form) = self.c4_mut() else {
            return;
        };
        let at = at.min(form.document.items.len()).min(form.rows.len());
        form.document.items.insert(at, C4Item::Element(statement));
        form.rows.insert(at, Some(row));
        focus.focus_handle(cx).focus(window, cx);
        self.c4_changed(cx);
    }

    pub(super) fn add_c4_boundary(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_c4(cx);
        let Some(form) = self.c4() else {
            return;
        };
        let alias = form.document.unused_alias("boundary");
        let statement = C4Statement::declaration(
            form.document.diagram_type.default_boundary_macro(),
            &alias,
            i18n::tr(NEW_BOUNDARY_LABEL),
        );
        let at = form.document.insertion_point(None);
        let row = self.new_c4_row(&statement, window, cx);
        let focus = row.label.clone();
        let Some(form) = self.c4_mut() else {
            return;
        };
        let at = at.min(form.document.items.len()).min(form.rows.len());
        form.document.insert_boundary(at, statement);
        form.rows.insert(at, None);
        form.rows.insert(at, Some(row));
        focus.focus_handle(cx).focus(window, cx);
        self.c4_changed(cx);
    }

    pub(super) fn add_c4_relation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_c4(cx);
        let Some(form) = self.c4() else {
            return;
        };
        let aliases = form.document.element_aliases();
        let Some(from) = aliases.first().cloned() else {
            self.message = Some(i18n::t!("Add an element first.").into());
            cx.notify();
            return;
        };
        let to = aliases.get(1).cloned().unwrap_or_else(|| from.clone());
        let statement = C4Statement::relation("Rel", &from, &to, i18n::tr(NEW_RELATION_LABEL));
        let row = self.new_c4_row(&statement, window, cx);
        let focus = row.label.clone();
        let Some(form) = self.c4_mut() else {
            return;
        };
        form.document.items.push(C4Item::Relation(statement));
        form.rows.push(Some(row));
        focus.focus_handle(cx).focus(window, cx);
        self.c4_changed(cx);
    }

    /// Removes an element with the relationships that join it, a boundary
    /// while keeping what is inside it, or a relationship.
    pub(super) fn remove_c4_item(&mut self, index: usize, cx: &mut Context<Self>) {
        self.sync_c4(cx);
        let Some(form) = self.c4_mut() else {
            return;
        };
        let order = match form.document.items.get(index) {
            Some(C4Item::Element(_)) => form.document.remove_element(index),
            Some(C4Item::BoundaryStart(_)) => form.document.remove_boundary(index),
            Some(C4Item::Relation(_)) => form.document.remove_relation(index),
            _ => return,
        };
        reorder(&mut form.rows, &order);
        self.c4_changed(cx);
    }

    pub(super) fn move_c4_item(&mut self, index: usize, forward: bool, cx: &mut Context<Self>) {
        self.sync_c4(cx);
        let Some(form) = self.c4_mut() else {
            return;
        };
        let order = match form.document.items.get(index) {
            Some(C4Item::Element(_) | C4Item::BoundaryStart(_)) => {
                form.document.move_in_tree(index, forward)
            }
            Some(C4Item::Relation(_)) => form.document.move_relation(index, forward),
            _ => return,
        };
        reorder(&mut form.rows, &order);
        self.c4_changed(cx);
    }

    /// Moves an element into the boundary starting at `parent`, or out of
    /// every boundary.
    pub(super) fn set_c4_parent(
        &mut self,
        index: usize,
        parent: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        self.sync_c4(cx);
        let Some(form) = self.c4_mut() else {
            return;
        };
        let order = form.document.move_element_to(index, parent);
        reorder(&mut form.rows, &order);
        self.c4_changed(cx);
    }

    /// Turns the item at `index` into the macro `name`, then shows the texts
    /// where the new macro reads them.
    pub(super) fn set_c4_macro(
        &mut self,
        index: usize,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_c4(cx);
        let Some(form) = self.c4_mut() else {
            return;
        };
        let Some(statement) = form
            .document
            .items
            .get_mut(index)
            .and_then(statement_of_mut)
        else {
            return;
        };
        statement.set_macro(name);
        let statement = statement.clone();
        let Some(Some(row)) = form.rows.get(index) else {
            return;
        };
        let fields = C4Field::ALL.map(|field| (row.field(field).clone(), statement.field(field)));
        for (editor, value) in fields {
            if let C4FieldValue::Value(value) = value {
                editor.update(cx, |editor, cx| {
                    if editor.text(cx) != value {
                        editor.set_text(value, window, cx);
                    }
                });
            }
        }
        self.c4_changed(cx);
    }

    pub(super) fn set_c4_relation_end(
        &mut self,
        index: usize,
        is_from: bool,
        alias: &str,
        cx: &mut Context<Self>,
    ) {
        self.sync_c4(cx);
        let Some(form) = self.c4_mut() else {
            return;
        };
        let Some(C4Item::Relation(statement)) = form.document.items.get_mut(index) else {
            return;
        };
        statement.set_relation_end(is_from, alias);
        self.c4_changed(cx);
    }

    pub(super) fn render_c4_toolbar(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(form) = self.c4() else {
            return div().into_any_element();
        };
        let current = form.document.diagram_type;
        let choices = C4DiagramType::ALL
            .into_iter()
            .map(|diagram_type| {
                let action: MenuAction =
                    Rc::new(move |this, _window, cx| this.set_c4_diagram_type(diagram_type, cx));
                (
                    SharedString::from(format!(
                        "{} ({})",
                        diagram_type.label(),
                        diagram_type.keyword()
                    )),
                    diagram_type == current,
                    action,
                )
            })
            .collect();
        h_flex()
            .gap_2()
            .child(
                Label::new(i18n::t!("Diagram Type"))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(render_choice_menu(
                "mermaid-c4-diagram-type",
                current.label().into(),
                choices,
                window,
                cx,
            ))
            .into_any_element()
    }

    pub(super) fn render_c4(&self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(form) = self.c4() else {
            return Vec::new();
        };
        let current = form.current_items(cx);
        let title_text = form.title.read(cx).text(cx);
        let title_is_valid = title_text.trim().is_empty() || is_valid_c4_title(title_text.trim());
        let styles = current
            .items
            .iter()
            .filter(|item| matches!(item, C4Item::Style(_)))
            .count();

        let mut elements = vec![
            h_flex()
                .w_full()
                .gap_2()
                .child(section_title(i18n::t!("Title")))
                .child(
                    field_frame(&form.title, LABEL_FIELD_WIDTH, title_is_valid, window, cx)
                        .flex_1()
                        .child(form.title.clone()),
                )
                .into_any_element(),
            self.render_c4_declarations(form, &current, window, cx),
            self.render_c4_relations(form, &current, window, cx),
        ];
        if styles > 0 {
            elements.push(
                Label::new(i18n::tn!(
                    styles,
                    "{count} style statement (UpdateElementStyle and the like) is kept as is and follows alias changes.",
                    "{count} style statements (UpdateElementStyle and the like) are kept as is and follow alias changes."
                ))
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
            );
        }
        elements
    }

    fn render_c4_declarations(
        &self,
        form: &C4Form,
        current: &C4Document,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut list = v_flex().w_full().gap_2().child(section_title(i18n::t!(
            "Elements and Boundaries (indented inside a boundary)"
        )));
        for (index, (item, row)) in current.items.iter().zip(&form.rows).enumerate() {
            let Some(row) = row else {
                continue;
            };
            let depth = current.depth(index);
            let body = match item {
                C4Item::Element(statement) => {
                    self.render_c4_element(index, statement, row, current, window, cx)
                }
                C4Item::BoundaryStart(statement) => {
                    self.render_c4_boundary(index, statement, row, current, window, cx)
                }
                _ => continue,
            };
            list = list.child(
                div()
                    .w_full()
                    .pl(rems(DEPTH_INDENT_REMS * depth as f32))
                    .child(body),
            );
        }
        list.child(
            h_flex()
                .gap_2()
                .child(add_button(
                    "mermaid-c4-add-element",
                    i18n::t!("Add Element"),
                    cx.listener(|this, _, window, cx| this.add_c4_element(None, window, cx)),
                ))
                .child(add_button(
                    "mermaid-c4-add-boundary",
                    i18n::t!("Add Boundary"),
                    cx.listener(|this, _, window, cx| this.add_c4_boundary(window, cx)),
                )),
        )
        .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_c4_element(
        &self,
        index: usize,
        statement: &C4Statement,
        row: &C4Row,
        current: &C4Document,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let offered = current.diagram_type.element_macros();
        let kind_menu = self.render_c4_macro_menu(index, statement, &offered, window, cx);

        let parent = current.parent(index);
        let mut parent_choices = vec![(
            SharedString::from(i18n::t!("Top Level")),
            parent.is_none(),
            Rc::new(
                move |this: &mut MermaidEditor, _: &mut Window, cx: &mut Context<MermaidEditor>| {
                    this.set_c4_parent(index, None, cx)
                },
            ) as MenuAction,
        )];
        for (start, alias) in current.boundaries() {
            parent_choices.push((
                SharedString::from(alias),
                parent == Some(start),
                Rc::new(
                    move |this: &mut MermaidEditor,
                          _: &mut Window,
                          cx: &mut Context<MermaidEditor>| {
                        this.set_c4_parent(index, Some(start), cx)
                    },
                ),
            ));
        }
        let parent_label = parent
            .and_then(|start| current.items.get(start))
            .and_then(statement_of)
            .and_then(C4Statement::alias)
            .unwrap_or(i18n::t!("Top Level"))
            .to_string();

        v_flex()
            .w_full()
            .gap_1()
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(kind_menu)
                    .child(self.render_c4_alias(row, window, cx))
                    .child(self.render_c4_field(statement, C4Field::Label, row, true, window, cx))
                    .child(self.render_c4_row_controls(
                        index,
                        current,
                        i18n::t!("Delete the Element and Its Relationships"),
                        cx,
                    )),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .pl_4()
                    .children(self.render_c4_labelled_field(
                        i18n::t!("Technology"),
                        statement,
                        C4Field::Technology,
                        row,
                        window,
                        cx,
                    ))
                    .children(self.render_c4_labelled_field(
                        i18n::t!("Description"),
                        statement,
                        C4Field::Description,
                        row,
                        window,
                        cx,
                    ))
                    .child(
                        Label::new(i18n::tc!("mermaid c4 element", "Parent"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(render_choice_menu(
                        SharedString::from(format!("mermaid-c4-parent-{index}")),
                        parent_label.into(),
                        parent_choices,
                        window,
                        cx,
                    )),
            )
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_c4_boundary(
        &self,
        index: usize,
        statement: &C4Statement,
        row: &C4Row,
        current: &C4Document,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let offered = C4_BOUNDARY_MACROS
            .iter()
            .copied()
            .filter(|name| *name == statement.name || statement.can_become(name))
            .collect::<Vec<_>>();
        let kind_menu = self.render_c4_macro_menu(index, statement, &offered, window, cx);
        let second_line = [C4Field::Type, C4Field::Description]
            .into_iter()
            .filter(|field| statement.field(*field) != C4FieldValue::Unavailable)
            .collect::<Vec<_>>();

        v_flex()
            .w_full()
            .gap_1()
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(kind_menu)
                    .child(self.render_c4_alias(row, window, cx))
                    .child(self.render_c4_field(statement, C4Field::Label, row, true, window, cx))
                    .child(
                        IconButton::new(
                            SharedString::from(format!("mermaid-c4-add-inside-{index}")),
                            IconName::Plus,
                        )
                        .shape(IconButtonShape::Square)
                        .icon_size(IconSize::XSmall)
                        .tooltip(Tooltip::text(i18n::t!(
                            "Add an Element Inside This Boundary"
                        )))
                        .on_click(cx.listener(
                            move |this, _, window, cx| this.add_c4_element(Some(index), window, cx),
                        )),
                    )
                    .child(self.render_c4_row_controls(
                        index,
                        current,
                        i18n::t!("Delete the Boundary (the elements inside are kept)"),
                        cx,
                    )),
            )
            .when(!second_line.is_empty(), |this| {
                this.child(h_flex().w_full().gap_1().pl_4().children(
                    second_line.into_iter().flat_map(|field| {
                        let label = match field {
                            C4Field::Type => i18n::t!("Type"),
                            _ => i18n::t!("Description"),
                        };
                        self.render_c4_labelled_field(label, statement, field, row, window, cx)
                    }),
                ))
            })
            .into_any_element()
    }

    fn render_c4_relations(
        &self,
        form: &C4Form,
        current: &C4Document,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let aliases = current.element_aliases();
        let mut list = v_flex()
            .w_full()
            .gap_2()
            .child(section_title(i18n::t!("Relationships")));
        for (index, (item, row)) in current.items.iter().zip(&form.rows).enumerate() {
            let (C4Item::Relation(statement), Some(row)) = (item, row) else {
                continue;
            };
            let Some((from, to)) = statement.relation_ends() else {
                continue;
            };
            let kind_menu = self.render_c4_macro_menu(
                index,
                statement,
                &C4_OFFERED_RELATION_MACROS,
                window,
                cx,
            );
            list = list.child(
                v_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .child(kind_menu)
                            .child(self.render_c4_end_menu(index, true, from, &aliases, window, cx))
                            .child(Label::new("→").size(LabelSize::Small).color(Color::Muted))
                            .child(self.render_c4_end_menu(index, false, to, &aliases, window, cx))
                            .child(self.render_c4_field(
                                statement,
                                C4Field::Label,
                                row,
                                true,
                                window,
                                cx,
                            ))
                            .child(self.render_c4_row_controls(
                                index,
                                current,
                                i18n::t!("Delete Relationship"),
                                cx,
                            )),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .pl_4()
                            .children(self.render_c4_labelled_field(
                                i18n::t!("Technology"),
                                statement,
                                C4Field::Technology,
                                row,
                                window,
                                cx,
                            ))
                            .children(self.render_c4_labelled_field(
                                i18n::t!("Description"),
                                statement,
                                C4Field::Description,
                                row,
                                window,
                                cx,
                            )),
                    )
                    .when(current.relation_is_replaced_later(index), |this| {
                        this.child(
                            Label::new(i18n::t!(
                                "A later {from} → {to} relationship overrides this one (the later one's kind and text are drawn)."
                            ))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        )
                    }),
            );
        }
        list.child(add_button(
            "mermaid-c4-add-relation",
            i18n::t!("Add Relationship"),
            cx.listener(|this, _, window, cx| this.add_c4_relation(window, cx)),
        ))
        .into_any_element()
    }

    fn render_c4_alias(
        &self,
        row: &C4Row,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_valid = is_valid_c4_alias(row.alias.read(cx).text(cx).trim());
        field_frame(&row.alias, ID_FIELD_WIDTH, is_valid, window, cx)
            .child(row.alias.clone())
            .into_any_element()
    }

    /// One text of a statement: its field, or a note in its place when the
    /// text is set by name and so left as it is.
    fn render_c4_field(
        &self,
        statement: &C4Statement,
        field: C4Field,
        row: &C4Row,
        grow: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match statement.field(field) {
            C4FieldValue::Value(_) => {
                let editor = row.field(field);
                let is_valid = is_valid_c4_text(&editor.read(cx).text(cx));
                field_frame(editor, LABEL_FIELD_WIDTH, is_valid, window, cx)
                    .when(grow, |frame| frame.flex_1())
                    .child(editor.clone())
                    .into_any_element()
            }
            C4FieldValue::Locked => div()
                .when(grow, |this| this.flex_1())
                .child(
                    Label::new(i18n::t!("(set by name with $name=; kept as is)"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            C4FieldValue::Unavailable => div().into_any_element(),
        }
    }

    /// A field with its name in front of it, or nothing when the macro has no
    /// such text.
    fn render_c4_labelled_field(
        &self,
        label: &'static str,
        statement: &C4Statement,
        field: C4Field,
        row: &C4Row,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        if statement.field(field) == C4FieldValue::Unavailable {
            return Vec::new();
        }
        vec![
            Label::new(label)
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
            self.render_c4_field(
                statement,
                field,
                row,
                field == C4Field::Description,
                window,
                cx,
            ),
        ]
    }

    fn render_c4_row_controls(
        &self,
        index: usize,
        current: &C4Document,
        remove_tooltip: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_relation = matches!(current.items.get(index), Some(C4Item::Relation(_)));
        let (at_start, at_end) = if is_relation {
            (
                !current.can_move_relation(index, false),
                !current.can_move_relation(index, true),
            )
        } else {
            (
                !current.can_move_in_tree(index, false),
                !current.can_move_in_tree(index, true),
            )
        };
        row_controls_labelled(
            "mermaid-c4-item",
            index,
            at_start,
            at_end,
            remove_tooltip,
            cx.listener(move |this, _, _window, cx| this.move_c4_item(index, false, cx)),
            cx.listener(move |this, _, _window, cx| this.move_c4_item(index, true, cx)),
            cx.listener(move |this, _, _window, cx| this.remove_c4_item(index, cx)),
        )
        .into_any_element()
    }

    /// The kind menu: the macros in `offered` the statement can become, with
    /// the one it is checked. A macro a diagram already uses stays listed
    /// even when the form would not offer it for a new item.
    fn render_c4_macro_menu(
        &self,
        index: usize,
        statement: &C4Statement,
        offered: &[&'static str],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current = statement.name.clone();
        let mut names = offered
            .iter()
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>();
        if !names.contains(&current) {
            names.insert(0, current.clone());
        }
        let mut choices: Vec<(SharedString, bool, MenuAction)> = Vec::new();
        for name in names {
            if name != current && !statement.can_become(&name) {
                continue;
            }
            let label = macro_menu_label(&name);
            let is_current = name == current;
            let action: MenuAction =
                Rc::new(move |this, window, cx| this.set_c4_macro(index, &name, window, cx));
            choices.push((label, is_current, action));
        }
        render_choice_menu(
            SharedString::from(format!("mermaid-c4-kind-{index}")),
            macro_menu_label(&current),
            choices,
            window,
            cx,
        )
    }

    fn render_c4_end_menu(
        &self,
        index: usize,
        is_from: bool,
        current: &str,
        aliases: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let choices = aliases
            .iter()
            .map(|alias| {
                let chosen = alias.clone();
                let action: MenuAction = Rc::new(move |this, _window, cx| {
                    this.set_c4_relation_end(index, is_from, &chosen, cx)
                });
                (SharedString::from(alias.clone()), alias == current, action)
            })
            .collect();
        render_choice_menu(
            SharedString::from(format!(
                "mermaid-c4-{}-{index}",
                if is_from { "from" } else { "to" }
            )),
            current.to_string().into(),
            choices,
            window,
            cx,
        )
    }
}

/// A drop-down whose entries run `action` on the editor, with the chosen one
/// checked.
fn render_choice_menu(
    id: impl Into<ElementId>,
    label: SharedString,
    choices: Vec<(SharedString, bool, MenuAction)>,
    window: &mut Window,
    cx: &mut Context<MermaidEditor>,
) -> AnyElement {
    let this = cx.entity().downgrade();
    let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
        for (label, selected, action) in &choices {
            let this = this.clone();
            let action = action.clone();
            menu = menu.toggleable_entry(
                label.clone(),
                *selected,
                IconPosition::Start,
                None,
                move |window, cx| {
                    this.update(cx, |this, cx| action(this, window, cx))
                        .log_err();
                },
            );
        }
        menu
    });
    DropdownMenu::new(id, label, menu)
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
}

#[cfg(test)]
#[path = "c4_tests.rs"]
mod c4_tests;
