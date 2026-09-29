//! The state diagram half of the Mermaid editor's form.
//!
//! Two things shape this form. First, a state diagram is not two lists — one of
//! states, one of transitions — even though it reads like one: the `--` divider
//! splits the statements around it into concurrent regions, and a comment has to
//! stay where it was written. So the statements are shown as one ordered list
//! per scope, the way the fence has them, and a composite state opens a nested
//! list of the same shape.
//!
//! Second, a state does not have to be declared: `[*] --> 待機` names 待機 into
//! existence. There is therefore no one statement whose id could be edited to
//! rename it, and the ids get a list of their own above the statements. Editing
//! one there carries into every statement that names it.

use std::ops::Range;

use editor::Editor;
use gpui::{AnyElement, App, Context, Entity, Window};
use mermaid_render::flowchart_model::FlowDirection;
use mermaid_render::state_model::{
    CompositeState, NotePlacement, StateDocument, StateEntry, StateItem, StateKind, StateNote,
    StateScope, StateTransition, TERMINAL_STATE, is_valid_note_text, is_valid_state_id,
    is_valid_style_class,
};
use ui::{ContextMenu, DropdownMenu, DropdownStyle, IconPosition, prelude::*};
use util::ResultExt as _;

use super::class_menu::{
    ToggleClass, class_candidates, declared_class_names, render_class_input, render_class_menu,
};
use super::{
    Form, ID_FIELD_WIDTH, LABEL_FIELD_WIDTH, MermaidEditor, add_button, field_frame, neighbour,
    row_controls_labelled, section_title,
};

/// How deeply the form draws composite states. The model refuses to parse
/// deeper than this, so a diagram that reaches the limit cannot be open here.
const MAX_DEPTH: usize = 16;

pub(super) struct StateForm {
    pub(super) document: StateDocument,
    /// One row per state id the diagram names, in the order it first appears.
    pub(super) ids: Vec<IdRow>,
    pub(super) body: ScopeRows,
    /// A class name typed by hand, offered beside the `classDef` names in
    /// every state's class menu.
    pub(super) class_input: Entity<Editor>,
}

/// The id of one state and the field it is edited in. `name` is what the
/// document calls it now, so that a field that has moved away from it is a
/// rename.
pub(super) struct IdRow {
    pub(super) name: String,
    pub(super) field: Entity<Editor>,
}

/// The text fields of one scope, in the same order as its items.
#[derive(Default)]
pub(super) struct ScopeRows {
    pub(super) items: Vec<ItemRow>,
}

pub(super) enum ItemRow {
    State {
        description: Entity<Editor>,
    },
    Transition {
        label: Entity<Editor>,
    },
    Composite {
        description: Entity<Editor>,
        body: ScopeRows,
    },
    Note {
        text: Entity<Editor>,
    },
    /// A divider or a line held verbatim: nothing to type into.
    Fixed,
}

impl StateForm {
    pub(super) fn fields(&self) -> Vec<Entity<Editor>> {
        let mut fields = self
            .ids
            .iter()
            .map(|row| row.field.clone())
            .collect::<Vec<_>>();
        fields.push(self.class_input.clone());
        collect_fields(&self.body, &mut fields);
        fields
    }

    /// The class names the state class menus offer.
    pub(super) fn class_candidates(&self, cx: &App) -> Vec<String> {
        let mut lines = Vec::new();
        collect_preserved(&self.document.body, &mut lines);
        let declared = declared_class_names(lines, is_valid_style_class);
        let mut assigned = Vec::new();
        for id in self.document.state_ids() {
            state_classes(&self.document.body, &id, &mut assigned);
        }
        class_candidates(
            declared,
            &assigned,
            &self.class_input.read(cx).text(cx),
            is_valid_style_class,
        )
    }

    /// The diagram the form currently describes: the structure held in
    /// `document` with the text fields' contents written over it, and every
    /// reference to a renamed state carried along.
    pub(super) fn current_document(&self, cx: &App) -> StateDocument {
        let mut document = self.document.clone();
        apply_fields(&mut document.body, &self.body, cx);

        let renames = self
            .ids
            .iter()
            .filter_map(|row| {
                let typed = row.field.read(cx).text(cx).trim().to_string();
                (typed != row.name).then(|| (row.name.clone(), typed))
            })
            .collect::<Vec<_>>();
        if !renames.is_empty() {
            apply_renames(&mut document.body, &renames);
        }
        document
    }
}

fn collect_fields(rows: &ScopeRows, fields: &mut Vec<Entity<Editor>>) {
    for row in &rows.items {
        match row {
            ItemRow::State { description } => fields.push(description.clone()),
            ItemRow::Transition { label } => fields.push(label.clone()),
            ItemRow::Composite { description, body } => {
                fields.push(description.clone());
                collect_fields(body, fields);
            }
            ItemRow::Note { text } => fields.push(text.clone()),
            ItemRow::Fixed => {}
        }
    }
}

/// Writes the fields of `rows` onto `scope`. Ids are not among them: they are
/// edited in the list above the statements.
fn apply_fields(scope: &mut StateScope, rows: &ScopeRows, cx: &App) {
    for (item, row) in scope.items.iter_mut().zip(&rows.items) {
        match (item, row) {
            (StateItem::State(entry), ItemRow::State { description }) => {
                let description = description.read(cx).text(cx);
                entry.description = (!description.is_empty()).then_some(description);
            }
            (StateItem::Transition(transition), ItemRow::Transition { label }) => {
                let label = label.read(cx).text(cx);
                transition.label = (!label.is_empty()).then_some(label);
            }
            (StateItem::Composite(composite), ItemRow::Composite { description, body }) => {
                let description = description.read(cx).text(cx);
                composite.description = (!description.is_empty()).then_some(description);
                apply_fields(&mut composite.body, body, cx);
            }
            (StateItem::Note(note), ItemRow::Note { text }) => {
                note.text = text.read(cx).text(cx);
            }
            _ => {}
        }
    }
}

/// Points everything that names a state at its new id.
///
/// A transition or a note left on the old id would name a state of its own,
/// which Mermaid draws as an extra, empty box.
fn apply_renames(scope: &mut StateScope, renames: &[(String, String)]) {
    let renamed = |id: &str| {
        renames
            .iter()
            .find(|(old, _)| old == id)
            .map(|(_, new)| new.clone())
    };
    for item in &mut scope.items {
        match item {
            StateItem::State(entry) => {
                if let Some(id) = renamed(&entry.id) {
                    entry.id = id;
                }
            }
            StateItem::Transition(transition) => {
                if let Some(id) = renamed(&transition.from) {
                    transition.from = id;
                }
                if let Some(id) = renamed(&transition.to) {
                    transition.to = id;
                }
            }
            StateItem::Note(note) => {
                if let Some(id) = renamed(&note.target) {
                    note.target = id;
                }
            }
            StateItem::Composite(composite) => {
                if let Some(id) = renamed(&composite.id) {
                    composite.id = id;
                }
                apply_renames(&mut composite.body, renames);
            }
            StateItem::Preserved(text) => retarget_preserved(text, renames),
            StateItem::Divider => {}
        }
    }
}

/// Where in `line` the states a statement the model keeps verbatim addresses
/// are written.
///
/// `class` and `style` take a comma separated list — Mermaid's lexer walks ids
/// across the commas and the spaces around them — and `click` takes one id, so
/// one span covers all three. The span is returned rather than the ids so that
/// a rename can be written into the line without disturbing the rest of it.
fn preserved_target_span(line: &str) -> Option<Range<usize>> {
    const TARGETED: [&str; 3] = ["class", "style", "click"];
    let indent = line.len() - line.trim_start().len();
    let trimmed = line.get(indent..)?;
    let keyword = trimmed.split_whitespace().next()?;
    if !TARGETED.contains(&keyword) {
        return None;
    }
    let after_keyword = trimmed.get(keyword.len()..)?;
    let leading = after_keyword.len() - after_keyword.trim_start().len();
    let start = indent + keyword.len() + leading;
    let length = id_list_length(after_keyword.trim_start());
    (length > 0).then_some(start..start + length)
}

/// How much of `rest` is the comma separated list of ids it opens with: an id,
/// then a comma and the id after it for as long as one follows. The whitespace
/// after the last id is left out, so the span covers the list and nothing else.
fn id_list_length(rest: &str) -> usize {
    let mut end = 0;
    let mut cursor = 0;
    while let Some(remainder) = rest.get(cursor..) {
        let id_length = remainder
            .find(|character: char| character.is_whitespace() || character == ',')
            .unwrap_or(remainder.len());
        if id_length == 0 {
            break;
        }
        cursor += id_length;
        end = cursor;
        let after_id = rest.get(cursor..).unwrap_or_default();
        let spaces = after_id.len() - after_id.trim_start().len();
        let Some(after_comma) = after_id.trim_start().strip_prefix(',') else {
            break;
        };
        cursor += spaces + 1 + after_comma.len() - after_comma.trim_start().len();
    }
    end
}

/// Points a statement the model keeps verbatim at the new spelling of every
/// state it addresses, one entry of its list at a time so that the separators
/// and the spaces around them stay as they were written.
fn retarget_preserved(line: &mut String, renames: &[(String, String)]) {
    let Some(span) = preserved_target_span(line) else {
        return;
    };
    let Some((rewritten, changed)) = line
        .get(span.clone())
        .map(|list| rewrite_id_list(list, renames))
    else {
        return;
    };
    if changed {
        line.replace_range(span, &rewritten);
    }
}

/// `list` with every id a rename names replaced, and whether any was.
fn rewrite_id_list(list: &str, renames: &[(String, String)]) -> (String, bool) {
    let mut rewritten = String::with_capacity(list.len());
    let mut changed = false;
    for (index, entry) in list.split(',').enumerate() {
        if index > 0 {
            rewritten.push(',');
        }
        let id = entry.trim();
        // A whole entry, so that renaming `A` leaves `AB` alone.
        match renames.iter().find(|(old, _)| old == id) {
            Some((_, new)) => {
                let leading = entry.len() - entry.trim_start().len();
                rewritten.push_str(entry.get(..leading).unwrap_or_default());
                rewritten.push_str(new);
                rewritten.push_str(entry.get(leading + id.len()..).unwrap_or_default());
                changed = true;
            }
            None => rewritten.push_str(entry),
        }
    }
    (rewritten, changed)
}

/// The ids a kept statement addresses once `id` is taken out of its list, or
/// `None` when `line` is not such a statement or does not address `id` at all.
/// An empty result means the statement has nothing left to be about.
fn preserved_without(line: &str, id: &str) -> Option<(Range<usize>, String)> {
    let span = preserved_target_span(line)?;
    let list = line.get(span.clone())?;
    if !list.split(',').any(|entry| entry.trim() == id) {
        return None;
    }
    let kept = list
        .split(',')
        .filter(|entry| entry.trim() != id)
        .collect::<Vec<_>>()
        .join(",");
    Some((span, kept.trim().to_string()))
}

fn collect_preserved<'a>(scope: &'a StateScope, lines: &mut Vec<&'a str>) {
    for item in &scope.items {
        match item {
            StateItem::Preserved(text) => lines.push(text),
            StateItem::Composite(composite) => collect_preserved(&composite.body, lines),
            _ => {}
        }
    }
}

/// The states and the class of a `class <ids> <class>` statement, or `None`
/// when `line` is not one.
fn class_statement(line: &str) -> Option<(Vec<&str>, &str)> {
    if line.split_whitespace().next()? != "class" {
        return None;
    }
    let span = preserved_target_span(line)?;
    let ids = line
        .get(span.clone())?
        .split(',')
        .map(str::trim)
        .collect::<Vec<_>>();
    let class = line
        .get(span.end..)?
        .split_whitespace()
        .next()?
        .trim_end_matches(';');
    (!class.is_empty()).then_some((ids, class))
}

/// Every class `id` has, in the order the fence gives them: the `:::` on a
/// statement that names it, and the `class` statements whose list names it.
pub(super) fn state_classes(scope: &StateScope, id: &str, classes: &mut Vec<String>) {
    fn push(classes: &mut Vec<String>, class: &str) {
        if !classes.iter().any(|known| known == class) {
            classes.push(class.to_string());
        }
    }
    for item in &scope.items {
        match item {
            StateItem::State(entry) if entry.id == id => {
                if let Some(class) = &entry.style_class {
                    push(classes, class);
                }
            }
            StateItem::Transition(transition) => {
                for (end, class) in [
                    (&transition.from, &transition.from_style_class),
                    (&transition.to, &transition.to_style_class),
                ] {
                    if end == id
                        && let Some(class) = class
                    {
                        push(classes, class);
                    }
                }
            }
            StateItem::Composite(composite) => state_classes(&composite.body, id, classes),
            StateItem::Preserved(text) => {
                if let Some((ids, class)) = class_statement(text)
                    && ids.contains(&id)
                {
                    push(classes, class);
                }
            }
            StateItem::State(_) | StateItem::Note(_) | StateItem::Divider => {}
        }
    }
}

/// Takes `class` away from `id` wherever the fence gives it: the `:::` goes
/// from every statement that names the state, and the state goes from the list
/// of every `class` statement for it — the statement with it, once the list
/// is empty, together with its row.
fn remove_state_class(scope: &mut StateScope, rows: &mut ScopeRows, id: &str, class: &str) {
    for index in (0..scope.items.len()).rev() {
        let Some(item) = scope.items.get_mut(index) else {
            continue;
        };
        let mut drop_item = false;
        match item {
            StateItem::State(entry) => {
                if entry.id == id && entry.style_class.as_deref() == Some(class) {
                    entry.style_class = None;
                }
            }
            StateItem::Transition(transition) => {
                if transition.from == id && transition.from_style_class.as_deref() == Some(class) {
                    transition.from_style_class = None;
                }
                if transition.to == id && transition.to_style_class.as_deref() == Some(class) {
                    transition.to_style_class = None;
                }
            }
            StateItem::Composite(composite) => match rows.items.get_mut(index) {
                Some(ItemRow::Composite { body, .. }) => {
                    remove_state_class(&mut composite.body, body, id, class)
                }
                _ => remove_state_class(&mut composite.body, &mut ScopeRows::default(), id, class),
            },
            StateItem::Preserved(text) => {
                let names_it = class_statement(text)
                    .is_some_and(|(ids, named)| named == class && ids.contains(&id));
                if names_it {
                    match preserved_without(text, id) {
                        Some((_, kept)) if kept.is_empty() => drop_item = true,
                        Some((span, kept)) => text.replace_range(span, &kept),
                        None => {}
                    }
                }
            }
            StateItem::Note(_) | StateItem::Divider => {}
        }
        if drop_item {
            scope.items.remove(index);
            if index < rows.items.len() {
                rows.items.remove(index);
            }
        }
    }
}

/// The first thing wrong with what the user typed, in the words shown above the
/// form.
pub(super) fn problem(document: &StateDocument) -> Option<SharedString> {
    for id in document.state_ids() {
        if id.is_empty() {
            return Some(i18n::t!("A state has an empty ID.").into());
        }
        if !is_valid_state_id(&id) {
            return Some(
                i18n::t!("The ID \"{id}\" is not valid (no symbols or Mermaid keywords).").into(),
            );
        }
    }
    let mut declared: Vec<String> = Vec::new();
    scope_problem(&document.body, &mut declared)
}

fn scope_problem(scope: &StateScope, declared: &mut Vec<String>) -> Option<SharedString> {
    for item in &scope.items {
        let id = match item {
            StateItem::State(entry) => Some(&entry.id),
            StateItem::Composite(composite) => Some(&composite.id),
            _ => None,
        };
        if let Some(id) = id {
            if declared.contains(id) {
                return Some(i18n::t!("The ID \"{id}\" is used more than once.").into());
            }
            declared.push(id.clone());
        }
        if let StateItem::Note(note) = item
            && !is_valid_note_text(&note.text)
        {
            return Some(
                i18n::t!(
                    "The note text contains characters that are not allowed (leading or trailing whitespace, line breaks, or `%%{{`)."
                )
                .into(),
            );
        }
        if let StateItem::Composite(composite) = item
            && let Some(problem) = scope_problem(&composite.body, declared)
        {
            return Some(problem);
        }
    }
    None
}

/// The scope a path addresses: each element steps into the composite state at
/// that index.
fn scope_at<'a>(scope: &'a mut StateScope, path: &[usize]) -> Option<&'a mut StateScope> {
    let Some((index, rest)) = path.split_first() else {
        return Some(scope);
    };
    match scope.items.get_mut(*index)? {
        StateItem::Composite(composite) => scope_at(&mut composite.body, rest),
        _ => None,
    }
}

fn rows_at<'a>(rows: &'a mut ScopeRows, path: &[usize]) -> Option<&'a mut ScopeRows> {
    let Some((index, rest)) = path.split_first() else {
        return Some(rows);
    };
    match rows.items.get_mut(*index)? {
        ItemRow::Composite { body, .. } => rows_at(body, rest),
        _ => None,
    }
}

/// Drops every statement that names `id`, so that a deleted state does not come
/// back as an empty box under one of them.
fn forget_state(scope: &mut StateScope, id: &str) {
    scope.items.retain(|item| !names_state(item, id));
    for item in &mut scope.items {
        match item {
            // A statement that addressed other states as well survives; this
            // one still has to come out of the list it addresses.
            StateItem::Preserved(text) => {
                if let Some((span, kept)) = preserved_without(text, id) {
                    text.replace_range(span, &kept);
                }
            }
            StateItem::Composite(composite) => forget_state(&mut composite.body, id),
            _ => {}
        }
    }
}

/// The rows [`forget_state`] would leave behind, so that the fields held beside
/// the items go with them. Runs before it, on the scope it is about to change.
fn forget_state_rows(scope: &StateScope, rows: &mut ScopeRows, id: &str) {
    // The nested scopes are paired with their rows by position, and thinning
    // this scope's rows out first would pair them with the wrong items: a
    // composite would then keep rows for statements `forget_state` is about to
    // drop, and every field below them would be read onto its neighbour.
    for (item, row) in scope.items.iter().zip(&mut rows.items) {
        if let (StateItem::Composite(composite), ItemRow::Composite { body, .. }) = (item, row) {
            forget_state_rows(&composite.body, body, id);
        }
    }
    let mut items = scope.items.iter();
    rows.items
        .retain(|_| items.next().is_none_or(|item| !names_state(item, id)));
}

fn names_state(item: &StateItem, id: &str) -> bool {
    match item {
        StateItem::State(entry) => entry.id == id,
        StateItem::Transition(transition) => transition.from == id || transition.to == id,
        StateItem::Note(note) => note.target == id,
        // Only a statement left addressing nothing at all goes; one that names
        // other states keeps its place and loses this id from its list.
        StateItem::Preserved(text) => {
            preserved_without(text, id).is_some_and(|(_, kept)| kept.is_empty())
        }
        StateItem::Composite(_) | StateItem::Divider => false,
    }
}

impl MermaidEditor {
    pub(super) fn new_state_form(
        &mut self,
        document: StateDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> StateForm {
        let body = self.new_scope_rows(&document.body, window, cx);
        let ids = self.new_id_rows(&document.state_ids(), window, cx);
        StateForm {
            document,
            ids,
            body,
            class_input: self.new_field("", window, cx),
        }
    }

    fn new_id_rows(
        &mut self,
        ids: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<IdRow> {
        ids.iter()
            .map(|id| {
                let id = id.clone();
                IdRow {
                    field: self.new_field(&id, window, cx),
                    name: id,
                }
            })
            .collect()
    }

    fn new_scope_rows(
        &mut self,
        scope: &StateScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ScopeRows {
        let mut items = Vec::with_capacity(scope.items.len());
        for item in &scope.items {
            items.push(match item {
                StateItem::State(entry) => {
                    let description = entry.description.clone().unwrap_or_default();
                    ItemRow::State {
                        description: self.new_field(&description, window, cx),
                    }
                }
                StateItem::Transition(transition) => {
                    let label = transition.label.clone().unwrap_or_default();
                    ItemRow::Transition {
                        label: self.new_field(&label, window, cx),
                    }
                }
                StateItem::Composite(composite) => {
                    let description = composite.description.clone().unwrap_or_default();
                    ItemRow::Composite {
                        description: self.new_field(&description, window, cx),
                        body: self.new_scope_rows(&composite.body, window, cx),
                    }
                }
                StateItem::Note(note) => {
                    let text = note.text.clone();
                    ItemRow::Note {
                        text: self.new_field(&text, window, cx),
                    }
                }
                StateItem::Divider | StateItem::Preserved(_) => ItemRow::Fixed,
            });
        }
        ScopeRows { items }
    }

    pub(super) fn state(&self) -> Option<&StateForm> {
        match &self.form {
            Form::State(form) => Some(form),
            _ => None,
        }
    }

    pub(super) fn state_mut(&mut self) -> Option<&mut StateForm> {
        match &mut self.form {
            Form::State(form) => Some(form),
            _ => None,
        }
    }

    /// Writes the fields into the document and rebuilds the id list from it.
    ///
    /// A statement that was added or removed may have brought a state into the
    /// diagram or taken the last mention of one away, and the id list is where
    /// states are renamed, so it has to follow.
    fn settle_state(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.state() else {
            return;
        };
        let document = form.current_document(cx);
        let ids = document.state_ids();
        let unchanged = form.ids.len() == ids.len()
            && form
                .ids
                .iter()
                .zip(&ids)
                .all(|(row, id)| row.name == *id && row.field.read(cx).text(cx).trim() == id);
        let rows = (!unchanged).then(|| self.new_id_rows(&ids, window, cx));
        let Some(form) = self.state_mut() else {
            return;
        };
        if let Some(rows) = rows {
            form.ids = rows;
        }
        form.document = document;
        self.schedule_preview(cx);
        cx.notify();
    }

    /// Adds `item` with its `row` to the end of the scope `path` addresses.
    fn push_state_item(
        &mut self,
        path: &[usize],
        item: StateItem,
        row: ItemRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        {
            let Some(form) = self.state_mut() else {
                return;
            };
            let (Some(scope), Some(rows)) = (
                scope_at(&mut form.document.body, path),
                rows_at(&mut form.body, path),
            ) else {
                return;
            };
            scope.items.push(item);
            rows.items.push(row);
        }
        self.settle_state(window, cx);
    }

    pub(super) fn add_state(
        &mut self,
        path: Vec<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.state() else {
            return;
        };
        // Not "state" in any case: Mermaid reads that word as the start of a
        // statement, so `is_valid_state_id` refuses it.
        let id = form.current_document(cx).unused_id(i18n::t!("NewState"));
        let row = ItemRow::State {
            description: self.new_field("", window, cx),
        };
        self.push_state_item(
            &path,
            StateItem::State(StateEntry {
                id,
                description: None,
                kind: StateKind::Normal,
                style_class: None,
            }),
            row,
            window,
            cx,
        );
    }

    /// Adds `class` to the state `id`, or takes it away when the state already
    /// has it. A class the fence gives with `:::` keeps that spelling until it
    /// is taken away; a new one is written as a `class` statement at the end
    /// of the diagram, which reaches a state inside a composite state too.
    pub(super) fn toggle_state_class(
        &mut self,
        id: &str,
        class: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        {
            let Some(form) = self.state_mut() else {
                return;
            };
            let mut assigned = Vec::new();
            state_classes(&form.document.body, id, &mut assigned);
            if assigned.iter().any(|name| name == class) {
                remove_state_class(&mut form.document.body, &mut form.body, id, class);
            } else {
                form.document
                    .body
                    .items
                    .push(StateItem::Preserved(format!("    class {id} {class}")));
                form.body.items.push(ItemRow::Fixed);
            }
        }
        self.settle_state(window, cx);
    }

    fn add_transition(&mut self, path: Vec<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.state() else {
            return;
        };
        let ids = form.current_document(cx).state_ids();
        let from = ids
            .first()
            .cloned()
            .unwrap_or_else(|| TERMINAL_STATE.to_string());
        let to = ids
            .get(1)
            .cloned()
            .unwrap_or_else(|| TERMINAL_STATE.to_string());
        let row = ItemRow::Transition {
            label: self.new_field("", window, cx),
        };
        self.push_state_item(
            &path,
            StateItem::Transition(StateTransition {
                from,
                from_style_class: None,
                to,
                to_style_class: None,
                label: None,
            }),
            row,
            window,
            cx,
        );
    }

    fn add_note(&mut self, path: Vec<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.state() else {
            return;
        };
        let Some(target) = form.current_document(cx).state_ids().first().cloned() else {
            self.message = Some(i18n::t!("Add a state first.").into());
            cx.notify();
            return;
        };
        let text = i18n::t!("Memo");
        let row = ItemRow::Note {
            text: self.new_field(text, window, cx),
        };
        self.push_state_item(
            &path,
            StateItem::Note(StateNote {
                placement: NotePlacement::RightOf,
                target,
                text: text.to_string(),
            }),
            row,
            window,
            cx,
        );
    }

    fn add_composite(&mut self, path: Vec<usize>, window: &mut Window, cx: &mut Context<Self>) {
        if path.len() + 1 >= MAX_DEPTH {
            self.message = Some(i18n::t!("Composite states are nested too deeply.").into());
            cx.notify();
            return;
        }
        let Some(form) = self.state() else {
            return;
        };
        let id = form
            .current_document(cx)
            .unused_id(i18n::t!("CompositeState"));
        let row = ItemRow::Composite {
            description: self.new_field("", window, cx),
            body: ScopeRows::default(),
        };
        self.push_state_item(
            &path,
            StateItem::Composite(CompositeState {
                id,
                description: None,
                body: StateScope::default(),
            }),
            row,
            window,
            cx,
        );
    }

    fn add_divider(&mut self, path: Vec<usize>, window: &mut Window, cx: &mut Context<Self>) {
        self.push_state_item(&path, StateItem::Divider, ItemRow::Fixed, window, cx);
    }

    pub(super) fn remove_state_item(
        &mut self,
        path: Vec<usize>,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        {
            let Some(form) = self.state_mut() else {
                return;
            };
            let (Some(scope), Some(rows)) = (
                scope_at(&mut form.document.body, &path),
                rows_at(&mut form.body, &path),
            ) else {
                return;
            };
            if index >= scope.items.len() || index >= rows.items.len() {
                return;
            }
            let removed = scope.items.remove(index);
            rows.items.remove(index);
            // Everything that still named the state would name it back into
            // the diagram as an empty box.
            let orphaned = match &removed {
                StateItem::State(entry) => Some(entry.id.clone()),
                StateItem::Composite(composite) => Some(composite.id.clone()),
                _ => None,
            };
            if let Some(id) = orphaned {
                forget_state_rows(&form.document.body, &mut form.body, &id);
                forget_state(&mut form.document.body, &id);
            }
        }
        self.settle_state(window, cx);
    }

    fn move_state_item(
        &mut self,
        path: Vec<usize>,
        index: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.state_mut() else {
            return;
        };
        let (Some(scope), Some(rows)) = (
            scope_at(&mut form.document.body, &path),
            rows_at(&mut form.body, &path),
        ) else {
            return;
        };
        let Some(other) = neighbour(index, forward, scope.items.len()) else {
            return;
        };
        if other >= rows.items.len() || index >= rows.items.len() {
            return;
        }
        scope.items.swap(index, other);
        rows.items.swap(index, other);
        self.schedule_preview(cx);
        cx.notify();
    }

    /// Changes one item through `edit`, which sees the item the path and index
    /// address.
    fn update_state_item(
        &mut self,
        path: &[usize],
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut StateItem),
    ) {
        {
            let Some(form) = self.state_mut() else {
                return;
            };
            let Some(scope) = scope_at(&mut form.document.body, path) else {
                return;
            };
            let Some(item) = scope.items.get_mut(index) else {
                return;
            };
            edit(item);
        }
        self.settle_state(window, cx);
    }

    fn set_scope_direction(
        &mut self,
        path: &[usize],
        direction: Option<FlowDirection>,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.state_mut() else {
            return;
        };
        let Some(scope) = scope_at(&mut form.document.body, path) else {
            return;
        };
        scope.direction = direction;
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn render_state_toolbar(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(form) = self.state() else {
            return div().into_any_element();
        };
        self.render_scope_direction(&[], form.document.body.direction, window, cx)
    }

    fn render_scope_direction(
        &self,
        path: &[usize],
        current: Option<FlowDirection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let owned_path = path.to_vec();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for choice in [None].into_iter().chain(FlowDirection::ALL.map(Some)) {
                let this = this.clone();
                let path = owned_path.clone();
                menu = menu.toggleable_entry(
                    direction_label(choice),
                    choice == current,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        let path = path.clone();
                        this.update(cx, |this, cx| this.set_scope_direction(&path, choice, cx))
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
                    SharedString::from(format!("mermaid-state-direction-{}", path_key(path))),
                    direction_label(current),
                    menu,
                )
                .style(DropdownStyle::Outlined)
                .trigger_size(ButtonSize::Compact),
            )
            .into_any_element()
    }

    pub(super) fn render_state(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(form) = self.state() else {
            return Vec::new();
        };
        let ids = form
            .ids
            .iter()
            .map(|row| row.field.read(cx).text(cx).trim().to_string())
            .collect::<Vec<_>>();

        let candidates = form.class_candidates(cx);
        let this = cx.entity().downgrade();
        let mut id_list = v_flex().w_full().gap_1().child(section_title(i18n::t!(
            "State IDs (renaming updates the references too)"
        )));
        for (index, row) in form.ids.iter().enumerate() {
            let valid = is_valid_state_id(row.field.read(cx).text(cx).trim());
            let mut assigned = Vec::new();
            state_classes(&form.document.body, &row.name, &mut assigned);
            let on_toggle: ToggleClass = {
                let this = this.clone();
                let id = row.name.clone();
                std::rc::Rc::new(move |class, window, cx| {
                    this.update(cx, |this, cx| {
                        this.toggle_state_class(&id, &class, window, cx)
                    })
                    .log_err();
                })
            };
            id_list = id_list.child(
                h_flex()
                    .gap_1()
                    .child(
                        field_frame(&row.field, ID_FIELD_WIDTH, valid, window, cx)
                            .child(row.field.clone()),
                    )
                    .child(render_class_menu(
                        ("mermaid-state-class", index),
                        &assigned,
                        &candidates,
                        on_toggle,
                        window,
                        cx,
                    )),
            );
        }
        id_list = id_list.child(render_class_input(
            &form.class_input,
            is_valid_style_class,
            window,
            cx,
        ));

        vec![
            id_list.into_any_element(),
            v_flex()
                .w_full()
                .gap_1()
                .child(section_title(i18n::t!("Diagram Content")))
                .child(self.render_scope(&[], &form.document.body, &form.body, &ids, window, cx))
                .into_any_element(),
        ]
    }

    fn render_scope(
        &self,
        path: &[usize],
        scope: &StateScope,
        rows: &ScopeRows,
        ids: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = path_key(path);
        let row_prefix = format!("mermaid-state-{key}");
        let last = scope.items.len().saturating_sub(1);
        let mut list = v_flex().w_full().gap_1();

        for index in 0..scope.items.len() {
            let (Some(item), Some(row)) = (scope.items.get(index), rows.items.get(index)) else {
                continue;
            };
            let controls = {
                let (up, down, remove) = (path.to_vec(), path.to_vec(), path.to_vec());
                // Deleting a composite state takes the whole scope inside it,
                // which the row does not otherwise show.
                let remove_tooltip = match item {
                    StateItem::Composite(_) => {
                        i18n::t!("Delete (the states and transitions inside are deleted too)")
                    }
                    _ => i18n::t!("Delete"),
                };
                row_controls_labelled(
                    &row_prefix,
                    index,
                    index == 0,
                    index >= last,
                    remove_tooltip,
                    cx.listener(move |this, _, _window, cx| {
                        this.move_state_item(up.clone(), index, false, cx)
                    }),
                    cx.listener(move |this, _, _window, cx| {
                        this.move_state_item(down.clone(), index, true, cx)
                    }),
                    cx.listener(move |this, _, window, cx| {
                        this.remove_state_item(remove.clone(), index, window, cx)
                    }),
                )
            };

            list = match (item, row) {
                (StateItem::State(entry), ItemRow::State { description }) => list.child(
                    h_flex()
                        .w_full()
                        .gap_1()
                        .child(id_label(&entry.id))
                        .child(
                            field_frame(description, LABEL_FIELD_WIDTH, true, window, cx)
                                .flex_1()
                                .child(description.clone()),
                        )
                        .child(self.render_state_kind_menu(path, index, entry.kind, window, cx))
                        .child(controls),
                ),
                (StateItem::Transition(transition), ItemRow::Transition { label }) => list.child(
                    h_flex()
                        .w_full()
                        .gap_1()
                        .child(self.render_endpoint(
                            path,
                            index,
                            true,
                            &transition.from,
                            ids,
                            window,
                            cx,
                        ))
                        .child(Label::new("→").size(LabelSize::Small).color(Color::Muted))
                        .child(self.render_endpoint(
                            path,
                            index,
                            false,
                            &transition.to,
                            ids,
                            window,
                            cx,
                        ))
                        .child(
                            field_frame(label, LABEL_FIELD_WIDTH, true, window, cx)
                                .flex_1()
                                .child(label.clone()),
                        )
                        .child(controls),
                ),
                (StateItem::Note(note), ItemRow::Note { text }) => list.child(
                    h_flex()
                        .w_full()
                        .gap_1()
                        .child(
                            Label::new(i18n::t!("Note"))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(self.render_note_placement(path, index, note.placement, window, cx))
                        .child(self.render_note_target(path, index, &note.target, ids, window, cx))
                        .child(
                            field_frame(
                                text,
                                LABEL_FIELD_WIDTH,
                                is_valid_note_text(&text.read(cx).text(cx)),
                                window,
                                cx,
                            )
                            .flex_1()
                            .child(text.clone()),
                        )
                        .child(controls),
                ),
                (StateItem::Composite(composite), ItemRow::Composite { description, body }) => {
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
                                        Label::new(i18n::t!("Composite"))
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                    .child(id_label(&composite.id))
                                    .child(
                                        field_frame(
                                            description,
                                            LABEL_FIELD_WIDTH,
                                            true,
                                            window,
                                            cx,
                                        )
                                        .flex_1()
                                        .child(description.clone()),
                                    )
                                    .child(self.render_scope_direction(
                                        &child_path,
                                        composite.body.direction,
                                        window,
                                        cx,
                                    ))
                                    .child(controls),
                            )
                            .child(div().w_full().pl_4().child(self.render_scope(
                                &child_path,
                                &composite.body,
                                body,
                                ids,
                                window,
                                cx,
                            ))),
                    )
                }
                (StateItem::Divider, _) => list.child(
                    h_flex()
                        .w_full()
                        .gap_1()
                        .child(
                            Label::new(i18n::t!("Concurrency Divider (--)"))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(controls),
                ),
                // A line the model keeps verbatim stays where it is and is not
                // offered for editing; showing it as a field would invite a
                // change the model could not carry back.
                (StateItem::Preserved(_), _) => list,
                _ => list,
            };
        }

        let in_composite = !path.is_empty();
        let mut buttons = h_flex().w_full().flex_wrap().gap_1();
        for (id, label, kind) in [
            ("state", i18n::t!("Add State"), AddKind::State),
            (
                "transition",
                i18n::t!("Add Transition"),
                AddKind::Transition,
            ),
            ("note", i18n::t!("Add Note"), AddKind::Note),
            (
                "composite",
                i18n::t!("Add Composite State"),
                AddKind::Composite,
            ),
            (
                "divider",
                i18n::t!("Add Concurrency Divider"),
                AddKind::Divider,
            ),
        ] {
            // `--` only means something inside a composite state.
            if matches!(kind, AddKind::Divider) && !in_composite {
                continue;
            }
            let path = path.to_vec();
            buttons = buttons.child(add_button(
                SharedString::from(format!("mermaid-state-add-{id}-{key}")),
                SharedString::from(label),
                cx.listener(move |this, _, window, cx| {
                    let path = path.clone();
                    match kind {
                        AddKind::State => this.add_state(path, window, cx),
                        AddKind::Transition => this.add_transition(path, window, cx),
                        AddKind::Note => this.add_note(path, window, cx),
                        AddKind::Composite => this.add_composite(path, window, cx),
                        AddKind::Divider => this.add_divider(path, window, cx),
                    }
                }),
            ));
        }

        list.child(buttons).into_any_element()
    }

    fn render_state_kind_menu(
        &self,
        path: &[usize],
        index: usize,
        current: StateKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let owned_path = path.to_vec();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for kind in StateKind::ALL {
                let this = this.clone();
                let path = owned_path.clone();
                menu = menu.toggleable_entry(
                    kind.label(),
                    kind == current,
                    IconPosition::Start,
                    None,
                    move |window, cx| {
                        let path = path.clone();
                        this.update(cx, |this, cx| {
                            this.update_state_item(&path, index, window, cx, |item| {
                                if let StateItem::State(entry) = item {
                                    entry.kind = kind;
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
            SharedString::from(format!("mermaid-state-kind-{}-{index}", path_key(path))),
            current.label(),
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
    }

    fn render_endpoint(
        &self,
        path: &[usize],
        index: usize,
        is_from: bool,
        current: &str,
        ids: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let owned_path = path.to_vec();
        let selected = current.to_string();
        let choices = std::iter::once(TERMINAL_STATE.to_string())
            .chain(ids.iter().cloned())
            .collect::<Vec<_>>();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for id in choices.clone() {
                let this = this.clone();
                let path = owned_path.clone();
                let is_selected = id == selected;
                menu = menu.toggleable_entry(
                    endpoint_label(&id),
                    is_selected,
                    IconPosition::Start,
                    None,
                    move |window, cx| {
                        let (path, id) = (path.clone(), id.clone());
                        this.update(cx, |this, cx| {
                            this.update_state_item(&path, index, window, cx, |item| {
                                if let StateItem::Transition(transition) = item {
                                    if is_from {
                                        transition.from = id;
                                    } else {
                                        transition.to = id;
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

        DropdownMenu::new(
            SharedString::from(format!(
                "mermaid-state-endpoint-{}-{index}-{}",
                path_key(path),
                usize::from(is_from)
            )),
            endpoint_label(current),
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
    }

    fn render_note_placement(
        &self,
        path: &[usize],
        index: usize,
        current: NotePlacement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let owned_path = path.to_vec();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for placement in NotePlacement::ALL {
                let this = this.clone();
                let path = owned_path.clone();
                menu = menu.toggleable_entry(
                    placement.label(),
                    placement == current,
                    IconPosition::Start,
                    None,
                    move |window, cx| {
                        let path = path.clone();
                        this.update(cx, |this, cx| {
                            this.update_state_item(&path, index, window, cx, |item| {
                                if let StateItem::Note(note) = item {
                                    note.placement = placement;
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
            SharedString::from(format!(
                "mermaid-state-note-side-{}-{index}",
                path_key(path)
            )),
            current.label(),
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
    }

    fn render_note_target(
        &self,
        path: &[usize],
        index: usize,
        current: &str,
        ids: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let owned_path = path.to_vec();
        let selected = current.to_string();
        let choices = ids.to_vec();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for id in choices.clone() {
                let this = this.clone();
                let path = owned_path.clone();
                let is_selected = id == selected;
                menu = menu.toggleable_entry(
                    id.clone(),
                    is_selected,
                    IconPosition::Start,
                    None,
                    move |window, cx| {
                        let (path, id) = (path.clone(), id.clone());
                        this.update(cx, |this, cx| {
                            this.update_state_item(&path, index, window, cx, |item| {
                                if let StateItem::Note(note) = item {
                                    note.target = id;
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
            SharedString::from(format!(
                "mermaid-state-note-target-{}-{index}",
                path_key(path)
            )),
            SharedString::from(current.to_string()),
            menu,
        )
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .into_any_element()
    }
}

#[derive(Clone, Copy)]
enum AddKind {
    State,
    Transition,
    Note,
    Composite,
    Divider,
}

/// A path spelled for an element id, which has to be unique within the modal.
fn path_key(path: &[usize]) -> String {
    path.iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join("-")
}

/// The id of the state a statement names. It is shown rather than typed: the
/// list above the statements is the one place a state is renamed, so that a
/// state named only by its transitions can be renamed too.
fn id_label(id: &str) -> Label {
    Label::new(SharedString::from(id.to_string())).size(LabelSize::Small)
}

fn direction_label(direction: Option<FlowDirection>) -> &'static str {
    match direction {
        Some(direction) => direction.label(),
        None => i18n::t!("Not Set"),
    }
}

fn endpoint_label(id: &str) -> SharedString {
    if id == TERMINAL_STATE {
        i18n::t!("[*] Start / End").into()
    } else {
        SharedString::from(id.to_string())
    }
}
