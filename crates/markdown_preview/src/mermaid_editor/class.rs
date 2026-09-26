//! The class diagram half of the Mermaid editor's form.
//!
//! Two things shape this form, and both are the same as the state diagram's.
//! First, a class diagram is not two lists — one of classes, one of relations —
//! even though it reads like one: a `%%` comment has to stay where it was
//! written, a class may be declared twice, and a `namespace` holds a list of its
//! own. So the statements are shown as one ordered list per scope, the way the
//! fence has them, and a namespace opens a nested list of the same shape.
//!
//! Second, a class does not have to be declared: `動物 <|-- 犬` names 犬 into
//! existence. There is therefore no one statement whose name could be edited to
//! rename it, and the names get a list of their own above the statements.
//! Editing one there carries into every statement that names it.
//!
//! Of the lines the model keeps verbatim, `classDef` and the accessibility
//! statements name no class. `style`, `cssClass`, `click`, `link` and
//! `callback` do, and a rename is carried into the names they address. Deleting
//! a class takes it out of them — the whole line, or the one entry of a
//! `cssClass` list — and so does a class that drops out of the diagram because
//! the last statement that named it went, but only for a name that was a class
//! when the editor opened: a line written for a class that was never there is
//! left as it was found. The `:::` of a `class` statement is carried on the
//! declaration itself and names a `classDef` rather than a class of the
//! diagram, so a rename leaves it alone.

use editor::Editor;
use gpui::{AnyElement, App, Context, Entity, Window};
use mermaid_render::class_model::{
    ClassDecl, ClassDocument, ClassItem, ClassNamespace, ClassNote, ClassRef, ClassRelation,
    RELATION_PRESETS, is_valid_annotation, is_valid_block_member_text, is_valid_class_name,
    is_valid_generic, is_valid_member_text, is_valid_quoted_text, is_valid_relation_arrow,
    is_valid_relation_label, rename_statement_targets, retain_statement_targets,
};
use mermaid_render::flowchart_model::FlowDirection;
use ui::{ContextMenu, DropdownMenu, DropdownStyle, IconPosition, prelude::*};
use util::ResultExt as _;

use super::{
    Form, ID_FIELD_WIDTH, LABEL_FIELD_WIDTH, MermaidEditor, NUMBER_FIELD_WIDTH, add_button,
    field_frame, neighbour, row_controls, row_controls_labelled, section_title,
};

/// How deeply the form draws namespaces. The model refuses to parse deeper than
/// this, so a diagram that reaches the limit cannot be open here.
const MAX_DEPTH: usize = 16;

pub(super) struct ClassForm {
    pub(super) document: ClassDocument,
    /// One row per class name the diagram mentions, in the order it first
    /// appears.
    pub(super) names: Vec<NameRow>,
    pub(super) body: ItemRows,
    /// The classes the diagram held when the editor opened it, under the names
    /// they have now. A kept `style` or `click` line whose class has left the
    /// diagram is only dropped when its class is one of these.
    pub(super) original_names: Vec<String>,
}

/// The name of one class and the field it is edited in. `name` is what the
/// document calls it now, so that a field that has moved away from it is a
/// rename.
pub(super) struct NameRow {
    pub(super) name: String,
    pub(super) field: Entity<Editor>,
}

/// The text fields of one scope, in the same order as its items.
#[derive(Default)]
pub(super) struct ItemRows {
    pub(super) items: Vec<ItemRow>,
}

pub(super) enum ItemRow {
    Class {
        generic: Entity<Editor>,
        label: Entity<Editor>,
        members: Vec<Entity<Editor>>,
    },
    Member {
        text: Entity<Editor>,
    },
    Annotation {
        annotation: Entity<Editor>,
    },
    Relation {
        /// The `~T~` written after the class at each end.
        left_generic: Entity<Editor>,
        right_generic: Entity<Editor>,
        left_cardinality: Entity<Editor>,
        right_cardinality: Entity<Editor>,
        label: Entity<Editor>,
    },
    Note {
        text: Entity<Editor>,
    },
    Namespace {
        name: Entity<Editor>,
        label: Entity<Editor>,
        body: ItemRows,
    },
    /// A line held verbatim: nothing to type into.
    Fixed,
}

impl ClassForm {
    pub(super) fn fields(&self) -> Vec<Entity<Editor>> {
        let mut fields = self
            .names
            .iter()
            .map(|row| row.field.clone())
            .collect::<Vec<_>>();
        collect_fields(&self.body, &mut fields);
        fields
    }

    /// The diagram as it will be written: [`Self::edited_document`], without
    /// the kept statements whose class has left the diagram.
    ///
    /// Those are only taken out here, on the way to the fence, because the
    /// document the form holds has to keep one item per row.
    pub(super) fn current_document(&self, cx: &App) -> ClassDocument {
        let renames = self.renames(cx);
        let mut document = self.edited_document(&renames, cx);
        let live = document.class_names();
        let original = self
            .original_names
            .iter()
            .map(|name| renamed_to(name, &renames))
            .collect::<Vec<_>>();
        drop_orphaned_statements(&mut document.items, &live, &original);
        document
    }

    /// The diagram the form currently describes: the structure held in
    /// `document` with the text fields' contents written over it, and every
    /// reference to a class in `renames` carried along.
    fn edited_document(&self, renames: &[(String, String)], cx: &App) -> ClassDocument {
        let mut document = self.document.clone();
        apply_fields(&mut document.items, &self.body, cx);
        if !renames.is_empty() {
            apply_renames(&mut document.items, renames);
        }
        document
    }

    /// The classes a statement added now can name: the renames typed so far
    /// that a class can take, and none of the ones still being typed, which
    /// would give the new statement a class of a name nothing else has.
    fn settled_class_names(&self, cx: &App) -> Vec<String> {
        let (renames, _pending) = self.settled_renames(cx);
        self.edited_document(&renames, cx).class_names()
    }

    /// [`Self::renames`], split into the ones to a name a class can have and
    /// the ones to a name it cannot have yet — a field cleared or half typed.
    fn settled_renames(&self, cx: &App) -> (Vec<(String, String)>, Vec<(String, String)>) {
        self.renames(cx)
            .into_iter()
            .partition(|(_, typed)| is_valid_class_name(typed))
    }

    /// Each class whose name field has moved away from its name, with the name
    /// typed there.
    fn renames(&self, cx: &App) -> Vec<(String, String)> {
        self.names
            .iter()
            .filter_map(|row| {
                let typed = row.field.read(cx).text(cx).trim().to_string();
                (typed != row.name).then(|| (row.name.clone(), typed))
            })
            .collect()
    }
}

/// The name `name` has after `renames`.
fn renamed_to(name: &str, renames: &[(String, String)]) -> String {
    renames
        .iter()
        .find(|(old, _)| old == name)
        .map(|(_, new)| new.clone())
        .unwrap_or_else(|| name.to_string())
}

/// Takes the classes that have left the diagram out of the kept statements
/// that address them, dropping a statement that is left addressing nothing.
///
/// A class that is not among `original` was never in the diagram as far as
/// the form knows, and a statement written for it is left alone: the user may
/// be about to add it, and deleting a line the form never showed is the one
/// change the user could not undo from inside it.
fn drop_orphaned_statements(items: &mut Vec<ClassItem>, live: &[String], original: &[String]) {
    items.retain_mut(|item| match item {
        ClassItem::Preserved(text) => {
            let kept = retain_statement_targets(text, |name| {
                live.iter().any(|live| live == name) || !original.iter().any(|gone| gone == name)
            });
            match kept {
                Some(kept) => {
                    *text = kept;
                    true
                }
                None => false,
            }
        }
        ClassItem::Namespace(namespace) => {
            drop_orphaned_statements(&mut namespace.items, live, original);
            true
        }
        _ => true,
    });
}

fn collect_fields(rows: &ItemRows, fields: &mut Vec<Entity<Editor>>) {
    for row in &rows.items {
        match row {
            ItemRow::Class {
                generic,
                label,
                members,
            } => {
                fields.push(generic.clone());
                fields.push(label.clone());
                fields.extend(members.iter().cloned());
            }
            ItemRow::Member { text } | ItemRow::Note { text } => fields.push(text.clone()),
            ItemRow::Annotation { annotation } => fields.push(annotation.clone()),
            ItemRow::Relation {
                left_generic,
                right_generic,
                left_cardinality,
                right_cardinality,
                label,
            } => {
                fields.push(left_generic.clone());
                fields.push(left_cardinality.clone());
                fields.push(right_cardinality.clone());
                fields.push(right_generic.clone());
                fields.push(label.clone());
            }
            ItemRow::Namespace { name, label, body } => {
                fields.push(name.clone());
                fields.push(label.clone());
                collect_fields(body, fields);
            }
            ItemRow::Fixed => {}
        }
    }
}

/// Writes the fields of `rows` onto `items`. Class names are not among them:
/// they are edited in the list above the statements.
fn apply_fields(items: &mut [ClassItem], rows: &ItemRows, cx: &App) {
    for (item, row) in items.iter_mut().zip(&rows.items) {
        match (item, row) {
            (
                ClassItem::Class(declaration),
                ItemRow::Class {
                    generic,
                    label,
                    members,
                },
            ) => {
                let generic = generic.read(cx).text(cx).trim().to_string();
                declaration.reference.generic = (!generic.is_empty()).then_some(generic);
                let label = label.read(cx).text(cx);
                declaration.label = (!label.is_empty()).then_some(label);
                if let Some(held) = &mut declaration.members {
                    for (member, field) in held.iter_mut().zip(members) {
                        *member = field.read(cx).text(cx).trim().to_string();
                    }
                }
            }
            (ClassItem::Member { text, .. }, ItemRow::Member { text: field }) => {
                *text = field.read(cx).text(cx).trim().to_string();
            }
            (
                ClassItem::Annotation { annotation, .. },
                ItemRow::Annotation { annotation: field },
            ) => {
                *annotation = field.read(cx).text(cx).trim().to_string();
            }
            (
                ClassItem::Relation(relation),
                ItemRow::Relation {
                    left_generic,
                    right_generic,
                    left_cardinality,
                    right_cardinality,
                    label,
                },
            ) => {
                for (end, field) in [
                    (&mut relation.left, left_generic),
                    (&mut relation.right, right_generic),
                ] {
                    let generic = field.read(cx).text(cx).trim().to_string();
                    end.generic = (!generic.is_empty()).then_some(generic);
                }
                let left = left_cardinality.read(cx).text(cx).trim().to_string();
                relation.left_cardinality = (!left.is_empty()).then_some(left);
                let right = right_cardinality.read(cx).text(cx).trim().to_string();
                relation.right_cardinality = (!right.is_empty()).then_some(right);
                // The label runs to the end of its line and comes back trimmed,
                // so a space the user cannot see would otherwise be a value the
                // model refuses to write and never explains.
                let label = label.read(cx).text(cx).trim().to_string();
                relation.label = (!label.is_empty()).then_some(label);
            }
            (ClassItem::Note(note), ItemRow::Note { text }) => {
                note.text = text.read(cx).text(cx);
            }
            (
                ClassItem::Namespace(namespace),
                ItemRow::Namespace {
                    name,
                    label,
                    body: rows,
                },
            ) => {
                namespace.name = name.read(cx).text(cx).trim().to_string();
                let label = label.read(cx).text(cx);
                namespace.label = (!label.is_empty()).then_some(label);
                apply_fields(&mut namespace.items, rows, cx);
            }
            _ => {}
        }
    }
}

/// Points everything that names a class at its new name.
///
/// A relation or a note left on the old name would name a class of its own,
/// which Mermaid draws as an extra, empty box.
fn apply_renames(items: &mut [ClassItem], renames: &[(String, String)]) {
    let renamed = |name: &str| {
        renames
            .iter()
            .find(|(old, _)| old == name)
            .map(|(_, new)| new.clone())
    };
    for item in items {
        match item {
            ClassItem::Class(declaration) => {
                if let Some(name) = renamed(&declaration.reference.name) {
                    declaration.reference.name = name;
                }
            }
            ClassItem::Member { reference, .. } | ClassItem::Annotation { reference, .. } => {
                if let Some(name) = renamed(&reference.name) {
                    reference.name = name;
                }
            }
            ClassItem::Relation(relation) => {
                for end in [&mut relation.left, &mut relation.right] {
                    if let Some(name) = renamed(&end.name) {
                        end.name = name;
                    }
                }
            }
            ClassItem::Note(note) => {
                if let Some(target) = &note.target
                    && let Some(name) = renamed(target)
                {
                    note.target = Some(name);
                }
            }
            ClassItem::Namespace(namespace) => apply_renames(&mut namespace.items, renames),
            ClassItem::Preserved(text) => *text = rename_statement_targets(text, &renamed),
        }
    }
}

/// The first thing wrong with what the user typed, in the words shown above the
/// form.
pub(super) fn problem(document: &ClassDocument) -> Option<SharedString> {
    items_problem(&document.items)
}

/// The classes a statement names, as it spells them.
///
/// [`ClassDocument::class_names`] answers with the classes the picture would
/// hold, which is not the same list: a field cleared to nothing leaves a
/// statement naming a class of no name, and that one has to be caught here
/// rather than quietly dropped, because writing it back leaves a fence neither
/// Mermaid nor this editor can read.
fn named_classes(item: &ClassItem) -> Vec<&str> {
    match item {
        ClassItem::Class(declaration) => vec![declaration.reference.name.as_str()],
        ClassItem::Member { reference, .. } | ClassItem::Annotation { reference, .. } => {
            vec![reference.name.as_str()]
        }
        ClassItem::Relation(relation) => {
            vec![relation.left.name.as_str(), relation.right.name.as_str()]
        }
        ClassItem::Note(note) => note.target.as_deref().into_iter().collect(),
        ClassItem::Namespace(_) | ClassItem::Preserved(_) => Vec::new(),
    }
}

fn items_problem(items: &[ClassItem]) -> Option<SharedString> {
    for item in items {
        for name in named_classes(item) {
            if name.is_empty() {
                return Some("名前が空のクラスがあります。".into());
            }
            if !is_valid_class_name(name) {
                return Some(
                    format!(
                        "クラス名「{name}」は使えません (空白・記号と Mermaid の予約語は不可)。"
                    )
                    .into(),
                );
            }
        }
        let problem = match item {
            ClassItem::Class(declaration) => class_problem(declaration),
            ClassItem::Member { text, .. } => (!is_valid_member_text(text)).then(|| {
                SharedString::from(format!(
                    "メンバー「{text}」は使えません (前後の空白と `}}` だけの行は不可)。"
                ))
            }),
            ClassItem::Annotation { annotation, .. } => {
                (!is_valid_annotation(annotation)).then(|| {
                    SharedString::from(format!(
                        "注釈「{annotation}」は使えません (`<`・`>`・`\"` と前後の空白は不可)。"
                    ))
                })
            }
            ClassItem::Relation(relation) => relation_problem(relation),
            ClassItem::Note(note) => (!is_valid_quoted_text(&note.text)).then(|| {
                SharedString::from("ノートの本文に `\"` と改行は使えません。".to_string())
            }),
            ClassItem::Namespace(namespace) => match is_valid_class_name(&namespace.name) {
                false => Some(match namespace.name.is_empty() {
                    true => SharedString::from("名前が空の `namespace` があります。"),
                    false => {
                        format!("`namespace` の名前「{}」は使えません。", namespace.name).into()
                    }
                }),
                true => namespace
                    .label
                    .as_ref()
                    .filter(|label| !is_valid_quoted_text(label))
                    .map(|_| SharedString::from("`namespace` の表示名に `\"` は使えません。"))
                    .or_else(|| items_problem(&namespace.items)),
            },
            ClassItem::Preserved(_) => None,
        };
        if problem.is_some() {
            return problem;
        }
    }
    None
}

fn class_problem(declaration: &ClassDecl) -> Option<SharedString> {
    if let Some(generic) = &declaration.reference.generic
        && !is_valid_generic(generic)
    {
        return Some(format!("総称型「{generic}」は使えません。").into());
    }
    if let Some(label) = &declaration.label
        && !is_valid_quoted_text(label)
    {
        return Some("クラスの表示名に `\"` と改行は使えません。".into());
    }
    declaration
        .members
        .iter()
        .flatten()
        .find(|member| !is_valid_block_member_text(member))
        .map(|member| {
            SharedString::from(format!(
                "メンバー「{member}」は使えません (`}}` で始まる行と前後の空白は不可)。"
            ))
        })
}

fn relation_problem(relation: &ClassRelation) -> Option<SharedString> {
    if !is_valid_relation_arrow(&relation.arrow) {
        return Some(format!("関係の記号「{}」は使えません。", relation.arrow).into());
    }
    for generic in [&relation.left.generic, &relation.right.generic]
        .into_iter()
        .flatten()
    {
        if !is_valid_generic(generic) {
            return Some(format!("総称型「{generic}」は使えません。").into());
        }
    }
    for cardinality in [&relation.left_cardinality, &relation.right_cardinality]
        .into_iter()
        .flatten()
    {
        if !is_valid_quoted_text(cardinality) {
            return Some("多重度に `\"` と改行は使えません。".into());
        }
    }
    relation
        .label
        .as_ref()
        .filter(|label| !is_valid_relation_label(label))
        .map(|_| SharedString::from("関係のラベルに改行と前後の空白は使えません。"))
}

/// The items a path addresses: each element steps into the namespace at that
/// index.
fn items_at<'a>(items: &'a mut Vec<ClassItem>, path: &[usize]) -> Option<&'a mut Vec<ClassItem>> {
    let Some((index, rest)) = path.split_first() else {
        return Some(items);
    };
    match items.get_mut(*index)? {
        ClassItem::Namespace(namespace) => items_at(&mut namespace.items, rest),
        _ => None,
    }
}

fn rows_at<'a>(rows: &'a mut ItemRows, path: &[usize]) -> Option<&'a mut ItemRows> {
    let Some((index, rest)) = path.split_first() else {
        return Some(rows);
    };
    match rows.items.get_mut(*index)? {
        ItemRow::Namespace { body, .. } => rows_at(body, rest),
        _ => None,
    }
}

/// Drops every statement that names `name`, so that a deleted class does not
/// come back as an empty box under one of them.
///
/// A kept `cssClass` line that also names other classes stays, with `name`
/// taken out of its list; [`names_class`] only answers for the lines that go.
fn forget_class(items: &mut Vec<ClassItem>, name: &str) {
    items.retain(|item| !names_class(item, name));
    for item in items {
        match item {
            ClassItem::Namespace(namespace) => forget_class(&mut namespace.items, name),
            ClassItem::Preserved(text) => {
                if let Some(kept) = retain_statement_targets(text, |target| target != name) {
                    *text = kept;
                }
            }
            _ => {}
        }
    }
}

/// The rows [`forget_class`] would leave behind, so that the fields held beside
/// the items go with them. Runs before it, on the items it is about to change.
fn forget_class_rows(items: &[ClassItem], rows: &mut ItemRows, name: &str) {
    // The nested scopes are paired with their rows by position, and thinning
    // this scope's rows out first would pair them with the wrong items: a
    // namespace would then keep rows for statements `forget_class` is about to
    // drop, and every field below them would be read onto its neighbour.
    for (item, row) in items.iter().zip(&mut rows.items) {
        if let (ClassItem::Namespace(namespace), ItemRow::Namespace { body, .. }) = (item, row) {
            forget_class_rows(&namespace.items, body, name);
        }
    }
    let mut items = items.iter();
    rows.items
        .retain(|_| items.next().is_none_or(|item| !names_class(item, name)));
}

fn names_class(item: &ClassItem, name: &str) -> bool {
    match item {
        ClassItem::Class(declaration) => declaration.reference.name == name,
        ClassItem::Member { reference, .. } | ClassItem::Annotation { reference, .. } => {
            reference.name == name
        }
        ClassItem::Relation(relation) => relation.left.name == name || relation.right.name == name,
        ClassItem::Note(note) => note.target.as_deref() == Some(name),
        // A `style` or `click` line about the class goes with it; one that
        // also addresses other classes loses the entry instead.
        ClassItem::Preserved(text) => {
            retain_statement_targets(text, |target| target != name).is_none()
        }
        ClassItem::Namespace(_) => false,
    }
}

/// The classes `items` declares with a `class` line, including the ones written
/// in the namespaces it holds.
fn declared_class_names(items: &[ClassItem], names: &mut Vec<String>) {
    for item in items {
        match item {
            ClassItem::Class(declaration) => {
                let name = &declaration.reference.name;
                if !names.iter().any(|known| known == name) {
                    names.push(name.clone());
                }
            }
            ClassItem::Namespace(namespace) => declared_class_names(&namespace.items, names),
            _ => {}
        }
    }
}

/// Whether any statement still declares `name` with a `class` line of its own.
/// A class that was declared twice keeps both lines, so deleting one of them
/// leaves the class in the diagram.
fn still_declared(items: &[ClassItem], name: &str) -> bool {
    items.iter().any(|item| match item {
        ClassItem::Class(declaration) => declaration.reference.name == name,
        ClassItem::Namespace(namespace) => still_declared(&namespace.items, name),
        _ => false,
    })
}

/// Every `namespace` name in the diagram, so that a new one gets a name of its
/// own. Namespaces live beside classes rather than among them, which is why
/// [`ClassDocument::class_names`] does not answer for them.
fn namespace_names(items: &[ClassItem], names: &mut Vec<String>) {
    for item in items {
        if let ClassItem::Namespace(namespace) = item {
            names.push(namespace.name.clone());
            namespace_names(&namespace.items, names);
        }
    }
}

fn unused_namespace_name(items: &[ClassItem]) -> String {
    let mut taken = Vec::new();
    namespace_names(items, &mut taken);
    const PREFIX: &str = "名前空間";
    if !taken.iter().any(|name| name == PREFIX) {
        return PREFIX.to_string();
    }
    for suffix in 1..usize::MAX {
        let candidate = format!("{PREFIX}{suffix}");
        if !taken.contains(&candidate) {
            return candidate;
        }
    }
    PREFIX.to_string()
}

impl MermaidEditor {
    pub(super) fn new_class_form(
        &mut self,
        document: ClassDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ClassForm {
        let body = self.new_item_rows(&document.items, window, cx);
        let original_names = document.class_names();
        let names = self.new_name_rows(&original_names, window, cx);
        ClassForm {
            document,
            names,
            body,
            original_names,
        }
    }

    fn new_name_rows(
        &mut self,
        names: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<NameRow> {
        names
            .iter()
            .map(|name| {
                let name = name.clone();
                NameRow {
                    field: self.new_field(&name, window, cx),
                    name,
                }
            })
            .collect()
    }

    fn new_item_rows(
        &mut self,
        items: &[ClassItem],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ItemRows {
        let mut rows = Vec::with_capacity(items.len());
        for item in items {
            rows.push(match item {
                ClassItem::Class(declaration) => {
                    let generic = declaration.reference.generic.clone().unwrap_or_default();
                    let label = declaration.label.clone().unwrap_or_default();
                    let members = declaration
                        .members
                        .iter()
                        .flatten()
                        .map(|member| {
                            let member = member.clone();
                            self.new_field(&member, window, cx)
                        })
                        .collect();
                    ItemRow::Class {
                        generic: self.new_field(&generic, window, cx),
                        label: self.new_field(&label, window, cx),
                        members,
                    }
                }
                ClassItem::Member { text, .. } => {
                    let text = text.clone();
                    ItemRow::Member {
                        text: self.new_field(&text, window, cx),
                    }
                }
                ClassItem::Annotation { annotation, .. } => {
                    let annotation = annotation.clone();
                    ItemRow::Annotation {
                        annotation: self.new_field(&annotation, window, cx),
                    }
                }
                ClassItem::Relation(relation) => {
                    let left_generic = relation.left.generic.clone().unwrap_or_default();
                    let right_generic = relation.right.generic.clone().unwrap_or_default();
                    let left = relation.left_cardinality.clone().unwrap_or_default();
                    let right = relation.right_cardinality.clone().unwrap_or_default();
                    let label = relation.label.clone().unwrap_or_default();
                    ItemRow::Relation {
                        left_generic: self.new_field(&left_generic, window, cx),
                        right_generic: self.new_field(&right_generic, window, cx),
                        left_cardinality: self.new_field(&left, window, cx),
                        right_cardinality: self.new_field(&right, window, cx),
                        label: self.new_field(&label, window, cx),
                    }
                }
                ClassItem::Note(note) => {
                    let text = note.text.clone();
                    ItemRow::Note {
                        text: self.new_field(&text, window, cx),
                    }
                }
                ClassItem::Namespace(namespace) => {
                    let name = namespace.name.clone();
                    let label = namespace.label.clone().unwrap_or_default();
                    ItemRow::Namespace {
                        name: self.new_field(&name, window, cx),
                        label: self.new_field(&label, window, cx),
                        body: self.new_item_rows(&namespace.items, window, cx),
                    }
                }
                ClassItem::Preserved(_) => ItemRow::Fixed,
            });
        }
        ItemRows { items: rows }
    }

    pub(super) fn class(&self) -> Option<&ClassForm> {
        match &self.form {
            Form::Class(form) => Some(form),
            _ => None,
        }
    }

    fn class_mut(&mut self) -> Option<&mut ClassForm> {
        match &mut self.form {
            Form::Class(form) => Some(form),
            _ => None,
        }
    }

    /// Writes the fields into the document and rebuilds the name list from it.
    ///
    /// A statement that was added or removed may have brought a class into the
    /// diagram or taken the last mention of one away, and the name list is
    /// where classes are renamed, so it has to follow.
    fn settle_class(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.class() else {
            return;
        };
        // A name no class can have yet stays a rename in waiting. Written into
        // the document, it would reach the kept `style` / `click` lines too,
        // and those name a class by its spelling: `style 犬 A` addresses `犬`,
        // so the rename the user then finishes would no longer find the line.
        let (renames, pending) = form.settled_renames(cx);
        let document = form.edited_document(&renames, cx);
        // The rename is written into the document here, so the names the
        // diagram opened with have to move with it.
        let original_names = form
            .original_names
            .iter()
            .map(|name| renamed_to(name, &renames))
            .collect::<Vec<_>>();
        let names = document.class_names();
        let unchanged = form.names.len() == names.len()
            && form.names.iter().zip(&names).all(|(row, name)| {
                row.name == *name
                    && (row.field.read(cx).text(cx).trim() == name
                        || pending.iter().any(|(old, _)| old == name))
            });
        let rows = (!unchanged).then(|| {
            let rows = self.new_name_rows(&names, window, cx);
            for (old, typed) in &pending {
                if let Some(row) = rows.iter().find(|row| row.name == *old) {
                    row.field
                        .update(cx, |field, cx| field.set_text(typed.as_str(), window, cx));
                }
            }
            rows
        });
        let Some(form) = self.class_mut() else {
            return;
        };
        if let Some(rows) = rows {
            form.names = rows;
        }
        form.document = document;
        form.original_names = original_names;
        self.schedule_preview(cx);
        cx.notify();
    }

    /// Adds `item` with its `row` to the end of the scope `path` addresses.
    fn push_class_item(
        &mut self,
        path: &[usize],
        item: ClassItem,
        row: ItemRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        {
            let Some(form) = self.class_mut() else {
                return;
            };
            let (Some(items), Some(rows)) = (
                items_at(&mut form.document.items, path),
                rows_at(&mut form.body, path),
            ) else {
                return;
            };
            items.push(item);
            rows.items.push(row);
        }
        self.settle_class(window, cx);
    }

    /// The class a statement that has to name one starts out addressing, or the
    /// message that says why there is none.
    fn first_class_name(&mut self, cx: &mut Context<Self>) -> Option<String> {
        let name = self
            .class()
            .and_then(|form| form.settled_class_names(cx).first().cloned());
        if name.is_none() {
            self.message = Some("先にクラスを追加してください。".into());
            cx.notify();
        }
        name
    }

    pub(super) fn add_class_item(
        &mut self,
        path: Vec<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.class() else {
            return;
        };
        let name = form.current_document(cx).unused_name("クラス");
        let row = ItemRow::Class {
            generic: self.new_field("", window, cx),
            label: self.new_field("", window, cx),
            members: Vec::new(),
        };
        self.push_class_item(
            &path,
            ClassItem::Class(ClassDecl {
                reference: ClassRef::new(name),
                label: None,
                css_class: None,
                members: None,
                comment: None,
            }),
            row,
            window,
            cx,
        );
    }

    fn add_member_statement(
        &mut self,
        path: Vec<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(name) = self.first_class_name(cx) else {
            return;
        };
        let row = ItemRow::Member {
            text: self.new_field("+メソッド()", window, cx),
        };
        self.push_class_item(
            &path,
            ClassItem::Member {
                reference: ClassRef::new(name),
                text: "+メソッド()".to_string(),
            },
            row,
            window,
            cx,
        );
    }

    fn add_annotation(&mut self, path: Vec<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.first_class_name(cx) else {
            return;
        };
        let row = ItemRow::Annotation {
            annotation: self.new_field("interface", window, cx),
        };
        self.push_class_item(
            &path,
            ClassItem::Annotation {
                reference: ClassRef::new(name),
                annotation: "interface".to_string(),
                comment: None,
            },
            row,
            window,
            cx,
        );
    }

    fn add_relation(&mut self, path: Vec<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.class() else {
            return;
        };
        let names = form.settled_class_names(cx);
        let Some(left) = names.first().cloned() else {
            self.message = Some("先にクラスを追加してください。".into());
            cx.notify();
            return;
        };
        let right = names.get(1).cloned().unwrap_or_else(|| left.clone());
        let row = ItemRow::Relation {
            left_generic: self.new_field("", window, cx),
            right_generic: self.new_field("", window, cx),
            left_cardinality: self.new_field("", window, cx),
            right_cardinality: self.new_field("", window, cx),
            label: self.new_field("", window, cx),
        };
        self.push_class_item(
            &path,
            ClassItem::Relation(ClassRelation {
                left: ClassRef::new(left),
                left_cardinality: None,
                arrow: "<|--".to_string(),
                right_cardinality: None,
                right: ClassRef::new(right),
                label: None,
                comment: None,
            }),
            row,
            window,
            cx,
        );
    }

    fn add_class_note(&mut self, path: Vec<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.class() else {
            return;
        };
        // A note with no class is a diagram Mermaid draws, so an empty diagram
        // is no reason to refuse one.
        let target = form.settled_class_names(cx).first().cloned();
        let row = ItemRow::Note {
            text: self.new_field("メモ", window, cx),
        };
        self.push_class_item(
            &path,
            ClassItem::Note(ClassNote {
                target,
                text: "メモ".to_string(),
                comment: None,
            }),
            row,
            window,
            cx,
        );
    }

    fn add_namespace(&mut self, path: Vec<usize>, window: &mut Window, cx: &mut Context<Self>) {
        if path.len() + 1 >= MAX_DEPTH {
            self.message = Some("`namespace` の入れ子が深すぎます。".into());
            cx.notify();
            return;
        }
        let Some(form) = self.class() else {
            return;
        };
        let name = unused_namespace_name(&form.document.items);
        let row = ItemRow::Namespace {
            name: self.new_field(&name, window, cx),
            label: self.new_field("", window, cx),
            body: ItemRows::default(),
        };
        self.push_class_item(
            &path,
            ClassItem::Namespace(ClassNamespace {
                name,
                label: None,
                items: Vec::new(),
                comment: None,
            }),
            row,
            window,
            cx,
        );
    }

    pub(super) fn remove_class_item(
        &mut self,
        path: Vec<usize>,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        {
            let Some(form) = self.class_mut() else {
                return;
            };
            let (Some(items), Some(rows)) = (
                items_at(&mut form.document.items, &path),
                rows_at(&mut form.body, &path),
            ) else {
                return;
            };
            if index >= items.len() || index >= rows.items.len() {
                return;
            }
            let removed = items.remove(index);
            rows.items.remove(index);
            // Everything that still named the class would name it back into the
            // diagram as an empty box — unless another `class` line still
            // declares it, in which case only this statement goes. Deleting a
            // namespace deletes the classes it held, as the button says it
            // does, so they are orphaned the same way.
            let orphaned = match &removed {
                ClassItem::Class(declaration) => vec![declaration.reference.name.clone()],
                ClassItem::Namespace(namespace) => {
                    let mut names = Vec::new();
                    declared_class_names(&namespace.items, &mut names);
                    names
                }
                _ => Vec::new(),
            };
            for name in orphaned {
                if !still_declared(&form.document.items, &name) {
                    forget_class_rows(&form.document.items, &mut form.body, &name);
                    forget_class(&mut form.document.items, &name);
                }
            }
        }
        self.settle_class(window, cx);
    }

    fn move_class_item(
        &mut self,
        path: Vec<usize>,
        index: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.class_mut() else {
            return;
        };
        let (Some(items), Some(rows)) = (
            items_at(&mut form.document.items, &path),
            rows_at(&mut form.body, &path),
        ) else {
            return;
        };
        let Some(other) = neighbour(index, forward, items.len()) else {
            return;
        };
        if other >= rows.items.len() || index >= rows.items.len() {
            return;
        }
        items.swap(index, other);
        rows.items.swap(index, other);
        self.schedule_preview(cx);
        cx.notify();
    }

    /// Changes one item through `edit`, which sees the item the path and index
    /// address.
    fn update_class_item(
        &mut self,
        path: &[usize],
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut ClassItem),
    ) {
        {
            let Some(form) = self.class_mut() else {
                return;
            };
            let Some(items) = items_at(&mut form.document.items, path) else {
                return;
            };
            let Some(item) = items.get_mut(index) else {
                return;
            };
            edit(item);
        }
        self.settle_class(window, cx);
    }

    fn add_class_member(
        &mut self,
        path: Vec<usize>,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let field = self.new_field("+メソッド()", window, cx);
        {
            let Some(form) = self.class_mut() else {
                return;
            };
            let (Some(items), Some(rows)) = (
                items_at(&mut form.document.items, &path),
                rows_at(&mut form.body, &path),
            ) else {
                return;
            };
            let (Some(ClassItem::Class(declaration)), Some(ItemRow::Class { members, .. })) =
                (items.get_mut(index), rows.items.get_mut(index))
            else {
                return;
            };
            declaration
                .members
                .get_or_insert_with(Vec::new)
                .push("+メソッド()".to_string());
            members.push(field);
        }
        self.settle_class(window, cx);
    }

    fn remove_class_member(
        &mut self,
        path: Vec<usize>,
        index: usize,
        member: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        {
            let Some(form) = self.class_mut() else {
                return;
            };
            let (Some(items), Some(rows)) = (
                items_at(&mut form.document.items, &path),
                rows_at(&mut form.body, &path),
            ) else {
                return;
            };
            let (Some(ClassItem::Class(declaration)), Some(ItemRow::Class { members, .. })) =
                (items.get_mut(index), rows.items.get_mut(index))
            else {
                return;
            };
            let Some(held) = &mut declaration.members else {
                return;
            };
            if member >= held.len() || member >= members.len() {
                return;
            }
            held.remove(member);
            members.remove(member);
            // An empty block is drawn as an empty compartment, which is not
            // what taking the last member out asks for.
            if held.is_empty() {
                declaration.members = None;
            }
        }
        self.settle_class(window, cx);
    }

    fn move_class_member(
        &mut self,
        path: Vec<usize>,
        index: usize,
        member: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.class_mut() else {
            return;
        };
        let (Some(items), Some(rows)) = (
            items_at(&mut form.document.items, &path),
            rows_at(&mut form.body, &path),
        ) else {
            return;
        };
        let (Some(ClassItem::Class(declaration)), Some(ItemRow::Class { members, .. })) =
            (items.get_mut(index), rows.items.get_mut(index))
        else {
            return;
        };
        let Some(held) = &mut declaration.members else {
            return;
        };
        let Some(other) = neighbour(member, forward, held.len()) else {
            return;
        };
        if other >= members.len() || member >= members.len() {
            return;
        }
        held.swap(member, other);
        members.swap(member, other);
        self.schedule_preview(cx);
        cx.notify();
    }

    fn set_class_direction(&mut self, direction: Option<FlowDirection>, cx: &mut Context<Self>) {
        let Some(form) = self.class_mut() else {
            return;
        };
        form.document.direction = direction;
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn render_class_toolbar(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(form) = self.class() else {
            return div().into_any_element();
        };
        let current = form.document.direction;
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for choice in [None].into_iter().chain(FlowDirection::ALL.map(Some)) {
                let this = this.clone();
                menu = menu.toggleable_entry(
                    direction_label(choice),
                    choice == current,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        this.update(cx, |this, cx| this.set_class_direction(choice, cx))
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
                DropdownMenu::new("mermaid-class-direction", direction_label(current), menu)
                    .style(DropdownStyle::Outlined)
                    .trigger_size(ButtonSize::Compact),
            )
            .into_any_element()
    }

    pub(super) fn render_class(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(form) = self.class() else {
            return Vec::new();
        };
        let names = form
            .names
            .iter()
            .map(|row| row.field.read(cx).text(cx).trim().to_string())
            .collect::<Vec<_>>();

        let mut name_list = v_flex()
            .w_full()
            .gap_1()
            .child(section_title("クラス名 (変えると参照も追随します)"));
        for row in &form.names {
            let valid = is_valid_class_name(row.field.read(cx).text(cx).trim());
            name_list = name_list.child(
                field_frame(&row.field, ID_FIELD_WIDTH, valid, window, cx).child(row.field.clone()),
            );
        }

        vec![
            name_list.into_any_element(),
            v_flex()
                .w_full()
                .gap_1()
                .child(section_title("図の中身"))
                .child(self.render_class_scope(
                    &[],
                    &form.document.items,
                    &form.body,
                    &names,
                    window,
                    cx,
                ))
                .into_any_element(),
        ]
    }

    fn render_class_scope(
        &self,
        path: &[usize],
        items: &[ClassItem],
        rows: &ItemRows,
        names: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = path_key(path);
        let row_prefix = format!("mermaid-class-{key}");
        let last = items.len().saturating_sub(1);
        let mut list = v_flex().w_full().gap_1();

        for index in 0..items.len() {
            let (Some(item), Some(row)) = (items.get(index), rows.items.get(index)) else {
                continue;
            };
            let controls = {
                let (up, down, remove) = (path.to_vec(), path.to_vec(), path.to_vec());
                let remove_tooltip = match item {
                    ClassItem::Class(_) => "削除 (このクラスを指す文も削除されます)",
                    ClassItem::Namespace(_) => "削除 (中のクラスも削除されます)",
                    _ => "削除",
                };
                row_controls_labelled(
                    &row_prefix,
                    index,
                    index == 0,
                    index >= last,
                    remove_tooltip,
                    cx.listener(move |this, _, _window, cx| {
                        this.move_class_item(up.clone(), index, false, cx)
                    }),
                    cx.listener(move |this, _, _window, cx| {
                        this.move_class_item(down.clone(), index, true, cx)
                    }),
                    cx.listener(move |this, _, window, cx| {
                        this.remove_class_item(remove.clone(), index, window, cx)
                    }),
                )
            };

            list = match (item, row) {
                (
                    ClassItem::Class(declaration),
                    ItemRow::Class {
                        generic,
                        label,
                        members,
                    },
                ) => {
                    let mut block = v_flex().w_full().gap_1().pl_4();
                    let held = declaration.members.as_deref().unwrap_or_default();
                    for member_index in 0..members.len() {
                        let (Some(field), Some(member)) =
                            (members.get(member_index), held.get(member_index))
                        else {
                            continue;
                        };
                        let (up, down, remove) = (path.to_vec(), path.to_vec(), path.to_vec());
                        block = block.child(
                            h_flex()
                                .w_full()
                                .gap_1()
                                .child(
                                    field_frame(
                                        field,
                                        LABEL_FIELD_WIDTH,
                                        is_valid_block_member_text(member),
                                        window,
                                        cx,
                                    )
                                    .flex_1()
                                    .child(field.clone()),
                                )
                                .child(row_controls(
                                    &format!("{row_prefix}-member-{index}"),
                                    member_index,
                                    member_index == 0,
                                    member_index + 1 >= members.len(),
                                    cx.listener(move |this, _, _window, cx| {
                                        this.move_class_member(
                                            up.clone(),
                                            index,
                                            member_index,
                                            false,
                                            cx,
                                        )
                                    }),
                                    cx.listener(move |this, _, _window, cx| {
                                        this.move_class_member(
                                            down.clone(),
                                            index,
                                            member_index,
                                            true,
                                            cx,
                                        )
                                    }),
                                    cx.listener(move |this, _, window, cx| {
                                        this.remove_class_member(
                                            remove.clone(),
                                            index,
                                            member_index,
                                            window,
                                            cx,
                                        )
                                    }),
                                )),
                        );
                    }
                    let add_path = path.to_vec();
                    block = block.child(add_button(
                        SharedString::from(format!("{row_prefix}-add-member-{index}")),
                        SharedString::from("メンバーを追加"),
                        cx.listener(move |this, _, window, cx| {
                            this.add_class_member(add_path.clone(), index, window, cx)
                        }),
                    ));

                    list.child(
                        v_flex()
                            .w_full()
                            .gap_1()
                            .child(
                                h_flex()
                                    .w_full()
                                    .gap_1()
                                    .child(
                                        Label::new("クラス")
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                    .child(name_label(&declaration.reference.name))
                                    .child(
                                        field_frame(
                                            generic,
                                            NUMBER_FIELD_WIDTH,
                                            declaration
                                                .reference
                                                .generic
                                                .as_ref()
                                                .is_none_or(|generic| is_valid_generic(generic)),
                                            window,
                                            cx,
                                        )
                                        .child(generic.clone()),
                                    )
                                    .child(
                                        field_frame(
                                            label,
                                            LABEL_FIELD_WIDTH,
                                            declaration
                                                .label
                                                .as_ref()
                                                .is_none_or(|label| is_valid_quoted_text(label)),
                                            window,
                                            cx,
                                        )
                                        .flex_1()
                                        .child(label.clone()),
                                    )
                                    .child(controls),
                            )
                            .child(block),
                    )
                }
                (ClassItem::Member { reference, text }, ItemRow::Member { text: field }) => list
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .child(
                                Label::new("メンバー")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(self.render_class_target(
                                path,
                                index,
                                &reference.name,
                                names,
                                false,
                                window,
                                cx,
                            ))
                            .child(
                                field_frame(
                                    field,
                                    LABEL_FIELD_WIDTH,
                                    is_valid_member_text(text),
                                    window,
                                    cx,
                                )
                                .flex_1()
                                .child(field.clone()),
                            )
                            .child(controls),
                    ),
                (
                    ClassItem::Annotation {
                        reference,
                        annotation,
                        ..
                    },
                    ItemRow::Annotation {
                        annotation: field, ..
                    },
                ) => list.child(
                    h_flex()
                        .w_full()
                        .gap_1()
                        .child(
                            Label::new("注釈")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            field_frame(
                                field,
                                ID_FIELD_WIDTH,
                                is_valid_annotation(annotation),
                                window,
                                cx,
                            )
                            .child(field.clone()),
                        )
                        .child(self.render_class_target(
                            path,
                            index,
                            &reference.name,
                            names,
                            false,
                            window,
                            cx,
                        ))
                        .child(controls),
                ),
                (
                    ClassItem::Relation(relation),
                    ItemRow::Relation {
                        left_generic,
                        right_generic,
                        left_cardinality,
                        right_cardinality,
                        label,
                    },
                ) => list.child(
                    h_flex()
                        .w_full()
                        .gap_1()
                        .child(self.render_class_endpoint(
                            path,
                            index,
                            true,
                            &relation.left,
                            left_generic,
                            names,
                            window,
                            cx,
                        ))
                        .child(
                            field_frame(
                                left_cardinality,
                                NUMBER_FIELD_WIDTH,
                                relation
                                    .left_cardinality
                                    .as_ref()
                                    .is_none_or(|text| is_valid_quoted_text(text)),
                                window,
                                cx,
                            )
                            .child(left_cardinality.clone()),
                        )
                        .child(self.render_relation_arrow(path, index, &relation.arrow, window, cx))
                        .child(
                            field_frame(
                                right_cardinality,
                                NUMBER_FIELD_WIDTH,
                                relation
                                    .right_cardinality
                                    .as_ref()
                                    .is_none_or(|text| is_valid_quoted_text(text)),
                                window,
                                cx,
                            )
                            .child(right_cardinality.clone()),
                        )
                        .child(self.render_class_endpoint(
                            path,
                            index,
                            false,
                            &relation.right,
                            right_generic,
                            names,
                            window,
                            cx,
                        ))
                        .child(
                            field_frame(
                                label,
                                LABEL_FIELD_WIDTH,
                                relation
                                    .label
                                    .as_ref()
                                    .is_none_or(|label| is_valid_relation_label(label)),
                                window,
                                cx,
                            )
                            .flex_1()
                            .child(label.clone()),
                        )
                        .child(controls),
                ),
                (ClassItem::Note(note), ItemRow::Note { text }) => list.child(
                    h_flex()
                        .w_full()
                        .gap_1()
                        .child(
                            Label::new("ノート")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(self.render_class_target(
                            path,
                            index,
                            note.target.as_deref().unwrap_or_default(),
                            names,
                            true,
                            window,
                            cx,
                        ))
                        .child(
                            field_frame(
                                text,
                                LABEL_FIELD_WIDTH,
                                is_valid_quoted_text(&note.text),
                                window,
                                cx,
                            )
                            .flex_1()
                            .child(text.clone()),
                        )
                        .child(controls),
                ),
                (
                    ClassItem::Namespace(namespace),
                    ItemRow::Namespace {
                        name,
                        label,
                        body: nested,
                    },
                ) => {
                    let mut child_path = path.to_vec();
                    child_path.push(index);
                    list.child(
                        v_flex()
                            .w_full()
                            .gap_1()
                            .child(
                                h_flex()
                                    .w_full()
                                    .gap_1()
                                    .child(
                                        Label::new("namespace")
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                    .child(
                                        field_frame(
                                            name,
                                            ID_FIELD_WIDTH,
                                            is_valid_class_name(&namespace.name),
                                            window,
                                            cx,
                                        )
                                        .child(name.clone()),
                                    )
                                    .child(
                                        field_frame(
                                            label,
                                            LABEL_FIELD_WIDTH,
                                            namespace
                                                .label
                                                .as_ref()
                                                .is_none_or(|label| is_valid_quoted_text(label)),
                                            window,
                                            cx,
                                        )
                                        .flex_1()
                                        .child(label.clone()),
                                    )
                                    .child(controls),
                            )
                            .child(div().w_full().pl_4().child(self.render_class_scope(
                                &child_path,
                                &namespace.items,
                                nested,
                                names,
                                window,
                                cx,
                            ))),
                    )
                }
                // A line the model keeps verbatim stays where it is and is not
                // offered for editing; showing it as a field would invite a
                // change the model could not carry back.
                (ClassItem::Preserved(_), _) => list,
                _ => list,
            };
        }

        let in_namespace = !path.is_empty();
        let mut buttons = h_flex().w_full().flex_wrap().gap_1();
        for (id, label, kind) in [
            ("class", "クラス", AddKind::Class),
            ("member", "メンバー行", AddKind::Member),
            ("annotation", "注釈", AddKind::Annotation),
            ("relation", "関係", AddKind::Relation),
            ("note", "ノート", AddKind::Note),
            ("namespace", "namespace", AddKind::Namespace),
        ] {
            // Mermaid only takes class declarations, notes and further
            // namespaces inside a namespace.
            if in_namespace && !matches!(kind, AddKind::Class | AddKind::Note | AddKind::Namespace)
            {
                continue;
            }
            let path = path.to_vec();
            buttons = buttons.child(add_button(
                SharedString::from(format!("mermaid-class-add-{id}-{key}")),
                SharedString::from(format!("{label}を追加")),
                cx.listener(move |this, _, window, cx| {
                    let path = path.clone();
                    match kind {
                        AddKind::Class => this.add_class_item(path, window, cx),
                        AddKind::Member => this.add_member_statement(path, window, cx),
                        AddKind::Annotation => this.add_annotation(path, window, cx),
                        AddKind::Relation => this.add_relation(path, window, cx),
                        AddKind::Note => this.add_class_note(path, window, cx),
                        AddKind::Namespace => this.add_namespace(path, window, cx),
                    }
                }),
            ));
        }

        list.child(buttons).into_any_element()
    }

    /// The menu that picks which class a member, an annotation or a note is
    /// about. `allow_none` is for the floating note, which is about no class.
    #[allow(clippy::too_many_arguments)]
    fn render_class_target(
        &self,
        path: &[usize],
        index: usize,
        current: &str,
        names: &[String],
        allow_none: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let owned_path = path.to_vec();
        let selected = current.to_string();
        let choices = allow_none
            .then(String::new)
            .into_iter()
            .chain(names.iter().cloned())
            .collect::<Vec<_>>();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for name in choices.clone() {
                let this = this.clone();
                let path = owned_path.clone();
                let is_selected = name == selected;
                menu = menu.toggleable_entry(
                    target_label(&name),
                    is_selected,
                    IconPosition::Start,
                    None,
                    move |window, cx| {
                        let (path, name) = (path.clone(), name.clone());
                        this.update(cx, |this, cx| {
                            this.update_class_item(&path, index, window, cx, |item| match item {
                                ClassItem::Member { reference, .. }
                                | ClassItem::Annotation { reference, .. } => {
                                    *reference = ClassRef::new(name)
                                }
                                ClassItem::Note(note) => {
                                    note.target = (!name.is_empty()).then_some(name)
                                }
                                _ => {}
                            });
                        })
                        .log_err();
                    },
                );
            }
            menu
        });

        DropdownMenu::new(
            SharedString::from(format!("mermaid-class-target-{}-{index}", path_key(path))),
            target_label(current),
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
    }

    /// The menu that picks the class at one end of a relation, with the field
    /// for the `~T~` written after it.
    #[allow(clippy::too_many_arguments)]
    fn render_class_endpoint(
        &self,
        path: &[usize],
        index: usize,
        is_left: bool,
        current: &ClassRef,
        generic: &Entity<Editor>,
        names: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let owned_path = path.to_vec();
        let selected = current.name.clone();
        let choices = names.to_vec();
        let generic_field = generic.clone();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for name in choices.clone() {
                let this = this.clone();
                let path = owned_path.clone();
                let generic_field = generic_field.clone();
                let is_selected = name == selected;
                menu = menu.toggleable_entry(
                    SharedString::from(name.clone()),
                    is_selected,
                    IconPosition::Start,
                    None,
                    move |window, cx| {
                        if is_selected {
                            return;
                        }
                        let (path, name) = (path.clone(), name.clone());
                        this.update(cx, |this, cx| {
                            // The type parameter belonged to the class that was
                            // there before, so it does not travel to the one
                            // that replaces it. The field is read back over the
                            // document when the form settles, so it is the
                            // field that has to be emptied.
                            generic_field.update(cx, |field, cx| field.set_text("", window, cx));
                            this.update_class_item(&path, index, window, cx, |item| {
                                if let ClassItem::Relation(relation) = item {
                                    let end = match is_left {
                                        true => &mut relation.left,
                                        false => &mut relation.right,
                                    };
                                    *end = ClassRef::new(name);
                                }
                            });
                        })
                        .log_err();
                    },
                );
            }
            menu
        });

        let valid = current
            .generic
            .as_ref()
            .is_none_or(|generic| is_valid_generic(generic));
        h_flex()
            .gap_1()
            .child(
                DropdownMenu::new(
                    SharedString::from(format!(
                        "mermaid-class-endpoint-{}-{index}-{}",
                        path_key(path),
                        usize::from(is_left)
                    )),
                    SharedString::from(current.name.clone()),
                    menu,
                )
                .style(DropdownStyle::Outlined)
                .trigger_size(ButtonSize::Compact),
            )
            .child(
                field_frame(generic, NUMBER_FIELD_WIDTH, valid, window, cx).child(generic.clone()),
            )
            .into_any_element()
    }

    fn render_relation_arrow(
        &self,
        path: &[usize],
        index: usize,
        current: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let owned_path = path.to_vec();
        let selected = current.to_string();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for (arrow, label) in RELATION_PRESETS {
                let this = this.clone();
                let path = owned_path.clone();
                menu = menu.toggleable_entry(
                    format!("{arrow}  {label}"),
                    arrow == selected,
                    IconPosition::Start,
                    None,
                    move |window, cx| {
                        let path = path.clone();
                        this.update(cx, |this, cx| {
                            this.update_class_item(&path, index, window, cx, |item| {
                                if let ClassItem::Relation(relation) = item {
                                    relation.arrow = arrow.to_string();
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
            SharedString::from(format!("mermaid-class-arrow-{}-{index}", path_key(path))),
            arrow_label(current),
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
    }
}

#[derive(Clone, Copy)]
enum AddKind {
    Class,
    Member,
    Annotation,
    Relation,
    Note,
    Namespace,
}

/// A path spelled for an element id, which has to be unique within the modal.
fn path_key(path: &[usize]) -> String {
    path.iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join("-")
}

/// The name of the class a statement is about. It is shown rather than typed:
/// the list above the statements is the one place a class is renamed, so that a
/// class named only by its relations can be renamed too.
fn name_label(name: &str) -> Label {
    Label::new(SharedString::from(name.to_string())).size(LabelSize::Small)
}

fn target_label(name: &str) -> SharedString {
    match name.is_empty() {
        true => "クラスなし".into(),
        false => SharedString::from(name.to_string()),
    }
}

/// The operator a relation is drawn with, with the words the preset list uses
/// for it. An operator the presets do not hold is shown as it was written.
fn arrow_label(arrow: &str) -> SharedString {
    RELATION_PRESETS
        .iter()
        .find(|(preset, _)| *preset == arrow)
        .map(|(preset, label)| SharedString::from(format!("{preset}  {label}")))
        .unwrap_or_else(|| SharedString::from(arrow.to_string()))
}

fn direction_label(direction: Option<FlowDirection>) -> &'static str {
    match direction {
        Some(direction) => direction.label(),
        None => "指定なし",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor::test::editor_test_context::EditorTestContext;
    use gpui::TestAppContext;
    use indoc::indoc;
    use mermaid_render::class_model::{emit_class, parse_class};
    use mermaid_render::mermaid_model::parse_document;
    use std::sync::Arc;

    fn document(source: &str) -> ClassDocument {
        parse_class(source).unwrap_or_else(|error| panic!("{source} should parse: {error}"))
    }

    /// Renaming a class has to carry into every statement that named it, or
    /// Mermaid draws the old name as an extra, empty box.
    #[test]
    fn a_rename_carries_into_every_statement_that_names_the_class() {
        let mut document = document(
            "classDiagram
  class 動物
  動物 : +鳴く()
  <<abstract>> 動物
  動物 <|-- 犬
  note for 動物 \"メモ\"
  namespace 生き物 {
    class 動物
  }",
        );
        apply_renames(
            &mut document.items,
            &[("動物".to_string(), "生物".to_string())],
        );
        let emitted = emit_class(&document);
        assert!(
            !emitted.contains("動物"),
            "the old name should be gone:\n{emitted}"
        );
        assert_eq!(emitted.matches("生物").count(), 6, "{emitted}");
        assert_eq!(
            document.class_names(),
            vec!["生物".to_string(), "犬".to_string()]
        );
    }

    /// A class that is only named by a relation still has to be renameable,
    /// which is what the name list above the statements is for.
    #[test]
    fn a_class_named_only_by_a_relation_is_in_the_name_list() {
        let document = document("classDiagram\n  動物 <|-- 犬");
        assert_eq!(
            document.class_names(),
            vec!["動物".to_string(), "犬".to_string()]
        );
    }

    /// Deleting one of two `class A` lines leaves the class in the diagram, so
    /// the statements that name it have to stay too.
    #[test]
    fn a_class_declared_twice_is_still_declared_after_one_line_goes() {
        let twice = document("classDiagram\n  class A\n  class A\n  A --> B").items;
        let once = document("classDiagram\n  class A\n  A --> B").items;
        for (items, expected) in [(twice, true), (once, false)] {
            let remaining = items.get(1..).unwrap_or_default();
            assert_eq!(still_declared(remaining, "A"), expected, "{items:?}");
        }
        // A class declared inside a namespace counts, since the namespace is
        // where Mermaid draws it.
        let nested = document("classDiagram\n  class A\n  namespace N {\n    class A\n  }").items;
        assert!(still_declared(nested.get(1..).unwrap_or_default(), "A"));
    }

    #[test]
    fn a_namespace_gets_a_name_of_its_own() {
        let items = document(
            "classDiagram\n  namespace 名前空間 {\n    namespace 名前空間1 {\n      class A\n    }\n  }",
        )
        .items;
        assert_eq!(unused_namespace_name(&items), "名前空間2");
        assert_eq!(unused_namespace_name(&[]), "名前空間");
    }

    #[test]
    fn the_problem_line_names_what_the_user_has_to_fix() {
        let cases: [(&str, fn(&mut ClassDocument), &str); 7] = [
            (
                "classDiagram\n  class A",
                |document| {
                    if let Some(ClassItem::Class(declaration)) = document.items.first_mut() {
                        declaration.reference.name = "駄目 な".to_string();
                    }
                },
                "クラス名",
            ),
            (
                "classDiagram\n  class A {\n    +x()\n  }",
                |document| {
                    if let Some(ClassItem::Class(declaration)) = document.items.first_mut()
                        && let Some(members) = &mut declaration.members
                        && let Some(member) = members.first_mut()
                    {
                        // A line that is nothing but `}` would close the block
                        // it is in and take the lines below it with it.
                        *member = "}".to_string();
                    }
                },
                "メンバー",
            ),
            (
                "classDiagram\n  class A",
                |document| {
                    if let Some(ClassItem::Class(declaration)) = document.items.first_mut() {
                        declaration.label = Some("引用 \" つき".to_string());
                    }
                },
                "表示名",
            ),
            (
                "classDiagram\n  A --> B",
                |document| {
                    if let Some(ClassItem::Relation(relation)) = document.items.first_mut() {
                        relation.arrow = "=>".to_string();
                    }
                },
                "関係の記号",
            ),
            (
                "classDiagram\n  A --> B",
                |document| {
                    if let Some(ClassItem::Relation(relation)) = document.items.first_mut() {
                        relation.left_cardinality = Some("引用 \" つき".to_string());
                    }
                },
                "多重度",
            ),
            (
                "classDiagram\n  class A\n  note for A \"メモ\"",
                |document| {
                    if let Some(ClassItem::Note(note)) = document.items.get_mut(1) {
                        note.text = "引用 \" つき".to_string();
                    }
                },
                "ノート",
            ),
            (
                "classDiagram\n  namespace N {\n    class A\n  }",
                |document| {
                    if let Some(ClassItem::Namespace(namespace)) = document.items.first_mut() {
                        namespace.name = "駄目 な".to_string();
                    }
                },
                "namespace",
            ),
        ];
        for (source, break_it, expected) in cases {
            let mut document = document(source);
            assert_eq!(problem(&document), None, "`{source}` should be clean");
            break_it(&mut document);
            let message = problem(&document)
                .unwrap_or_else(|| panic!("`{source}` should be a problem after it is broken"));
            assert!(
                message.contains(expected),
                "the message should mention `{expected}`, got `{message}`"
            );
        }
    }

    /// Every document the model parses has to be one the form reports as clean,
    /// otherwise a diagram the user never touched could not be confirmed.
    #[test]
    fn a_parsed_document_has_nothing_wrong_with_it() {
        for source in [
            "classDiagram\n  class A[\"表示\"] {\n    <<interface>>\n    +x() int\n  }\n  A <|-- B",
            "classDiagram\n  direction LR\n  namespace N {\n    class A\n    note for A \"メモ\"\n  }\n  A \"1\" o-- \"0..*\" B : 持つ",
            "classDiagram\n  class Box~T~\n  Box : +get() T\n  <<generic>> Box",
            "classDiagram\n  %% メモ\n  class A\n\n  classDef warn fill:#f96",
        ] {
            assert_eq!(problem(&document(source)), None, "for {source}");
        }
    }

    /// The trap the state form fell into: thinning a scope's rows out before
    /// walking into its nested scopes pairs the nested items with the wrong
    /// rows, and a deleted statement's field is then read onto its neighbour.
    ///
    /// The relation at the top drops a row above the namespace, and the note
    /// about 犬 drops one above the note about 猫, so a pairing that slipped
    /// writes the first note's text onto the second.
    #[gpui::test]
    async fn deleting_a_class_takes_its_statements_without_shifting_the_others(
        cx: &mut TestAppContext,
    ) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                classDiagramˇ
                    class 犬
                    犬 <|-- 柴
                    class 猫[\"ねこ\"]
                    namespace 生き物 {
                        note for 犬 \"犬のメモ\"
                        note for 猫 \"猫のメモ\"
                    }
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.remove_class_item(Vec::new(), 0, window, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            cx.update_editor(|editor, _window, cx| editor.text(cx)),
            indoc! {"
                ```mermaid
                classDiagram
                    class 猫[\"ねこ\"]
                    namespace 生き物 {
                        note for 猫 \"猫のメモ\"
                    }
                ```
            "},
        );
    }

    /// Renaming in the name list has to reach the relation that only mentions
    /// the class, the note that is about it and the member line under it.
    #[gpui::test]
    async fn renaming_a_class_rewrites_every_statement_that_named_it(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                classDiagramˇ
                    class 動物
                    動物 : +鳴く()
                    動物 <|-- 犬
                    note for 動物 \"メモ\"
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let Some(row) = modal.class().and_then(|form| form.names.first()) else {
                    panic!("the diagram names two classes");
                };
                let field = row.field.clone();
                field.update(cx, |field, cx| field.set_text("生物", window, cx));
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            cx.update_editor(|editor, _window, cx| editor.text(cx)),
            indoc! {"
                ```mermaid
                classDiagram
                    class 生物
                    生物 : +鳴く()
                    生物 <|-- 犬
                    note for 生物 \"メモ\"
                ```
            "},
        );
    }

    /// Taking the last member out of a block takes the block with it: an empty
    /// one is drawn as an empty compartment, which is not what the button asks
    /// for.
    #[gpui::test]
    async fn the_member_block_goes_when_its_last_member_does(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                classDiagramˇ
                    class A {
                        +x()
                    }
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.remove_class_member(Vec::new(), 0, 0, window, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            cx.update_editor(|editor, _window, cx| editor.text(cx)),
            indoc! {"
                ```mermaid
                classDiagram
                    class A
                ```
            "},
        );
    }

    /// A value the model cannot write back has to stop the write rather than
    /// being written and breaking the fence.
    #[gpui::test]
    async fn a_quote_in_a_note_is_refused_and_the_fence_is_left_alone(cx: &mut TestAppContext) {
        let source = indoc! {"
            ```mermaid
            classDiagramˇ
                class A
                note for A \"メモ\"
            ```
        "};
        let (mut cx, modal, _app_state) = open_on(source, cx).await;
        let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let Some(ItemRow::Note { text }) =
                    modal.class().and_then(|form| form.body.items.get(1))
                else {
                    panic!("the second statement is the note");
                };
                let text = text.clone();
                text.update(cx, |text, cx| text.set_text("引用 \" つき", window, cx));
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            cx.update_editor(|editor, _window, cx| editor.text(cx)),
            before,
            "a note Mermaid could not read must not be written back"
        );
        let message = cx.update(|_window, cx| modal.read(cx).message.clone());
        assert_eq!(
            message.as_deref(),
            Some("ノートの本文に `\"` と改行は使えません。")
        );
    }

    /// The preserved lines have to keep their place through an edit, which is
    /// the check the ER form's `brace_delta` bug slipped past.
    #[gpui::test]
    async fn the_comments_around_an_edited_statement_keep_their_place(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                classDiagramˇ
                    %% 上のメモ
                    class A {
                        +x()
                    }
                    %% 下のメモ
                    classDef warn fill:#f96
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.add_class_member(Vec::new(), 1, window, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            cx.update_editor(|editor, _window, cx| editor.text(cx)),
            indoc! {"
                ```mermaid
                classDiagram
                    %% 上のメモ
                    class A {
                        +x()
                        +メソッド()
                    }
                    %% 下のメモ
                    classDef warn fill:#f96
                ```
            "},
        );
    }

    /// A `%%` line inside a member block is one of the block's lines rather
    /// than a comment Mermaid strips, so a diagram that holds one has to stay
    /// confirmable: a validator that refused it would open the fence and then
    /// never let it be written back.
    #[test]
    fn a_comment_inside_a_member_block_does_not_block_the_write_back() {
        let mut emptied = document("classDiagram\n  class A {\n    +x()\n    %% 説明\n  }");
        assert_eq!(problem(&emptied), None);

        // An emptied member is a blank line in the block, which Mermaid skips
        // and the model reads back as the same empty entry.
        if let Some(ClassItem::Class(declaration)) = emptied.items.first_mut()
            && let Some(members) = &mut declaration.members
            && let Some(member) = members.first_mut()
        {
            *member = String::new();
        }
        assert_eq!(problem(&emptied), None);
        assert_eq!(
            emit_class(&emptied),
            "classDiagram\n    class A {\n\n        %% 説明\n    }"
        );

        if let Some(ClassItem::Class(declaration)) = emptied.items.first_mut()
            && let Some(members) = &mut declaration.members
            && let Some(member) = members.first_mut()
        {
            *member = "} 後".to_string();
        }
        assert!(
            problem(&emptied).is_some_and(|message| message.contains("メンバー")),
            "a line that starts with `}}` would close the block"
        );
    }

    /// A name field cleared to nothing leaves every statement that named the
    /// class naming one of no name. `ClassDocument::class_names` reports the
    /// classes the picture would hold and so does not list it, which is why the
    /// form checks the statements themselves: writing ` --> B` back would leave
    /// a fence Mermaid draws as a syntax error and this editor cannot reopen.
    #[gpui::test]
    async fn a_class_name_cleared_to_nothing_stops_the_write(cx: &mut TestAppContext) {
        let source = indoc! {"
            ```mermaid
            classDiagramˇ
                動物 <|-- 犬
                note for 動物 \"メモ\"
            ```
        "};
        let (mut cx, modal, _app_state) = open_on(source, cx).await;
        let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let Some(row) = modal.class().and_then(|form| form.names.first()) else {
                    panic!("the diagram names two classes");
                };
                let field = row.field.clone();
                field.update(cx, |field, cx| field.set_text("", window, cx));
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            cx.update_editor(|editor, _window, cx| editor.text(cx)),
            before,
            "a class of no name must not be written back"
        );
        assert_eq!(
            cx.update(|_window, cx| modal.read(cx).message.clone())
                .as_deref(),
            Some("名前が空のクラスがあります。")
        );
    }

    /// A deleted statement has to let go of the fields it held, or a long
    /// editing session leaks one editor per deletion.
    #[gpui::test]
    async fn a_deleted_statement_releases_its_fields(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                classDiagramˇ
                    class A {
                        +x()
                    }
                    class B
                ```
            "},
            cx,
        )
        .await;

        let removed = cx.update(|_window, cx| {
            let Some(ItemRow::Class {
                generic,
                label,
                members,
            }) = modal
                .read(cx)
                .class()
                .and_then(|form| form.body.items.first())
            else {
                panic!("the first statement is a class with a member block");
            };
            let mut fields = vec![generic.downgrade(), label.downgrade()];
            fields.extend(members.iter().map(|field| field.downgrade()));
            fields
        });
        assert_eq!(removed.len(), 3, "the generic, the label and one member");

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.remove_class_item(Vec::new(), 0, window, cx)
            })
        });
        cx.run_until_parked();
        for field in removed {
            field.assert_released();
        }
    }

    /// The style class of a `class A:::warn` statement is carried through an
    /// edit and follows the class it was written on when it is renamed.
    #[gpui::test]
    async fn a_style_class_survives_an_edit_and_a_rename(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                classDiagramˇ
                    classDef warn fill:#f96
                    class 動物:::warn
                    動物 <|-- 犬
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let Some(row) = modal.class().and_then(|form| form.names.first()) else {
                    panic!("the diagram names two classes");
                };
                let field = row.field.clone();
                field.update(cx, |field, cx| field.set_text("生物", window, cx));
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            cx.update_editor(|editor, _window, cx| editor.text(cx)),
            indoc! {"
                ```mermaid
                classDiagram
                    classDef warn fill:#f96
                    class 生物:::warn
                    生物 <|-- 犬
                ```
            "},
        );
    }

    /// The remove button says the classes inside go with the namespace, so the
    /// statements outside that named them have to go too — otherwise Mermaid
    /// draws the class the user just deleted as a box of its own.
    #[gpui::test]
    async fn deleting_a_namespace_takes_the_classes_it_declared(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                classDiagramˇ
                    namespace 外 {
                        namespace 内 {
                            class 犬
                        }
                        class 猫
                    }
                    class 鳥
                    犬 <|-- 柴
                    note for 猫 \"猫のメモ\"
                    note for 鳥 \"鳥のメモ\"
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.remove_class_item(Vec::new(), 0, window, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            cx.update_editor(|editor, _window, cx| editor.text(cx)),
            indoc! {"
                ```mermaid
                classDiagram
                    class 鳥
                    note for 鳥 \"鳥のメモ\"
                ```
            "},
            "the class outside the namespace and its note must be left alone"
        );
    }

    /// Renaming a class inside a namespace must not disturb the statements
    /// around it, which is where a pairing that slipped would show up.
    #[gpui::test]
    async fn renaming_a_class_inside_a_namespace_leaves_its_neighbours_alone(
        cx: &mut TestAppContext,
    ) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                classDiagramˇ
                    namespace 外 {
                        namespace 内 {
                            class 犬[\"いぬ\"]
                        }
                        class 猫[\"ねこ\"]
                    }
                    note for 犬 \"犬のメモ\"
                    note for 猫 \"猫のメモ\"
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let Some(row) = modal
                    .class()
                    .and_then(|form| form.names.iter().find(|row| row.name == "犬"))
                else {
                    panic!("the diagram names 犬");
                };
                let field = row.field.clone();
                field.update(cx, |field, cx| field.set_text("柴", window, cx));
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            cx.update_editor(|editor, _window, cx| editor.text(cx)),
            indoc! {"
                ```mermaid
                classDiagram
                    namespace 外 {
                        namespace 内 {
                            class 柴[\"いぬ\"]
                        }
                        class 猫[\"ねこ\"]
                    }
                    note for 柴 \"犬のメモ\"
                    note for 猫 \"猫のメモ\"
                ```
            "},
        );
    }

    /// A trailing space in a label field is a value the user cannot see, so it
    /// has to be trimmed rather than reported as a value the model refuses.
    #[gpui::test]
    async fn a_space_at_the_end_of_a_relation_label_does_not_stop_the_write(
        cx: &mut TestAppContext,
    ) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                classDiagramˇ
                    動物 <|-- 犬
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let Some(ItemRow::Relation { label, .. }) =
                    modal.class().and_then(|form| form.body.items.first())
                else {
                    panic!("the only statement is the relation");
                };
                let label = label.clone();
                label.update(cx, |label, cx| label.set_text("継承する ", window, cx));
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            cx.update_editor(|editor, _window, cx| editor.text(cx)),
            indoc! {"
                ```mermaid
                classDiagram
                    動物 <|-- 犬 : 継承する
                ```
            "},
        );
        assert_eq!(
            cx.update(|_window, cx| modal.read(cx).message.clone()),
            None
        );
    }

    /// `named_classes` is what stops a relation end cleared to nothing: the
    /// name list is built from `class_names`, which leaves an empty name out,
    /// so checking only that list would let ` <|-- 犬` be written back.
    #[test]
    fn an_empty_or_broken_relation_end_is_a_problem() {
        let mut cleared = document("classDiagram\n  動物 <|-- 犬");
        if let Some(ClassItem::Relation(relation)) = cleared.items.first_mut() {
            relation.left.name = String::new();
        }
        assert!(cleared.class_names().iter().all(|name| !name.is_empty()));
        assert_eq!(
            problem(&cleared).as_deref(),
            Some("名前が空のクラスがあります。")
        );

        let mut generic = document("classDiagram\n  動物 <|-- 犬");
        assert_eq!(problem(&generic), None);
        if let Some(ClassItem::Relation(relation)) = generic.items.first_mut() {
            relation.right.generic = Some("T U".to_string());
        }
        assert_eq!(
            problem(&generic).as_deref(),
            Some("総称型「T U」は使えません。")
        );
    }

    /// The kept statements that address a class follow it: a rename is written
    /// into each of them, and a deleted class takes its `style` / `click` /
    /// `link` / `callback` lines and its entry of a `cssClass` list with it.
    /// The note below them checks that the rows stayed paired with their items.
    #[gpui::test]
    async fn statements_that_address_a_class_follow_a_rename_and_a_deletion(
        cx: &mut TestAppContext,
    ) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                classDiagramˇ
                    class 犬
                    class 猫
                    style 犬 fill:#f96
                    cssClass \"犬, 猫\" warn
                    click 犬 href \"https://example.com\"
                    link 猫 \"https://example.com\"
                    callback 犬 \"handler\"
                    note for 猫 \"猫のメモ\"
                    classDef warn fill:#f96
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let Some(row) = modal
                    .class()
                    .and_then(|form| form.names.iter().find(|row| row.name == "猫"))
                else {
                    panic!("the diagram names 猫");
                };
                let field = row.field.clone();
                field.update(cx, |field, cx| field.set_text("ねこ", window, cx));
                modal.remove_class_item(Vec::new(), 0, window, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            cx.update_editor(|editor, _window, cx| editor.text(cx)),
            indoc! {"
                ```mermaid
                classDiagram
                    class ねこ
                    cssClass \"ねこ\" warn
                    link ねこ \"https://example.com\"
                    note for ねこ \"猫のメモ\"
                    classDef warn fill:#f96
                ```
            "},
        );
    }

    /// A class that leaves the diagram because the last statement naming it
    /// went takes its `style` line along. A `style` line written for a class
    /// the diagram never had is not the form's to judge, and stays.
    #[gpui::test]
    async fn a_class_that_leaves_the_diagram_takes_its_style_line_along(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                classDiagramˇ
                    動物 <|-- 犬
                    class 猫
                    style 犬 fill:#f96
                    style 未定 fill:#69f
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                modal.remove_class_item(Vec::new(), 0, window, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            cx.update_editor(|editor, _window, cx| editor.text(cx)),
            indoc! {"
                ```mermaid
                classDiagram
                    class 猫
                    style 未定 fill:#69f
                ```
            "},
        );
    }

    /// A name field that holds something no class can be called while the form
    /// settles — an item added, another removed — must not be written into
    /// the kept lines: they name a class by its spelling, and one that has
    /// taken `犬 A` or nothing would no longer follow the rename the user
    /// finishes typing afterwards.
    #[gpui::test]
    async fn a_half_typed_name_is_not_written_into_the_kept_lines(cx: &mut TestAppContext) {
        for half_typed in ["犬 A", ""] {
            let (mut cx, modal, _app_state) = open_on(
                indoc! {"
                    ```mermaid
                    classDiagramˇ
                        class 犬
                        class 猫
                        style 犬 fill:#f96
                        cssClass \"犬,猫\" warn
                    ```
                "},
                cx,
            )
            .await;

            cx.update(|window, cx| {
                modal.update(cx, |modal, cx| {
                    let Some(row) = modal
                        .class()
                        .and_then(|form| form.names.iter().find(|row| row.name == "犬"))
                    else {
                        panic!("the diagram names 犬");
                    };
                    let field = row.field.clone();
                    field.update(cx, |field, cx| field.set_text(half_typed, window, cx));
                    modal.add_class_note(Vec::new(), window, cx);
                    let Some(field) = modal.class().and_then(|form| {
                        form.names
                            .iter()
                            .find(|row| row.field.read(cx).text(cx) == half_typed)
                            .map(|row| row.field.clone())
                    }) else {
                        panic!("the half-typed name is still in its field");
                    };
                    field.update(cx, |field, cx| field.set_text("狼", window, cx));
                    modal.remove_class_item(Vec::new(), 4, window, cx);
                    modal.confirm(&menu::SecondaryConfirm, window, cx);
                })
            });

            assert_eq!(
                cx.update_editor(|editor, _window, cx| editor.text(cx)),
                indoc! {"
                    ```mermaid
                    classDiagram
                        class 狼
                        class 猫
                        style 狼 fill:#f96
                        cssClass \"狼,猫\" warn
                    ```
                "},
                "after {half_typed:?}",
            );
        }
    }

    /// The `~T~` at each end of a relation is edited in its own field, and a
    /// rename of the class keeps it.
    #[gpui::test]
    async fn the_generic_at_a_relation_end_is_edited_and_survives_a_rename(
        cx: &mut TestAppContext,
    ) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                classDiagramˇ
                    リスト~T~ --> 要素
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let Some(ItemRow::Relation {
                    left_generic,
                    right_generic,
                    ..
                }) = modal.class().and_then(|form| form.body.items.first())
                else {
                    panic!("the only statement is the relation");
                };
                assert_eq!(left_generic.read(cx).text(cx), "T");
                let (left_generic, right_generic) = (left_generic.clone(), right_generic.clone());
                left_generic.update(cx, |field, cx| field.set_text("U", window, cx));
                right_generic.update(cx, |field, cx| field.set_text("int", window, cx));
                let Some(row) = modal.class().and_then(|form| form.names.first()) else {
                    panic!("the diagram names two classes");
                };
                let field = row.field.clone();
                field.update(cx, |field, cx| field.set_text("一覧", window, cx));
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            cx.update_editor(|editor, _window, cx| editor.text(cx)),
            indoc! {"
                ```mermaid
                classDiagram
                    一覧~U~ --> 要素~int~
                ```
            "},
        );
    }

    /// Comments at the end of a statement and blank lines inside a member
    /// block are carried through an edit on their own lines.
    #[gpui::test]
    async fn end_of_line_comments_and_blank_member_lines_survive_an_edit(cx: &mut TestAppContext) {
        let (mut cx, modal, _app_state) = open_on(
            indoc! {"
                ```mermaid
                classDiagramˇ
                    class 動物 { %% 基底
                        +名前 string

                        +鳴く() void
                    }
                    動物 <|-- 犬 %% 継承
                    note for 動物 \"メモ\" %% 補足
                ```
            "},
            cx,
        )
        .await;

        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                let Some(row) = modal.class().and_then(|form| form.names.first()) else {
                    panic!("the diagram names two classes");
                };
                let field = row.field.clone();
                field.update(cx, |field, cx| field.set_text("生物", window, cx));
                modal.confirm(&menu::SecondaryConfirm, window, cx);
            })
        });

        assert_eq!(
            cx.update_editor(|editor, _window, cx| editor.text(cx)),
            indoc! {"
                ```mermaid
                classDiagram
                    class 生物 { %% 基底
                        +名前 string

                        +鳴く() void
                    }
                    生物 <|-- 犬 %% 継承
                    note for 生物 \"メモ\" %% 補足
                ```
            "},
        );
    }

    fn init_test(cx: &mut TestAppContext) -> Arc<workspace::AppState> {
        cx.update(|cx| {
            let state = workspace::AppState::test(cx);
            editor::init(cx);
            crate::init(cx);
            state
        })
    }

    fn markdown_language() -> Arc<language::Language> {
        Arc::new(language::Language::new(
            language::LanguageConfig {
                name: "Markdown".into(),
                ..Default::default()
            },
            None,
        ))
    }

    /// An editor holding `state` with the modal opened on the fence the cursor
    /// marker sits in.
    async fn open_on(
        state: &str,
        cx: &mut TestAppContext,
    ) -> (
        EditorTestContext,
        Entity<MermaidEditor>,
        Arc<workspace::AppState>,
    ) {
        let app_state = init_test(cx);
        let mut cx = EditorTestContext::new(cx).await;
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(markdown_language()), cx));
        cx.set_state(state);

        let editor = cx.editor.clone();
        let target = cx
            .update_editor(|editor, _window, cx| editor.mermaid_fence_target(cx))
            .expect("the cursor is inside a mermaid fence");
        let document = parse_document(&target.source)
            .unwrap_or_else(|error| panic!("the fence should open: {error}"));
        let modal = cx.update(|window, cx| {
            cx.new(|cx| MermaidEditor::new(editor.downgrade(), target, Some(document), window, cx))
        });
        (cx, modal, app_state)
    }
}
