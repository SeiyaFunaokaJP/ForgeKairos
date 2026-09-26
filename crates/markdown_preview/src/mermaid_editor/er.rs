//! The entity relationship half of the Mermaid editor's form.
//!
//! An ER diagram is two tables: the entities, each with its own attribute
//! table, and the relationships between them. A relationship names its ends by
//! entity name — an alias cannot be written there — so both ends are picked
//! from a menu rather than typed, and renaming an entity carries into every
//! relationship that named it.

use std::ops::Range;

use editor::Editor;
use gpui::{AnyElement, App, Context, Entity, Window};
use mermaid_render::er_model::{
    ErAttribute, ErAttributeKey, ErCardinality, ErDirection, ErDocument, ErEntity, ErRelationship,
    is_valid_attribute_word, is_valid_class_name, is_valid_entity_name,
};
use ui::{
    Checkbox, ContextMenu, DropdownMenu, DropdownStyle, IconButtonShape, IconPosition, ToggleState,
    Tooltip, prelude::*,
};
use util::ResultExt as _;

use super::class_menu::{
    ToggleClass, class_candidates, declared_class_names, render_class_input, render_class_menu,
};
use super::{
    Form, ID_FIELD_WIDTH, LABEL_FIELD_WIDTH, MermaidEditor, add_button, field_frame, neighbour,
    row_controls, section_title,
};

pub(super) struct ErForm {
    pub(super) document: ErDocument,
    pub(super) entities: Vec<EntityRow>,
    pub(super) relationships: Vec<RelationshipRow>,
    /// A class name typed by hand, offered beside the `classDef` names in
    /// every entity's class menu.
    pub(super) class_input: Entity<Editor>,
}

pub(super) struct EntityRow {
    pub(super) name: Entity<Editor>,
    pub(super) alias: Entity<Editor>,
    pub(super) attributes: Vec<AttributeRow>,
}

pub(super) struct AttributeRow {
    pub(super) type_name: Entity<Editor>,
    pub(super) name: Entity<Editor>,
    pub(super) comment: Entity<Editor>,
}

pub(super) struct RelationshipRow {
    pub(super) label: Entity<Editor>,
}

impl ErForm {
    pub(super) fn fields(&self) -> Vec<Entity<Editor>> {
        let entities = self.entities.iter().flat_map(|row| {
            [row.name.clone(), row.alias.clone()]
                .into_iter()
                .chain(row.attributes.iter().flat_map(|attribute| {
                    [
                        attribute.type_name.clone(),
                        attribute.name.clone(),
                        attribute.comment.clone(),
                    ]
                }))
        });
        entities
            .chain([self.class_input.clone()])
            .chain(self.relationships.iter().map(|row| row.label.clone()))
            .collect()
    }

    /// The class names the entity class menus offer.
    pub(super) fn class_candidates(&self, cx: &App) -> Vec<String> {
        let declared = declared_class_names(
            self.document
                .preserved_lines
                .iter()
                .map(|line| line.text.as_str()),
            is_valid_class_name,
        );
        class_candidates(
            declared,
            self.document
                .entities
                .iter()
                .flat_map(|entity| &entity.classes),
            &self.class_input.read(cx).text(cx),
            is_valid_class_name,
        )
    }

    /// The diagram the form currently describes, with every relationship
    /// pointed at the entity names as they are now spelled.
    pub(super) fn current_document(&self, cx: &App) -> ErDocument {
        let mut document = self.document.clone();
        let mut renames: Vec<(String, String)> = Vec::new();
        for (entity, row) in document.entities.iter_mut().zip(&self.entities) {
            let name = row.name.read(cx).text(cx).trim().to_string();
            if name != entity.name {
                renames.push((std::mem::replace(&mut entity.name, name.clone()), name));
            }
            let alias = row.alias.read(cx).text(cx);
            entity.alias = (!alias.is_empty()).then_some(alias);
            for (attribute, field) in entity.attributes.iter_mut().zip(&row.attributes) {
                attribute.type_name = field.type_name.read(cx).text(cx).trim().to_string();
                attribute.name = field.name.read(cx).text(cx).trim().to_string();
                let comment = field.comment.read(cx).text(cx);
                attribute.comment = (!comment.is_empty()).then_some(comment);
            }
        }
        for (relationship, row) in document.relationships.iter_mut().zip(&self.relationships) {
            relationship.label = row.label.read(cx).text(cx);
        }
        if !renames.is_empty() {
            for relationship in &mut document.relationships {
                for end in [&mut relationship.left, &mut relationship.right] {
                    if let Some((_, new)) = renames.iter().find(|(old, _)| old == end) {
                        *end = new.clone();
                    }
                }
            }
            for line in &mut document.preserved_lines {
                retarget_preserved(&mut line.text, &renames);
            }
        }
        document
    }
}

/// Where in `line` the entities a statement the model keeps verbatim addresses
/// are written.
///
/// Only `style` names entities, and it takes a comma separated list — Mermaid
/// lexes the names across the commas and the spaces around them. `classDef`
/// names a class and the accessibility statements name nothing, so neither is
/// answered for. The span is returned rather than the names so that a rename
/// can be written into the line without disturbing the rest of it.
fn preserved_target_span(line: &str) -> Option<Range<usize>> {
    let indent = line.len() - line.trim_start().len();
    let trimmed = line.get(indent..)?;
    let keyword = trimmed.split_whitespace().next()?;
    if !keyword.eq_ignore_ascii_case("style") {
        return None;
    }
    let after_keyword = trimmed.get(keyword.len()..)?;
    let leading = after_keyword.len() - after_keyword.trim_start().len();
    let start = indent + keyword.len() + leading;
    let length = id_list_length(after_keyword.trim_start());
    (length > 0).then_some(start..start + length)
}

/// How much of `rest` is the comma separated list of names it opens with: a
/// name, then a comma and the name after it for as long as one follows. The
/// whitespace after the last name is left out, so the span covers the list and
/// nothing else.
fn id_list_length(rest: &str) -> usize {
    let mut end = 0;
    let mut cursor = 0;
    while let Some(remainder) = rest.get(cursor..) {
        let name_length = remainder
            .find(|character: char| character.is_whitespace() || character == ',')
            .unwrap_or(remainder.len());
        if name_length == 0 {
            break;
        }
        cursor += name_length;
        end = cursor;
        let after_name = rest.get(cursor..).unwrap_or_default();
        let spaces = after_name.len() - after_name.trim_start().len();
        let Some(after_comma) = after_name.trim_start().strip_prefix(',') else {
            break;
        };
        cursor += spaces + 1 + after_comma.len() - after_comma.trim_start().len();
    }
    end
}

/// Points a kept statement at the new spelling of every entity it addresses,
/// one entry of its list at a time so that the separators and the spaces around
/// them stay as they were written.
fn retarget_preserved(line: &mut String, renames: &[(String, String)]) {
    let Some(span) = preserved_target_span(line) else {
        return;
    };
    let Some((rewritten, changed)) = line
        .get(span.clone())
        .map(|list| rewrite_name_list(list, renames))
    else {
        return;
    };
    if changed {
        line.replace_range(span, &rewritten);
    }
}

/// `list` with every name a rename names replaced, and whether any was.
fn rewrite_name_list(list: &str, renames: &[(String, String)]) -> (String, bool) {
    let mut rewritten = String::with_capacity(list.len());
    let mut changed = false;
    for (index, entry) in list.split(',').enumerate() {
        if index > 0 {
            rewritten.push(',');
        }
        let name = entry.trim();
        // A whole entry, so that renaming `A` leaves `AB` alone.
        match renames.iter().find(|(old, _)| old == name) {
            Some((_, new)) => {
                let leading = entry.len() - entry.trim_start().len();
                rewritten.push_str(entry.get(..leading).unwrap_or_default());
                rewritten.push_str(new);
                rewritten.push_str(entry.get(leading + name.len()..).unwrap_or_default());
                changed = true;
            }
            None => rewritten.push_str(entry),
        }
    }
    (rewritten, changed)
}

/// The entities a kept statement addresses once `name` is taken out of its
/// list, or `None` when `line` is not such a statement or does not address
/// `name` at all. An empty result means the statement has nothing left to be
/// about and goes with the entity.
fn preserved_without(line: &str, name: &str) -> Option<(Range<usize>, String)> {
    let span = preserved_target_span(line)?;
    let list = line.get(span.clone())?;
    if !list.split(',').any(|entry| entry.trim() == name) {
        return None;
    }
    let kept = list
        .split(',')
        .filter(|entry| entry.trim() != name)
        .collect::<Vec<_>>()
        .join(",");
    Some((span, kept.trim().to_string()))
}

/// The first thing wrong with what the user typed. The rest — a relationship
/// naming an entity that is not there, a duplicate entity — comes from
/// [`ErDocument::structure_error`].
pub(super) fn problem(document: &ErDocument) -> Option<SharedString> {
    // `erDiagram` on its own is not a diagram Mermaid draws, so an ER chosen
    // from the kind picker and confirmed straight away would leave a fence the
    // preview can only report an error about.
    if document.entities.is_empty() {
        return Some("エンティティを 1 つ以上追加してください。".into());
    }
    for entity in &document.entities {
        if !is_valid_entity_name(&entity.name) {
            return Some("名前が空のエンティティがあります。".into());
        }
        for attribute in &entity.attributes {
            for word in [&attribute.type_name, &attribute.name] {
                if !is_valid_attribute_word(word) {
                    return Some(
                        format!(
                            "「{word}」は属性の型や名前として使えません (空白・記号と PK / FK / UK は不可)。"
                        )
                        .into(),
                    );
                }
            }
        }
    }
    None
}

impl MermaidEditor {
    pub(super) fn new_er_form(
        &mut self,
        document: ErDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ErForm {
        let mut entities = Vec::with_capacity(document.entities.len());
        for entity in &document.entities {
            let (name, alias) = (
                entity.name.clone(),
                entity.alias.clone().unwrap_or_default(),
            );
            let mut attributes = Vec::with_capacity(entity.attributes.len());
            for attribute in &entity.attributes {
                let (type_name, attribute_name, comment) = (
                    attribute.type_name.clone(),
                    attribute.name.clone(),
                    attribute.comment.clone().unwrap_or_default(),
                );
                attributes.push(AttributeRow {
                    type_name: self.new_field(&type_name, window, cx),
                    name: self.new_field(&attribute_name, window, cx),
                    comment: self.new_field(&comment, window, cx),
                });
            }
            entities.push(EntityRow {
                name: self.new_field(&name, window, cx),
                alias: self.new_field(&alias, window, cx),
                attributes,
            });
        }
        let mut relationships = Vec::with_capacity(document.relationships.len());
        for relationship in &document.relationships {
            let label = relationship.label.clone();
            relationships.push(RelationshipRow {
                label: self.new_field(&label, window, cx),
            });
        }
        ErForm {
            document,
            entities,
            relationships,
            class_input: self.new_field("", window, cx),
        }
    }

    pub(super) fn er(&self) -> Option<&ErForm> {
        match &self.form {
            Form::Er(form) => Some(form),
            _ => None,
        }
    }

    pub(super) fn er_mut(&mut self) -> Option<&mut ErForm> {
        match &mut self.form {
            Form::Er(form) => Some(form),
            _ => None,
        }
    }

    pub(super) fn add_entity(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.er() else {
            return;
        };
        let name = form.current_document(cx).unused_entity_name("エンティティ");
        let row = EntityRow {
            name: self.new_field(&name, window, cx),
            alias: self.new_field("", window, cx),
            attributes: Vec::new(),
        };
        let Some(form) = self.er_mut() else {
            return;
        };
        form.document.entities.push(ErEntity {
            name,
            alias: None,
            attributes: Vec::new(),
            classes: Vec::new(),
            trailing_comment: None,
        });
        form.entities.push(row);
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn remove_entity(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(form) = self.er_mut() else {
            return;
        };
        if index >= form.entities.len() || index >= form.document.entities.len() {
            return;
        }
        let removed = form.document.entities.remove(index).name;
        form.entities.remove(index);
        // A relationship left pointing at the entity would declare it again,
        // bringing an empty box back where the entity was deleted.
        let orphaned = form
            .document
            .relationships
            .iter()
            .enumerate()
            .filter(|(_, relationship)| {
                relationship.left == removed || relationship.right == removed
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        for index in orphaned.into_iter().rev() {
            form.document.relationships.remove(index);
            if index < form.relationships.len() {
                form.relationships.remove(index);
            }
        }
        // A `style` kept verbatim would go on addressing a name that is no
        // longer declared; one that styles other entities too keeps its place
        // and loses this name from its list.
        form.document.preserved_lines.retain_mut(|line| {
            match preserved_without(&line.text, &removed) {
                Some((_, kept)) if kept.is_empty() => false,
                Some((span, kept)) => {
                    line.text.replace_range(span, &kept);
                    true
                }
                None => true,
            }
        });
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn move_entity(&mut self, index: usize, forward: bool, cx: &mut Context<Self>) {
        let Some(form) = self.er_mut() else {
            return;
        };
        let Some(other) = neighbour(index, forward, form.entities.len()) else {
            return;
        };
        if other >= form.document.entities.len() || index >= form.document.entities.len() {
            return;
        }
        form.document.entities.swap(index, other);
        form.entities.swap(index, other);
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn add_attribute(
        &mut self,
        entity: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let row = AttributeRow {
            type_name: self.new_field("string", window, cx),
            name: self.new_field("", window, cx),
            comment: self.new_field("", window, cx),
        };
        let Some(form) = self.er_mut() else {
            return;
        };
        let (Some(entity_row), Some(entity_model)) = (
            form.entities.get_mut(entity),
            form.document.entities.get_mut(entity),
        ) else {
            return;
        };
        entity_model.attributes.push(ErAttribute {
            type_name: "string".to_string(),
            name: String::new(),
            keys: Vec::new(),
            comment: None,
            trailing_comment: None,
        });
        entity_row.attributes.push(row);
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn remove_attribute(&mut self, entity: usize, index: usize, cx: &mut Context<Self>) {
        let Some(form) = self.er_mut() else {
            return;
        };
        let (Some(entity_row), Some(entity_model)) = (
            form.entities.get_mut(entity),
            form.document.entities.get_mut(entity),
        ) else {
            return;
        };
        if index >= entity_row.attributes.len() || index >= entity_model.attributes.len() {
            return;
        }
        entity_model.attributes.remove(index);
        entity_row.attributes.remove(index);
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn move_attribute(
        &mut self,
        entity: usize,
        index: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.er_mut() else {
            return;
        };
        let (Some(entity_row), Some(entity_model)) = (
            form.entities.get_mut(entity),
            form.document.entities.get_mut(entity),
        ) else {
            return;
        };
        let Some(other) = neighbour(index, forward, entity_model.attributes.len()) else {
            return;
        };
        if other >= entity_row.attributes.len() || index >= entity_row.attributes.len() {
            return;
        }
        entity_model.attributes.swap(index, other);
        entity_row.attributes.swap(index, other);
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn toggle_attribute_key(
        &mut self,
        entity: usize,
        index: usize,
        key: ErAttributeKey,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.er_mut() else {
            return;
        };
        let Some(attribute) = form
            .document
            .entities
            .get_mut(entity)
            .and_then(|entity| entity.attributes.get_mut(index))
        else {
            return;
        };
        match attribute.keys.iter().position(|held| *held == key) {
            Some(position) => {
                attribute.keys.remove(position);
            }
            None => attribute.keys.push(key),
        }
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn add_relationship(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.er() else {
            return;
        };
        let document = form.current_document(cx);
        let Some(left) = document.entities.first().map(|entity| entity.name.clone()) else {
            self.message = Some("先にエンティティを追加してください。".into());
            cx.notify();
            return;
        };
        let right = document
            .entities
            .get(1)
            .map(|entity| entity.name.clone())
            .unwrap_or_else(|| left.clone());
        let row = RelationshipRow {
            label: self.new_field("", window, cx),
        };
        let Some(form) = self.er_mut() else {
            return;
        };
        form.document.relationships.push(ErRelationship {
            left,
            right,
            left_cardinality: ErCardinality::ExactlyOne,
            right_cardinality: ErCardinality::ZeroOrMore,
            identifying: true,
            label: String::new(),
            trailing_comment: None,
        });
        form.relationships.push(row);
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn remove_relationship(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(form) = self.er_mut() else {
            return;
        };
        if index >= form.relationships.len() || index >= form.document.relationships.len() {
            return;
        }
        form.document.relationships.remove(index);
        form.relationships.remove(index);
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn move_relationship(
        &mut self,
        index: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.er_mut() else {
            return;
        };
        let Some(other) = neighbour(index, forward, form.relationships.len()) else {
            return;
        };
        if other >= form.document.relationships.len() || index >= form.document.relationships.len()
        {
            return;
        }
        form.document.relationships.swap(index, other);
        form.relationships.swap(index, other);
        self.schedule_preview(cx);
        cx.notify();
    }

    /// Adds `class` to the entity at `index`, or takes it away when the entity
    /// already has it. The model writes the classes inline (`A:::warn`), so a
    /// `class` statement the diagram was written with goes the same way.
    pub(super) fn toggle_entity_class(
        &mut self,
        index: usize,
        class: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.er_mut() else {
            return;
        };
        let Some(entity) = form.document.entities.get_mut(index) else {
            return;
        };
        match entity.classes.iter().position(|name| name == class) {
            Some(position) => {
                entity.classes.remove(position);
            }
            None => entity.classes.push(class.to_string()),
        }
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn update_relationship(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut ErRelationship),
    ) {
        let Some(form) = self.er_mut() else {
            return;
        };
        let Some(relationship) = form.document.relationships.get_mut(index) else {
            return;
        };
        edit(relationship);
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn set_er_direction(
        &mut self,
        direction: Option<ErDirection>,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.er_mut() else {
            return;
        };
        form.document.direction = direction;
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn render_er_toolbar(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(form) = self.er() else {
            return div().into_any_element();
        };
        let current = form.document.direction;
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for choice in [None].into_iter().chain(ErDirection::ALL.map(Some)) {
                let this = this.clone();
                menu = menu.toggleable_entry(
                    direction_label(choice),
                    choice == current,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        this.update(cx, |this, cx| this.set_er_direction(choice, cx))
                            .log_err();
                    },
                );
            }
            menu
        });

        h_flex()
            .gap_2()
            .child(
                Label::new("方向")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                DropdownMenu::new("mermaid-er-direction", direction_label(current), menu)
                    .style(DropdownStyle::Outlined)
                    .trigger_size(ButtonSize::Compact),
            )
            .into_any_element()
    }

    pub(super) fn render_er(&self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(form) = self.er() else {
            return Vec::new();
        };
        let names = form
            .entities
            .iter()
            .map(|row| row.name.read(cx).text(cx).trim().to_string())
            .collect::<Vec<_>>();
        vec![
            self.render_entities(form, window, cx),
            self.render_relationships(form, &names, window, cx),
        ]
    }

    pub(super) fn render_entities(
        &self,
        form: &ErForm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let last = form.entities.len().saturating_sub(1);
        let candidates = form.class_candidates(cx);
        let this = cx.entity().downgrade();
        let mut section = v_flex()
            .w_full()
            .gap_2()
            .child(section_title("エンティティ"));
        for index in 0..form.entities.len() {
            let (Some(row), Some(entity)) =
                (form.entities.get(index), form.document.entities.get(index))
            else {
                continue;
            };
            let mut attributes = v_flex().w_full().gap_1().pl_4();
            for attribute_index in 0..row.attributes.len() {
                let (Some(attribute_row), Some(attribute)) = (
                    row.attributes.get(attribute_index),
                    entity.attributes.get(attribute_index),
                ) else {
                    continue;
                };
                let mut keys = h_flex().flex_none().gap_x_1();
                for key in ErAttributeKey::ALL {
                    let checked = attribute.keys.contains(&key);
                    keys = keys.child(
                        Checkbox::new(
                            SharedString::from(format!(
                                "mermaid-er-key-{index}-{attribute_index}-{}",
                                key.keyword()
                            )),
                            checked.into(),
                        )
                        .label(key.keyword())
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(
                            move |this, _: &ToggleState, _window, cx| {
                                this.toggle_attribute_key(index, attribute_index, key, cx);
                            },
                        )),
                    );
                }
                attributes = attributes.child(
                    h_flex()
                        .w_full()
                        .gap_1()
                        .child(
                            field_frame(
                                &attribute_row.type_name,
                                ID_FIELD_WIDTH,
                                is_valid_attribute_word(
                                    attribute_row.type_name.read(cx).text(cx).trim(),
                                ),
                                window,
                                cx,
                            )
                            .child(attribute_row.type_name.clone()),
                        )
                        .child(
                            field_frame(
                                &attribute_row.name,
                                ID_FIELD_WIDTH,
                                is_valid_attribute_word(
                                    attribute_row.name.read(cx).text(cx).trim(),
                                ),
                                window,
                                cx,
                            )
                            .child(attribute_row.name.clone()),
                        )
                        .child(keys)
                        .child(
                            field_frame(
                                &attribute_row.comment,
                                LABEL_FIELD_WIDTH,
                                true,
                                window,
                                cx,
                            )
                            .flex_1()
                            .child(attribute_row.comment.clone()),
                        )
                        .child(row_controls(
                            &format!("mermaid-er-attribute-{index}"),
                            attribute_index,
                            attribute_index == 0,
                            attribute_index + 1 >= row.attributes.len(),
                            cx.listener(move |this, _, _window, cx| {
                                this.move_attribute(index, attribute_index, false, cx)
                            }),
                            cx.listener(move |this, _, _window, cx| {
                                this.move_attribute(index, attribute_index, true, cx)
                            }),
                            cx.listener(move |this, _, _window, cx| {
                                this.remove_attribute(index, attribute_index, cx)
                            }),
                        )),
                );
            }

            section = section.child(
                v_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .child(
                                field_frame(
                                    &row.name,
                                    ID_FIELD_WIDTH,
                                    is_valid_entity_name(row.name.read(cx).text(cx).trim()),
                                    window,
                                    cx,
                                )
                                .child(row.name.clone()),
                            )
                            .child(
                                field_frame(&row.alias, LABEL_FIELD_WIDTH, true, window, cx)
                                    .flex_1()
                                    .child(row.alias.clone()),
                            )
                            .child({
                                let this = this.clone();
                                let on_toggle: ToggleClass =
                                    std::rc::Rc::new(move |class, _window, cx| {
                                        this.update(cx, |this, cx| {
                                            this.toggle_entity_class(index, &class, cx)
                                        })
                                        .log_err();
                                    });
                                render_class_menu(
                                    ("mermaid-er-entity-class", index),
                                    &entity.classes,
                                    &candidates,
                                    on_toggle,
                                    window,
                                    cx,
                                )
                            })
                            .child(row_controls(
                                "mermaid-er-entity",
                                index,
                                index == 0,
                                index >= last,
                                cx.listener(move |this, _, _window, cx| {
                                    this.move_entity(index, false, cx)
                                }),
                                cx.listener(move |this, _, _window, cx| {
                                    this.move_entity(index, true, cx)
                                }),
                                cx.listener(move |this, _, _window, cx| {
                                    this.remove_entity(index, cx)
                                }),
                            )),
                    )
                    .child(attributes)
                    .child(div().pl_4().child(add_button(
                        SharedString::from(format!("mermaid-er-add-attribute-{index}")),
                        "属性を追加",
                        cx.listener(move |this, _, window, cx| {
                            this.add_attribute(index, window, cx)
                        }),
                    ))),
            );
        }
        section
            .child(add_button(
                "mermaid-er-add-entity",
                "エンティティを追加",
                cx.listener(|this, _, window, cx| this.add_entity(window, cx)),
            ))
            .child(render_class_input(
                &form.class_input,
                is_valid_class_name,
                window,
                cx,
            ))
            .into_any_element()
    }

    pub(super) fn render_relationships(
        &self,
        form: &ErForm,
        names: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let last = form.relationships.len().saturating_sub(1);
        let mut section = v_flex().w_full().gap_1().child(section_title("関係"));
        for index in 0..form.relationships.len() {
            let (Some(row), Some(relationship)) = (
                form.relationships.get(index),
                form.document.relationships.get(index),
            ) else {
                continue;
            };
            section = section.child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(self.render_entity_menu(
                        index,
                        true,
                        &relationship.left,
                        names,
                        window,
                        cx,
                    ))
                    .child(self.render_cardinality_menu(
                        index,
                        true,
                        relationship.left_cardinality,
                        window,
                        cx,
                    ))
                    .child(self.render_identifying_button(index, relationship.identifying, cx))
                    .child(self.render_cardinality_menu(
                        index,
                        false,
                        relationship.right_cardinality,
                        window,
                        cx,
                    ))
                    .child(self.render_entity_menu(
                        index,
                        false,
                        &relationship.right,
                        names,
                        window,
                        cx,
                    ))
                    .child(
                        field_frame(&row.label, LABEL_FIELD_WIDTH, true, window, cx)
                            .flex_1()
                            .child(row.label.clone()),
                    )
                    .child(row_controls(
                        "mermaid-er-relationship",
                        index,
                        index == 0,
                        index >= last,
                        cx.listener(move |this, _, _window, cx| {
                            this.move_relationship(index, false, cx)
                        }),
                        cx.listener(move |this, _, _window, cx| {
                            this.move_relationship(index, true, cx)
                        }),
                        cx.listener(move |this, _, _window, cx| {
                            this.remove_relationship(index, cx)
                        }),
                    )),
            );
        }
        section
            .child(add_button(
                "mermaid-er-add-relationship",
                "関係を追加",
                cx.listener(|this, _, window, cx| this.add_relationship(window, cx)),
            ))
            .into_any_element()
    }

    pub(super) fn render_entity_menu(
        &self,
        index: usize,
        is_left: bool,
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
                            this.update_relationship(index, cx, move |relationship| {
                                if is_left {
                                    relationship.left = name;
                                } else {
                                    relationship.right = name;
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
            ("mermaid-er-end", index * 2 + usize::from(is_left)),
            SharedString::from(current.to_string()),
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
    }

    pub(super) fn render_cardinality_menu(
        &self,
        index: usize,
        is_left: bool,
        current: ErCardinality,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            // A diagram may already spell a cardinality the menu does not
            // offer — Mermaid's markdown parent, which has no right hand
            // spelling — and picking another one must not be the only way out
            // of it.
            let mut offered = ErCardinality::ALL.to_vec();
            if !offered.contains(&current) {
                offered.push(current);
            }
            for cardinality in offered {
                let this = this.clone();
                menu = menu.toggleable_entry(
                    cardinality.label(),
                    cardinality == current,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        this.update(cx, |this, cx| {
                            this.update_relationship(index, cx, move |relationship| {
                                if is_left {
                                    relationship.left_cardinality = cardinality;
                                } else {
                                    relationship.right_cardinality = cardinality;
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
            ("mermaid-er-cardinality", index * 2 + usize::from(is_left)),
            current.label(),
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
    }

    /// The line between the two ends: solid for an identifying relationship,
    /// dashed for a non-identifying one.
    pub(super) fn render_identifying_button(
        &self,
        index: usize,
        identifying: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        IconButton::new(
            ("mermaid-er-identifying", index),
            if identifying {
                IconName::Dash
            } else {
                IconName::Ellipsis
            },
        )
        .shape(IconButtonShape::Square)
        .icon_size(IconSize::XSmall)
        .tooltip(Tooltip::text(if identifying {
            "実線 (強い関係)。押すと破線"
        } else {
            "破線 (弱い関係)。押すと実線"
        }))
        .on_click(cx.listener(move |this, _, _window, cx| {
            this.update_relationship(index, cx, |relationship| {
                relationship.identifying = !relationship.identifying;
            });
        }))
        .into_any_element()
    }
}

pub(super) fn direction_label(direction: Option<ErDirection>) -> &'static str {
    match direction {
        Some(direction) => direction.label(),
        None => "指定なし",
    }
}
