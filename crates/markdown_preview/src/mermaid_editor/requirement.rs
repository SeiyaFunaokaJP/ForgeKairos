//! The requirement diagram half of the Mermaid editor's form.
//!
//! A requirement diagram is two lists of boxes — the requirements and the
//! elements that satisfy, verify or trace them — and the relationships between
//! them. A relationship names its ends by box name, so both ends are picked
//! from a menu rather than typed, and renaming a box carries into every
//! relationship and styling statement that named it. Deleting a box takes
//! those along, and its delete button says so.

use editor::Editor;
use gpui::{AnyElement, App, Context, Entity, Window};
use mermaid_render::requirement_model::{
    RelationshipKind, Requirement, RequirementDirection, RequirementDocument, RequirementElement,
    RequirementKind, RequirementRelationship, RequirementRisk, RequirementStyleStatement,
    RequirementValue, VerifyMethod, invalid_name_message, invalid_value_message,
    is_valid_requirement_class_name, is_valid_requirement_name, is_valid_requirement_value,
};
use ui::{ContextMenu, DropdownMenu, DropdownStyle, IconPosition, prelude::*};
use util::ResultExt as _;

use super::class_menu::{
    ToggleClass, class_candidates, declared_class_names, render_class_input, render_class_menu,
};
use super::{
    Form, ID_FIELD_WIDTH, LABEL_FIELD_WIDTH, MermaidEditor, NUMBER_FIELD_WIDTH, add_button,
    field_frame, neighbour, row_controls, row_controls_labelled, section_title,
};

pub(super) struct RequirementForm {
    pub(super) document: RequirementDocument,
    pub(super) requirements: Vec<RequirementRow>,
    pub(super) elements: Vec<ElementRow>,
    /// A class name typed by hand, offered beside the `classDef` names in
    /// every box's class menu.
    pub(super) class_input: Entity<Editor>,
}

pub(super) struct RequirementRow {
    pub(super) name: Entity<Editor>,
    pub(super) id: Entity<Editor>,
    pub(super) text: Entity<Editor>,
}

pub(super) struct ElementRow {
    pub(super) name: Entity<Editor>,
    pub(super) element_type: Entity<Editor>,
    pub(super) doc_ref: Entity<Editor>,
}

/// Which list a box is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BoxList {
    Requirements,
    Elements,
}

impl RequirementForm {
    pub(super) fn fields(&self) -> Vec<Entity<Editor>> {
        self.requirements
            .iter()
            .flat_map(|row| [row.name.clone(), row.id.clone(), row.text.clone()])
            .chain(self.elements.iter().flat_map(|row| {
                [
                    row.name.clone(),
                    row.element_type.clone(),
                    row.doc_ref.clone(),
                ]
            }))
            .chain([self.class_input.clone()])
            .collect()
    }

    /// The requirement diagram the form currently describes, with every
    /// relationship and styling statement pointed at the names as they are now
    /// spelled.
    pub(super) fn current_document(&self, cx: &App) -> RequirementDocument {
        self.document_with_renames(cx, |_, _| true)
    }

    /// The document the fields describe, with the relationships and styling
    /// statements carried along only by the renames `follow` accepts. A rename
    /// it refuses leaves the stored name as it was, so the rename is still
    /// pending once it settles.
    fn document_with_renames(
        &self,
        cx: &App,
        follow: impl Fn(&str, &str) -> bool,
    ) -> RequirementDocument {
        let mut document = self.document.clone();
        let mut renames: Vec<(String, String)> = Vec::new();
        let mut rename = |stored: &mut String, field: &Entity<Editor>| {
            let name = field.read(cx).text(cx).trim().to_string();
            if name != *stored && follow(stored, &name) {
                renames.push((std::mem::replace(stored, name.clone()), name));
            }
        };
        for (requirement, row) in document.requirements.iter_mut().zip(&self.requirements) {
            rename(&mut requirement.name, &row.name);
            read_value(&mut requirement.id, &row.id, cx);
            read_value(&mut requirement.text, &row.text, cx);
        }
        for (element, row) in document.elements.iter_mut().zip(&self.elements) {
            rename(&mut element.name, &row.name);
            read_value(&mut element.element_type, &row.element_type, cx);
            read_value(&mut element.doc_ref, &row.doc_ref, cx);
        }
        document.rename(&renames);
        document
    }

    /// The class names the class menus offer.
    fn class_candidates(&self, cx: &App) -> Vec<String> {
        let declared = declared_class_names(
            self.document
                .style_statements
                .iter()
                .filter_map(|statement| match statement {
                    RequirementStyleStatement::ClassDef(text) => Some(text.as_str()),
                    _ => None,
                }),
            is_valid_requirement_class_name,
        );
        class_candidates(
            declared,
            self.document
                .requirements
                .iter()
                .flat_map(|requirement| &requirement.classes)
                .chain(
                    self.document
                        .elements
                        .iter()
                        .flat_map(|element| &element.classes),
                ),
            &self.class_input.read(cx).text(cx),
            is_valid_requirement_class_name,
        )
    }
}

fn read_value(value: &mut RequirementValue, field: &Entity<Editor>, cx: &App) {
    value.value = field.read(cx).text(cx).trim().to_string();
}

/// The first thing wrong with what the user typed, in the words shown above the
/// form. The rest — a duplicate name, a relationship to a box that is gone —
/// comes from [`RequirementDocument::structure_error`].
pub(super) fn problem(document: &RequirementDocument) -> Option<SharedString> {
    let names = document
        .requirements
        .iter()
        .map(|requirement| &requirement.name)
        .chain(document.elements.iter().map(|element| &element.name));
    for name in names {
        if !is_valid_requirement_name(name) {
            return Some(invalid_name_message(name).into());
        }
    }
    for requirement in &document.requirements {
        for value in [&requirement.id, &requirement.text] {
            if !is_valid_requirement_value(&value.value) {
                return Some(invalid_value_message(&requirement.name, &value.value).into());
            }
        }
    }
    for element in &document.elements {
        for value in [&element.element_type, &element.doc_ref] {
            if !is_valid_requirement_value(&value.value) {
                return Some(invalid_value_message(&element.name, &value.value).into());
            }
        }
    }
    None
}

impl MermaidEditor {
    pub(super) fn new_requirement_form(
        &mut self,
        document: RequirementDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> RequirementForm {
        let mut requirements = Vec::with_capacity(document.requirements.len());
        for requirement in &document.requirements {
            requirements.push(self.new_requirement_row(requirement, window, cx));
        }
        let mut elements = Vec::with_capacity(document.elements.len());
        for element in &document.elements {
            elements.push(self.new_element_row(element, window, cx));
        }
        RequirementForm {
            document,
            requirements,
            elements,
            class_input: self.new_field("", window, cx),
        }
    }

    fn new_requirement_row(
        &mut self,
        requirement: &Requirement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> RequirementRow {
        RequirementRow {
            name: self.new_field(&requirement.name, window, cx),
            id: self.new_placeholder_field(&requirement.id.value, "ID", window, cx),
            text: self.new_placeholder_field(
                &requirement.text.value,
                i18n::t!("Requirement text (text)"),
                window,
                cx,
            ),
        }
    }

    fn new_element_row(
        &mut self,
        element: &RequirementElement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ElementRow {
        ElementRow {
            name: self.new_field(&element.name, window, cx),
            element_type: self.new_placeholder_field(
                &element.element_type.value,
                i18n::t!("Element type (type)"),
                window,
                cx,
            ),
            doc_ref: self.new_placeholder_field(
                &element.doc_ref.value,
                i18n::t!("Document reference (docref)"),
                window,
                cx,
            ),
        }
    }

    fn new_placeholder_field(
        &mut self,
        text: &str,
        placeholder: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Editor> {
        let field = self.new_field(text, window, cx);
        field.update(cx, |field, cx| {
            field.set_placeholder_text(placeholder, window, cx);
        });
        field
    }

    pub(super) fn requirement(&self) -> Option<&RequirementForm> {
        match &self.form {
            Form::Requirement(form) => Some(form),
            _ => None,
        }
    }

    fn requirement_mut(&mut self) -> Option<&mut RequirementForm> {
        match &mut self.form {
            Form::Requirement(form) => Some(form),
            _ => None,
        }
    }

    /// The form with its document brought up to what the fields say, so that
    /// a change to the lists or the menus starts from the names as they are
    /// spelled now rather than as they were when the form opened.
    ///
    /// A name typed onto one that another box still has or is still known by,
    /// or not yet a name at all, is not a rename the relationships can follow
    /// yet: taking them along now would hand them to the wrong box once the
    /// name is typed back.
    fn synced_requirement_form(&mut self, cx: &App) -> Option<&mut RequirementForm> {
        let form = self.requirement()?;
        let typed = form
            .requirements
            .iter()
            .map(|row| &row.name)
            .chain(form.elements.iter().map(|row| &row.name))
            .map(|field| field.read(cx).text(cx).trim().to_string())
            .collect::<Vec<_>>();
        let stored = form
            .document
            .requirements
            .iter()
            .map(|requirement| requirement.name.as_str())
            .chain(
                form.document
                    .elements
                    .iter()
                    .map(|element| element.name.as_str()),
            )
            .collect::<Vec<_>>();
        let document = form.document_with_renames(cx, |_, new| {
            is_valid_requirement_name(new)
                && typed.iter().filter(|name| *name == new).count() == 1
                && !stored.contains(&new)
        });
        let form = self.requirement_mut()?;
        form.document = document;
        Some(form)
    }

    fn requirement_changed(&mut self, cx: &mut Context<Self>) {
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn add_requirement(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.synced_requirement_form(cx) else {
            return;
        };
        let requirement = Requirement {
            kind: RequirementKind::Requirement,
            name: form.document.unused_name(i18n::t!("Requirement")),
            id: RequirementValue::new("", false),
            text: RequirementValue::new("", true),
            risk: None,
            verify_method: None,
            classes: Vec::new(),
            body_comments: Vec::new(),
        };
        let row = self.new_requirement_row(&requirement, window, cx);
        let Some(form) = self.requirement_mut() else {
            return;
        };
        form.document.requirements.push(requirement);
        form.requirements.push(row);
        self.requirement_changed(cx);
    }

    pub(super) fn add_requirement_element(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.synced_requirement_form(cx) else {
            return;
        };
        let element = RequirementElement {
            name: form.document.unused_name(i18n::t!("Element")),
            element_type: RequirementValue::new("", true),
            doc_ref: RequirementValue::new("", false),
            classes: Vec::new(),
            body_comments: Vec::new(),
        };
        let row = self.new_element_row(&element, window, cx);
        let Some(form) = self.requirement_mut() else {
            return;
        };
        form.document.elements.push(element);
        form.elements.push(row);
        self.requirement_changed(cx);
    }

    /// Deletes a box together with the relationships and styling statements
    /// that name it, which would otherwise point at nothing.
    pub(super) fn remove_requirement_box(
        &mut self,
        list: BoxList,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.synced_requirement_form(cx) else {
            return;
        };
        let removed = match list {
            BoxList::Requirements => {
                if index >= form.requirements.len() || index >= form.document.requirements.len() {
                    return;
                }
                form.requirements.remove(index);
                form.document.requirements.remove(index).name
            }
            BoxList::Elements => {
                if index >= form.elements.len() || index >= form.document.elements.len() {
                    return;
                }
                form.elements.remove(index);
                form.document.elements.remove(index).name
            }
        };
        form.document.remove_references(&removed);
        self.requirement_changed(cx);
    }

    pub(super) fn move_requirement_box(
        &mut self,
        list: BoxList,
        index: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.synced_requirement_form(cx) else {
            return;
        };
        match list {
            BoxList::Requirements => {
                let Some(other) = neighbour(index, forward, form.requirements.len()) else {
                    return;
                };
                if other >= form.document.requirements.len()
                    || index >= form.document.requirements.len()
                {
                    return;
                }
                form.requirements.swap(index, other);
                form.document.requirements.swap(index, other);
            }
            BoxList::Elements => {
                let Some(other) = neighbour(index, forward, form.elements.len()) else {
                    return;
                };
                if other >= form.document.elements.len() || index >= form.document.elements.len() {
                    return;
                }
                form.elements.swap(index, other);
                form.document.elements.swap(index, other);
            }
        }
        self.requirement_changed(cx);
    }

    pub(super) fn update_requirement(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut Requirement),
    ) {
        let Some(requirement) = self
            .synced_requirement_form(cx)
            .and_then(|form| form.document.requirements.get_mut(index))
        else {
            return;
        };
        edit(requirement);
        self.requirement_changed(cx);
    }

    /// Adds `class` to a box, or takes it away when the box already has it.
    /// The classes are the ones written with `:::` on the box's first line.
    pub(super) fn toggle_requirement_box_class(
        &mut self,
        list: BoxList,
        index: usize,
        class: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.synced_requirement_form(cx) else {
            return;
        };
        let classes = match list {
            BoxList::Requirements => form
                .document
                .requirements
                .get_mut(index)
                .map(|requirement| &mut requirement.classes),
            BoxList::Elements => form
                .document
                .elements
                .get_mut(index)
                .map(|element| &mut element.classes),
        };
        let Some(classes) = classes else {
            return;
        };
        match classes.iter().position(|name| name == class) {
            Some(position) => {
                classes.remove(position);
            }
            None => classes.push(class.to_string()),
        }
        self.requirement_changed(cx);
    }

    pub(super) fn add_requirement_relationship(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.synced_requirement_form(cx) else {
            return;
        };
        let document = &form.document;
        // The usual reading of the diagram: an element satisfies a
        // requirement.
        let source = document
            .elements
            .first()
            .map(|element| element.name.clone())
            .or_else(|| document.names().into_iter().next());
        let target = document
            .requirements
            .first()
            .map(|requirement| requirement.name.clone())
            .or_else(|| document.names().into_iter().last());
        let (Some(source), Some(target)) = (source, target) else {
            self.message = Some(i18n::t!("Add a requirement or an element first.").into());
            cx.notify();
            return;
        };
        form.document.relationships.push(RequirementRelationship {
            source,
            kind: RelationshipKind::Satisfies,
            target,
        });
        self.requirement_changed(cx);
    }

    pub(super) fn remove_requirement_relationship(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(form) = self.synced_requirement_form(cx) else {
            return;
        };
        if index >= form.document.relationships.len() {
            return;
        }
        form.document.relationships.remove(index);
        self.requirement_changed(cx);
    }

    pub(super) fn move_requirement_relationship(
        &mut self,
        index: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.synced_requirement_form(cx) else {
            return;
        };
        let Some(other) = neighbour(index, forward, form.document.relationships.len()) else {
            return;
        };
        if index >= form.document.relationships.len() {
            return;
        }
        form.document.relationships.swap(index, other);
        self.requirement_changed(cx);
    }

    pub(super) fn update_requirement_relationship(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut RequirementRelationship),
    ) {
        let Some(relationship) = self
            .synced_requirement_form(cx)
            .and_then(|form| form.document.relationships.get_mut(index))
        else {
            return;
        };
        edit(relationship);
        self.requirement_changed(cx);
    }

    pub(super) fn set_requirement_direction(
        &mut self,
        direction: Option<RequirementDirection>,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.requirement_mut() else {
            return;
        };
        form.document.direction = direction;
        self.requirement_changed(cx);
    }

    pub(super) fn render_requirement(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(form) = self.requirement() else {
            return Vec::new();
        };
        // The names as they are typed right now, which is what the
        // relationship menus show and offer.
        let document = form.current_document(cx);
        let candidates = form.class_candidates(cx);
        vec![
            self.render_requirement_list(form, &document, &candidates, window, cx),
            self.render_requirement_elements(form, &document, &candidates, window, cx),
            self.render_requirement_relationships(&document, window, cx),
        ]
    }

    fn render_requirement_list(
        &self,
        form: &RequirementForm,
        document: &RequirementDocument,
        candidates: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let last = form.requirements.len().saturating_sub(1);
        let mut section = v_flex()
            .w_full()
            .gap_2()
            .child(section_title(i18n::t!("Requirements")));
        for index in 0..form.requirements.len() {
            let (Some(row), Some(requirement)) = (
                form.requirements.get(index),
                document.requirements.get(index),
            ) else {
                continue;
            };
            let kind = requirement.kind;
            let kind_menu = self.render_requirement_choice_menu(
                ("mermaid-requirement-kind", index),
                kind.label(),
                RequirementKind::ALL
                    .into_iter()
                    .map(|choice| (choice.label(), choice == kind, choice))
                    .collect(),
                index,
                |requirement, choice| requirement.kind = choice,
                window,
                cx,
            );
            let risk = requirement.risk;
            let risk_menu = self.render_requirement_choice_menu(
                ("mermaid-requirement-risk", index),
                risk_label(risk),
                [None]
                    .into_iter()
                    .chain(RequirementRisk::ALL.map(Some))
                    .map(|choice| (risk_label(choice), choice == risk, choice))
                    .collect(),
                index,
                |requirement, choice| requirement.risk = choice,
                window,
                cx,
            );
            let method = requirement.verify_method;
            let method_menu = self.render_requirement_choice_menu(
                ("mermaid-requirement-verify", index),
                verify_method_label(method),
                [None]
                    .into_iter()
                    .chain(VerifyMethod::ALL.map(Some))
                    .map(|choice| (verify_method_label(choice), choice == method, choice))
                    .collect(),
                index,
                |requirement, choice| requirement.verify_method = choice,
                window,
                cx,
            );

            section = section.child(
                v_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .child(kind_menu)
                            .child(
                                field_frame(
                                    &row.name,
                                    ID_FIELD_WIDTH,
                                    is_valid_requirement_name(&requirement.name),
                                    window,
                                    cx,
                                )
                                .child(row.name.clone()),
                            )
                            .child(
                                field_frame(
                                    &row.id,
                                    NUMBER_FIELD_WIDTH,
                                    is_valid_requirement_value(&requirement.id.value),
                                    window,
                                    cx,
                                )
                                .child(row.id.clone()),
                            )
                            .child(self.render_requirement_class_menu(
                                BoxList::Requirements,
                                index,
                                &requirement.classes,
                                candidates,
                                window,
                                cx,
                            ))
                            .child(div().flex_1())
                            .child(self.render_requirement_box_controls(
                                BoxList::Requirements,
                                index,
                                last,
                                document.references_to(&requirement.name) > 0,
                                cx,
                            )),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .pl_4()
                            .child(
                                field_frame(
                                    &row.text,
                                    LABEL_FIELD_WIDTH,
                                    is_valid_requirement_value(&requirement.text.value),
                                    window,
                                    cx,
                                )
                                .flex_1()
                                .child(row.text.clone()),
                            )
                            .child(risk_menu)
                            .child(method_menu),
                    ),
            );
        }
        section
            .child(add_button(
                "mermaid-requirement-add",
                i18n::t!("Add Requirement"),
                cx.listener(|this, _, window, cx| this.add_requirement(window, cx)),
            ))
            .into_any_element()
    }

    fn render_requirement_elements(
        &self,
        form: &RequirementForm,
        document: &RequirementDocument,
        candidates: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let last = form.elements.len().saturating_sub(1);
        let mut section = v_flex().w_full().gap_2().child(section_title(i18n::t!(
            "Elements (what satisfies or verifies a requirement)"
        )));
        for index in 0..form.elements.len() {
            let (Some(row), Some(element)) =
                (form.elements.get(index), document.elements.get(index))
            else {
                continue;
            };
            section = section.child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        field_frame(
                            &row.name,
                            ID_FIELD_WIDTH,
                            is_valid_requirement_name(&element.name),
                            window,
                            cx,
                        )
                        .child(row.name.clone()),
                    )
                    .child(
                        field_frame(
                            &row.element_type,
                            ID_FIELD_WIDTH,
                            is_valid_requirement_value(&element.element_type.value),
                            window,
                            cx,
                        )
                        .child(row.element_type.clone()),
                    )
                    .child(
                        field_frame(
                            &row.doc_ref,
                            LABEL_FIELD_WIDTH,
                            is_valid_requirement_value(&element.doc_ref.value),
                            window,
                            cx,
                        )
                        .flex_1()
                        .child(row.doc_ref.clone()),
                    )
                    .child(self.render_requirement_class_menu(
                        BoxList::Elements,
                        index,
                        &element.classes,
                        candidates,
                        window,
                        cx,
                    ))
                    .child(self.render_requirement_box_controls(
                        BoxList::Elements,
                        index,
                        last,
                        document.references_to(&element.name) > 0,
                        cx,
                    )),
            );
        }
        section
            .child(add_button(
                "mermaid-requirement-add-element",
                i18n::t!("Add Element"),
                cx.listener(|this, _, window, cx| this.add_requirement_element(window, cx)),
            ))
            .child(render_class_input(
                &form.class_input,
                is_valid_requirement_class_name,
                window,
                cx,
            ))
            .into_any_element()
    }

    fn render_requirement_box_controls(
        &self,
        list: BoxList,
        index: usize,
        last: usize,
        is_referenced: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let prefix = match list {
            BoxList::Requirements => "mermaid-requirement-box",
            BoxList::Elements => "mermaid-requirement-element",
        };
        let move_up = cx.listener(move |this, _, _window, cx| {
            this.move_requirement_box(list, index, false, cx)
        });
        let move_down = cx
            .listener(move |this, _, _window, cx| this.move_requirement_box(list, index, true, cx));
        let remove =
            cx.listener(move |this, _, _window, cx| this.remove_requirement_box(list, index, cx));
        if is_referenced {
            row_controls_labelled(
                prefix,
                index,
                index == 0,
                index >= last,
                i18n::t!("Delete (also removes the relationships and styles that use this name)"),
                move_up,
                move_down,
                remove,
            )
            .into_any_element()
        } else {
            row_controls(
                prefix,
                index,
                index == 0,
                index >= last,
                move_up,
                move_down,
                remove,
            )
            .into_any_element()
        }
    }

    fn render_requirement_class_menu(
        &self,
        list: BoxList,
        index: usize,
        assigned: &[String],
        candidates: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let on_toggle: ToggleClass = std::rc::Rc::new(move |class, _window, cx| {
            this.update(cx, |this, cx| {
                this.toggle_requirement_box_class(list, index, &class, cx)
            })
            .log_err();
        });
        let id = match list {
            BoxList::Requirements => ("mermaid-requirement-class", index),
            BoxList::Elements => ("mermaid-requirement-element-class", index),
        };
        render_class_menu(id, assigned, candidates, on_toggle, window, cx)
    }

    /// A dropdown over `choices` — each a label, whether it is the current
    /// one, and the value — whose pick `apply` writes into the requirement at
    /// `index`.
    #[allow(clippy::too_many_arguments)]
    fn render_requirement_choice_menu<T: Copy + 'static>(
        &self,
        id: (&'static str, usize),
        current: &'static str,
        choices: Vec<(&'static str, bool, T)>,
        index: usize,
        apply: fn(&mut Requirement, T),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for (label, selected, value) in choices.iter().copied() {
                let this = this.clone();
                menu = menu.toggleable_entry(
                    label,
                    selected,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        this.update(cx, |this, cx| {
                            this.update_requirement(index, cx, |requirement| {
                                apply(requirement, value)
                            })
                        })
                        .log_err();
                    },
                );
            }
            menu
        });
        DropdownMenu::new(id, current, menu)
            .style(DropdownStyle::Outlined)
            .trigger_size(ButtonSize::Compact)
            .into_any_element()
    }

    fn render_requirement_relationships(
        &self,
        document: &RequirementDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let names = document.names();
        let last = document.relationships.len().saturating_sub(1);
        let mut section = v_flex()
            .w_full()
            .gap_1()
            .child(section_title(i18n::t!("Relationships")));
        for (index, relationship) in document.relationships.iter().enumerate() {
            section = section.child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(self.render_requirement_end_menu(
                        index,
                        true,
                        &relationship.source,
                        &names,
                        window,
                        cx,
                    ))
                    .child(self.render_relationship_kind_menu(index, relationship.kind, window, cx))
                    .child(Label::new("→").size(LabelSize::Small).color(Color::Muted))
                    .child(self.render_requirement_end_menu(
                        index,
                        false,
                        &relationship.target,
                        &names,
                        window,
                        cx,
                    ))
                    .child(div().flex_1())
                    .child(row_controls(
                        "mermaid-requirement-relationship",
                        index,
                        index == 0,
                        index >= last,
                        cx.listener(move |this, _, _window, cx| {
                            this.move_requirement_relationship(index, false, cx)
                        }),
                        cx.listener(move |this, _, _window, cx| {
                            this.move_requirement_relationship(index, true, cx)
                        }),
                        cx.listener(move |this, _, _window, cx| {
                            this.remove_requirement_relationship(index, cx)
                        }),
                    )),
            );
        }
        section
            .child(add_button(
                "mermaid-requirement-add-relationship",
                i18n::t!("Add Relationship"),
                cx.listener(|this, _, _window, cx| this.add_requirement_relationship(cx)),
            ))
            .into_any_element()
    }

    fn render_requirement_end_menu(
        &self,
        index: usize,
        is_source: bool,
        current: &str,
        names: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let selected = current.to_string();
        let choices = names.to_vec();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for name in choices.clone() {
                let this = this.clone();
                let is_selected = name == selected;
                menu = menu.toggleable_entry(
                    name.clone(),
                    is_selected,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        let name = name.clone();
                        this.update(cx, |this, cx| {
                            this.update_requirement_relationship(index, cx, move |relationship| {
                                if is_source {
                                    relationship.source = name;
                                } else {
                                    relationship.target = name;
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
            (
                "mermaid-requirement-end",
                index * 2 + usize::from(is_source),
            ),
            SharedString::from(current.to_string()),
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
    }

    fn render_relationship_kind_menu(
        &self,
        index: usize,
        current: RelationshipKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for kind in RelationshipKind::ALL {
                let this = this.clone();
                menu = menu.toggleable_entry(
                    kind.label(),
                    kind == current,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        this.update(cx, |this, cx| {
                            this.update_requirement_relationship(index, cx, move |relationship| {
                                relationship.kind = kind;
                            });
                        })
                        .log_err();
                    },
                );
            }
            menu
        });
        DropdownMenu::new(
            ("mermaid-requirement-relationship-kind", index),
            current.label(),
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
    }

    pub(super) fn render_requirement_toolbar(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(form) = self.requirement() else {
            return div().into_any_element();
        };
        let current = form.document.direction;
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for choice in [None]
                .into_iter()
                .chain(RequirementDirection::ALL.map(Some))
            {
                let this = this.clone();
                menu = menu.toggleable_entry(
                    direction_label(choice),
                    choice == current,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        this.update(cx, |this, cx| this.set_requirement_direction(choice, cx))
                            .log_err();
                    },
                );
            }
            menu
        });

        h_flex()
            .gap_2()
            .child(
                Label::new(i18n::t!("Direction"))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                DropdownMenu::new(
                    "mermaid-requirement-direction",
                    direction_label(current),
                    menu,
                )
                .style(DropdownStyle::Outlined)
                .trigger_size(ButtonSize::Compact),
            )
            .into_any_element()
    }
}

fn risk_label(risk: Option<RequirementRisk>) -> &'static str {
    risk.map_or(i18n::t!("Risk: Not Set"), RequirementRisk::label)
}

fn verify_method_label(method: Option<VerifyMethod>) -> &'static str {
    method.map_or(i18n::t!("Verification: Not Set"), VerifyMethod::label)
}

fn direction_label(direction: Option<RequirementDirection>) -> &'static str {
    match direction {
        Some(direction) => direction.label(),
        None => i18n::t!("Not Set"),
    }
}

#[cfg(test)]
#[path = "requirement_tests.rs"]
mod requirement_tests;
