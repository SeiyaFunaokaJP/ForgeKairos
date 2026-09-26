//! The git graph half of the Mermaid editor's form.
//!
//! A git graph is a script, so the form is one ordered list of operations —
//! commit, create a branch, check one out, merge, cherry-pick — each with its
//! own row. Where a row names a branch or a commit, it picks it from a menu of
//! what the graph already has rather than taking typed text, so that renaming
//! a branch or a commit id in its own field carries into every row that names
//! it and a typo cannot point a merge at a branch that does not exist.
//!
//! Removing a branch or a commit that other rows still name is refused with
//! the rows in the way, rather than leaving them naming nothing.

use std::rc::Rc;

use editor::Editor;
use gpui::{AnyElement, App, Context, Entity, Window};
use mermaid_render::gitgraph_model::{
    GitCommit, GitCommitType, GitGraphDirection, GitGraphDocument, GitGraphItem, GitGraphItemKind,
    GitMerge, is_valid_gitgraph_branch_name, is_valid_gitgraph_order, is_valid_gitgraph_text,
};
use ui::{ContextMenu, DropdownMenu, DropdownStyle, IconPosition, prelude::*};
use util::ResultExt as _;

use super::{
    Form, ID_FIELD_WIDTH, LABEL_FIELD_WIDTH, MermaidEditor, NUMBER_FIELD_WIDTH, add_button,
    field_frame, neighbour, row_controls, section_title,
};

#[cfg(test)]
#[path = "gitgraph_tests.rs"]
mod gitgraph_tests;

pub(super) struct GitGraphForm {
    pub(super) document: GitGraphDocument,
    pub(super) rows: Vec<GitItemRow>,
}

pub(super) enum GitItemRow {
    Commit {
        id: Entity<Editor>,
        /// `None` when the commit carries more than one tag, which a single
        /// field cannot hold without losing some; those are kept as written.
        tag: Option<Entity<Editor>>,
    },
    Branch {
        name: Entity<Editor>,
        order: Entity<Editor>,
    },
    Merge {
        id: Entity<Editor>,
        tag: Option<Entity<Editor>>,
    },
    /// A checkout, a cherry-pick or a preserved line: everything on the row is
    /// a menu, or nothing is editable at all.
    Fixed,
}

impl GitGraphForm {
    pub(super) fn fields(&self) -> Vec<Entity<Editor>> {
        self.rows
            .iter()
            .flat_map(|row| match row {
                GitItemRow::Commit { id, tag } | GitItemRow::Merge { id, tag } => {
                    std::iter::once(id.clone()).chain(tag.clone()).collect()
                }
                GitItemRow::Branch { name, order } => vec![name.clone(), order.clone()],
                GitItemRow::Fixed => Vec::new(),
            })
            .collect()
    }

    /// The graph the form currently describes, with every row that names a
    /// branch or a commit pointed at the name its own field now holds.
    pub(super) fn current_document(&self, cx: &App) -> GitGraphDocument {
        self.read_fields(false, cx)
    }

    /// The document a structural change is made on. Rows name branches and
    /// commits by their names, so a name that is empty or also belongs to
    /// another branch or commit is not taken into it: once two branches shared
    /// a name there, a later rename of either would carry every row naming
    /// that name along with it. The field keeps what was typed, and the rename
    /// from the last settled name follows once the name is unique again.
    fn settled_document(&self, cx: &App) -> GitGraphDocument {
        self.read_fields(true, cx)
    }

    fn read_fields(&self, settled: bool, cx: &App) -> GitGraphDocument {
        let mut document = self.document.clone();
        let mut branch_renames: Vec<(String, String)> = Vec::new();
        let mut commit_renames: Vec<(String, String)> = Vec::new();
        let field_text = |field: &Entity<Editor>| field.read(cx).text(cx).trim().to_string();

        let mut branch_names = vec![document.main_branch.clone()];
        let mut commit_ids = Vec::new();
        for (item, row) in document.items.iter().zip(&self.rows) {
            match (item, row) {
                (GitGraphItem::Branch(branch), GitItemRow::Branch { name, .. }) => {
                    branch_names.push(branch.name.clone());
                    branch_names.push(field_text(name));
                }
                (
                    GitGraphItem::Commit(GitCommit { id: old, .. }),
                    GitItemRow::Commit { id, .. },
                )
                | (GitGraphItem::Merge(GitMerge { id: old, .. }), GitItemRow::Merge { id, .. }) => {
                    commit_ids.extend(old.clone());
                    commit_ids.push(field_text(id));
                }
                _ => {}
            }
        }
        // Each row's own old and new name are among the names counted, twice
        // when they are the same.
        let is_shared = |names: &[String], old: &str, new: &str| {
            let own = if old == new { 2 } else { 1 };
            names.iter().filter(|name| name.as_str() == new).count() > own
        };

        for (item, row) in document.items.iter_mut().zip(&self.rows) {
            match (item, row) {
                (
                    GitGraphItem::Commit(GitCommit { id: old, tags, .. }),
                    GitItemRow::Commit { id, tag },
                )
                | (
                    GitGraphItem::Merge(GitMerge { id: old, tags, .. }),
                    GitItemRow::Merge { id, tag },
                ) => {
                    let text = field_text(id);
                    let unsettled = text.is_empty()
                        || is_shared(&commit_ids, old.as_deref().unwrap_or_default(), &text);
                    if !(settled && unsettled) {
                        rename_id(old, text, &mut commit_renames);
                    }
                    if let Some(tag) = tag {
                        set_tag(tags, tag, cx);
                    }
                }
                (GitGraphItem::Branch(branch), GitItemRow::Branch { name, order }) => {
                    let new_name = field_text(name);
                    let unsettled =
                        new_name.is_empty() || is_shared(&branch_names, &branch.name, &new_name);
                    if new_name != branch.name && !(settled && unsettled) {
                        // An emptied field is a name still being typed; the
                        // rows naming the branch keep the old name until there
                        // is a new one to follow.
                        if !new_name.is_empty() && !branch.name.is_empty() {
                            branch_renames.push((branch.name.clone(), new_name.clone()));
                        }
                        branch.name = new_name;
                    }
                    let order = field_text(order);
                    branch.order = (!order.is_empty()).then_some(order);
                }
                _ => {}
            }
        }

        document.retarget_branches(&branch_renames);
        document.retarget_commits(&commit_renames);
        document
    }
}

fn rename_id(id: &mut Option<String>, text: String, renames: &mut Vec<(String, String)>) {
    let new_id = (!text.is_empty()).then_some(text);
    if new_id == *id {
        return;
    }
    if let (Some(old), Some(new)) = (id.as_ref(), new_id.as_ref()) {
        renames.push((old.clone(), new.clone()));
    }
    *id = new_id;
}

/// Writes a tag field back. A `tag: ""` shows as an empty field, and leaving
/// that field empty keeps it rather than rewriting the statement without it.
fn set_tag(tags: &mut Vec<String>, field: &Entity<Editor>, cx: &App) {
    let text = field.read(cx).text(cx).trim().to_string();
    if text.is_empty() {
        if tags.len() != 1 || tags.first().is_some_and(|tag| !tag.is_empty()) {
            tags.clear();
        }
    } else {
        *tags = vec![text];
    }
}

/// Every check a git graph needs is about how one row sits in the whole
/// script, so they all live in `GitGraphDocument::structure_error`, which the
/// caller consults right after this.
pub(super) fn problem(_document: &GitGraphDocument) -> Option<SharedString> {
    None
}

fn commit_type_label(commit_type: Option<GitCommitType>) -> &'static str {
    commit_type.map_or("種類: 既定", GitCommitType::label)
}

fn direction_label(direction: Option<GitGraphDirection>) -> &'static str {
    direction.map_or("既定 (左から右)", GitGraphDirection::label)
}

/// A preserved line as its row shows it: the first line, cut short.
fn preserved_label(text: &str) -> SharedString {
    let first = text.lines().next().unwrap_or_default().trim();
    let mut label = first
        .chars()
        .take(PRESERVED_LABEL_CHARS)
        .collect::<String>();
    if label.chars().count() < first.chars().count() || text.contains('\n') {
        label.push('…');
    }
    label.into()
}

const PRESERVED_LABEL_CHARS: usize = 40;

type Pick<T> = Rc<dyn Fn(&mut MermaidEditor, T, &mut Window, &mut Context<MermaidEditor>)>;

impl MermaidEditor {
    pub(super) fn new_gitgraph_form(
        &mut self,
        document: GitGraphDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> GitGraphForm {
        let rows = document
            .items
            .iter()
            .map(|item| self.new_gitgraph_row(item, window, cx))
            .collect();
        GitGraphForm { document, rows }
    }

    fn new_gitgraph_row(
        &mut self,
        item: &GitGraphItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> GitItemRow {
        match item {
            GitGraphItem::Commit(commit) => GitItemRow::Commit {
                id: self.new_field(commit.id.as_deref().unwrap_or_default(), window, cx),
                tag: self.new_tag_field(&commit.tags, window, cx),
            },
            GitGraphItem::Merge(merge) => GitItemRow::Merge {
                id: self.new_field(merge.id.as_deref().unwrap_or_default(), window, cx),
                tag: self.new_tag_field(&merge.tags, window, cx),
            },
            GitGraphItem::Branch(branch) => GitItemRow::Branch {
                name: self.new_field(&branch.name, window, cx),
                order: self.new_field(branch.order.as_deref().unwrap_or_default(), window, cx),
            },
            GitGraphItem::Checkout(_)
            | GitGraphItem::CherryPick(_)
            | GitGraphItem::Preserved(_) => GitItemRow::Fixed,
        }
    }

    fn new_tag_field(
        &mut self,
        tags: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<Editor>> {
        (tags.len() <= 1).then(|| {
            self.new_field(
                tags.first().map(String::as_str).unwrap_or_default(),
                window,
                cx,
            )
        })
    }

    pub(super) fn gitgraph(&self) -> Option<&GitGraphForm> {
        match &self.form {
            Form::GitGraph(form) => Some(form),
            _ => None,
        }
    }

    pub(super) fn gitgraph_mut(&mut self) -> Option<&mut GitGraphForm> {
        match &mut self.form {
            Form::GitGraph(form) => Some(form),
            _ => None,
        }
    }

    /// Reads the fields into the document before a structural change. Which
    /// rows a branch or a commit is named by is decided by the names in the
    /// fields, and a rename has to reach those rows before one of them moves or
    /// goes.
    fn sync_gitgraph(&mut self, cx: &mut Context<Self>) {
        let Some(document) = self.gitgraph().map(|form| form.settled_document(cx)) else {
            return;
        };
        if let Some(form) = self.gitgraph_mut() {
            form.document = document;
        }
    }

    fn gitgraph_changed(&mut self, cx: &mut Context<Self>) {
        self.message = None;
        self.schedule_preview(cx);
        cx.notify();
    }

    pub(super) fn add_gitgraph_item(
        &mut self,
        kind: GitGraphItemKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_gitgraph(cx);
        let Some(item) = self
            .gitgraph()
            .map(|form| form.document.new_item(kind, form.document.items.len()))
        else {
            return;
        };
        let row = self.new_gitgraph_row(&item, window, cx);
        let Some(form) = self.gitgraph_mut() else {
            return;
        };
        form.document.items.push(item);
        form.rows.push(row);
        self.gitgraph_changed(cx);
    }

    pub(super) fn remove_gitgraph_item(&mut self, index: usize, cx: &mut Context<Self>) {
        self.sync_gitgraph(cx);
        let Some(form) = self.gitgraph_mut() else {
            return;
        };
        if index >= form.rows.len() {
            return;
        }
        match form.document.remove_item(index) {
            Ok(_) => {
                form.rows.remove(index);
                self.gitgraph_changed(cx);
            }
            Err(reason) => {
                self.message = Some(reason.into());
                cx.notify();
            }
        }
    }

    pub(super) fn move_gitgraph_item(
        &mut self,
        index: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        self.sync_gitgraph(cx);
        let Some(form) = self.gitgraph_mut() else {
            return;
        };
        let length = form.rows.len();
        if length != form.document.items.len() || index >= length {
            return;
        }
        let Some(other) = neighbour(index, forward, length) else {
            return;
        };
        form.document.items.swap(index, other);
        form.rows.swap(index, other);
        self.gitgraph_changed(cx);
    }

    pub(super) fn set_gitgraph_item_kind(
        &mut self,
        index: usize,
        kind: GitGraphItemKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_gitgraph(cx);
        let Some(form) = self.gitgraph_mut() else {
            return;
        };
        if index >= form.rows.len() {
            return;
        }
        if let Err(reason) = form.document.change_kind(index, kind) {
            self.message = Some(reason.into());
            cx.notify();
            return;
        }
        let Some(item) = form.document.items.get(index).cloned() else {
            return;
        };
        let row = self.new_gitgraph_row(&item, window, cx);
        if let Some(slot) = self
            .gitgraph_mut()
            .and_then(|form| form.rows.get_mut(index))
        {
            *slot = row;
        }
        self.gitgraph_changed(cx);
    }

    fn update_gitgraph_item(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
        update: impl FnOnce(&mut GitGraphItem),
    ) {
        self.sync_gitgraph(cx);
        let Some(item) = self
            .gitgraph_mut()
            .and_then(|form| form.document.items.get_mut(index))
        else {
            return;
        };
        update(item);
        self.gitgraph_changed(cx);
    }

    pub(super) fn set_gitgraph_branch(
        &mut self,
        index: usize,
        branch: String,
        cx: &mut Context<Self>,
    ) {
        self.update_gitgraph_item(index, cx, |item| match item {
            GitGraphItem::Checkout(checkout) => checkout.branch = branch,
            GitGraphItem::Merge(merge) => merge.branch = branch,
            _ => {}
        });
    }

    pub(super) fn set_gitgraph_commit_type(
        &mut self,
        index: usize,
        commit_type: Option<GitCommitType>,
        cx: &mut Context<Self>,
    ) {
        self.update_gitgraph_item(index, cx, |item| match item {
            GitGraphItem::Commit(commit) => commit.commit_type = commit_type,
            GitGraphItem::Merge(merge) => merge.commit_type = commit_type,
            _ => {}
        });
    }

    pub(super) fn set_gitgraph_pick(&mut self, index: usize, id: String, cx: &mut Context<Self>) {
        self.update_gitgraph_item(index, cx, |item| {
            if let GitGraphItem::CherryPick(pick) = item {
                pick.id = id;
            }
        });
    }

    pub(super) fn set_gitgraph_pick_parent(
        &mut self,
        index: usize,
        parent: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.update_gitgraph_item(index, cx, |item| {
            if let GitGraphItem::CherryPick(pick) = item {
                pick.parent = parent;
            }
        });
    }

    pub(super) fn set_gitgraph_direction(
        &mut self,
        direction: Option<GitGraphDirection>,
        cx: &mut Context<Self>,
    ) {
        self.sync_gitgraph(cx);
        let Some(form) = self.gitgraph_mut() else {
            return;
        };
        form.document.direction = direction;
        self.gitgraph_changed(cx);
    }

    pub(super) fn render_gitgraph_toolbar(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(form) = self.gitgraph() else {
            return div().into_any_element();
        };
        let current = form.document.direction;
        let choices = [None]
            .into_iter()
            .chain(GitGraphDirection::ALL.map(Some))
            .map(|direction| (SharedString::from(direction_label(direction)), direction))
            .collect();
        h_flex()
            .gap_2()
            .child(section_title("向き"))
            .child(self.render_gitgraph_menu(
                "mermaid-git-direction".into(),
                direction_label(current).into(),
                choices,
                current,
                |this, direction, _window, cx| this.set_gitgraph_direction(direction, cx),
                window,
                cx,
            ))
            .into_any_element()
    }

    pub(super) fn render_gitgraph(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(form) = self.gitgraph() else {
            return Vec::new();
        };
        let current = form.current_document(cx);

        // The main branch is named by the configuration in front of the
        // diagram, which this form does not edit, so its name is only shown.
        let header = h_flex()
            .w_full()
            .gap_2()
            .child(section_title(format!(
                "主ブランチ: {}",
                current.main_branch
            )))
            .child(
                Label::new("(名前は設定の mainBranchName をテキストで変更)")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .into_any_element();

        let mut list = v_flex()
            .w_full()
            .gap_1()
            .child(section_title("操作 (上から順に実行)"));
        let last = form.rows.len().saturating_sub(1);
        for index in 0..form.rows.len() {
            let (Some(row), Some(item)) = (form.rows.get(index), current.items.get(index)) else {
                continue;
            };
            list = list.child(
                h_flex()
                    .w_full()
                    .flex_wrap()
                    .gap_1()
                    .child(
                        Label::new(format!("{}", index + 1))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(self.render_gitgraph_kind_menu(index, item, window, cx))
                    .children(self.render_gitgraph_row_body(index, row, item, &current, window, cx))
                    .child(div().flex_1())
                    .child(row_controls(
                        "mermaid-git-item",
                        index,
                        index == 0,
                        index >= last,
                        cx.listener(move |this, _, _window, cx| {
                            this.move_gitgraph_item(index, false, cx)
                        }),
                        cx.listener(move |this, _, _window, cx| {
                            this.move_gitgraph_item(index, true, cx)
                        }),
                        cx.listener(move |this, _, _window, cx| {
                            this.remove_gitgraph_item(index, cx)
                        }),
                    )),
            );
        }

        let mut buttons = h_flex().w_full().flex_wrap().gap_1();
        for kind in GitGraphItemKind::ALL {
            buttons = buttons.child(add_button(
                SharedString::from(format!("mermaid-git-add-{kind:?}")),
                format!("{}を追加", kind.label()),
                cx.listener(move |this, _, window, cx| this.add_gitgraph_item(kind, window, cx)),
            ));
        }

        vec![header, list.child(buttons).into_any_element()]
    }

    fn render_gitgraph_kind_menu(
        &self,
        index: usize,
        item: &GitGraphItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(current) = item.kind() else {
            return Label::new("メモ")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element();
        };
        let choices = GitGraphItemKind::ALL
            .into_iter()
            .map(|kind| (SharedString::from(kind.label()), kind))
            .collect();
        self.render_gitgraph_menu(
            SharedString::from(format!("mermaid-git-kind-{index}")),
            current.label().into(),
            choices,
            current,
            move |this, kind, window, cx| this.set_gitgraph_item_kind(index, kind, window, cx),
            window,
            cx,
        )
    }

    fn render_gitgraph_row_body(
        &self,
        index: usize,
        row: &GitItemRow,
        item: &GitGraphItem,
        document: &GitGraphDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let ids = document.commit_ids();
        let branches = document.branch_names();
        match (item, row) {
            (GitGraphItem::Commit(commit), GitItemRow::Commit { id, tag }) => {
                let mut body = self.render_gitgraph_id_and_tag(
                    commit.id.as_deref(),
                    id,
                    &commit.tags,
                    tag.as_ref(),
                    &ids,
                    window,
                    cx,
                );
                body.push(self.render_gitgraph_type_menu(index, commit.commit_type, window, cx));
                body
            }
            (GitGraphItem::Merge(merge), GitItemRow::Merge { id, tag }) => {
                let mut body = vec![
                    section_title("取り込む").into_any_element(),
                    self.render_gitgraph_branch_menu(index, &merge.branch, &branches, window, cx),
                ];
                body.extend(self.render_gitgraph_id_and_tag(
                    merge.id.as_deref(),
                    id,
                    &merge.tags,
                    tag.as_ref(),
                    &ids,
                    window,
                    cx,
                ));
                body.push(self.render_gitgraph_type_menu(index, merge.commit_type, window, cx));
                body
            }
            (GitGraphItem::Branch(branch), GitItemRow::Branch { name, order }) => {
                let unique = branches
                    .iter()
                    .filter(|other| **other == branch.name)
                    .count()
                    == 1;
                vec![
                    field_frame(
                        name,
                        LABEL_FIELD_WIDTH,
                        is_valid_gitgraph_branch_name(&branch.name) && unique,
                        window,
                        cx,
                    )
                    .child(name.clone())
                    .into_any_element(),
                    section_title("順序").into_any_element(),
                    field_frame(
                        order,
                        NUMBER_FIELD_WIDTH,
                        branch
                            .order
                            .as_ref()
                            .is_none_or(|order| is_valid_gitgraph_order(order)),
                        window,
                        cx,
                    )
                    .child(order.clone())
                    .into_any_element(),
                ]
            }
            (GitGraphItem::Checkout(checkout), _) => vec![self.render_gitgraph_branch_menu(
                index,
                &checkout.branch,
                &branches,
                window,
                cx,
            )],
            (GitGraphItem::CherryPick(pick), _) => {
                let picks = ids
                    .iter()
                    .map(|id| (SharedString::from(id.to_string()), id.to_string()))
                    .collect();
                let parents = std::iter::once((SharedString::from("(指定なし)"), None))
                    .chain(
                        ids.iter()
                            .map(|id| (SharedString::from(id.to_string()), Some(id.to_string()))),
                    )
                    .collect();
                let pick_label = if pick.id.is_empty() {
                    "(選択)".to_string()
                } else {
                    pick.id.clone()
                };
                vec![
                    section_title("コミット").into_any_element(),
                    self.render_gitgraph_menu(
                        SharedString::from(format!("mermaid-git-pick-{index}")),
                        pick_label.into(),
                        picks,
                        pick.id.clone(),
                        move |this, id, _window, cx| this.set_gitgraph_pick(index, id, cx),
                        window,
                        cx,
                    ),
                    section_title("親").into_any_element(),
                    self.render_gitgraph_menu(
                        SharedString::from(format!("mermaid-git-parent-{index}")),
                        pick.parent
                            .clone()
                            .unwrap_or_else(|| "(指定なし)".to_string())
                            .into(),
                        parents,
                        pick.parent.clone(),
                        move |this, parent, _window, cx| {
                            this.set_gitgraph_pick_parent(index, parent, cx)
                        },
                        window,
                        cx,
                    ),
                ]
            }
            (GitGraphItem::Preserved(text), _) => vec![
                Label::new(preserved_label(text))
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element(),
            ],
            _ => Vec::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_gitgraph_id_and_tag(
        &self,
        id: Option<&str>,
        id_field: &Entity<Editor>,
        tags: &[String],
        tag_field: Option<&Entity<Editor>>,
        ids: &[&str],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let id_is_valid = id.is_none_or(|id| {
            is_valid_gitgraph_text(id) && ids.iter().filter(|other| **other == id).count() == 1
        });
        let mut body = vec![
            section_title("ID").into_any_element(),
            field_frame(id_field, ID_FIELD_WIDTH, id_is_valid, window, cx)
                .child(id_field.clone())
                .into_any_element(),
            section_title("タグ").into_any_element(),
        ];
        body.push(match tag_field {
            Some(field) => field_frame(
                field,
                ID_FIELD_WIDTH,
                tags.iter().all(|tag| is_valid_gitgraph_text(tag)),
                window,
                cx,
            )
            .child(field.clone())
            .into_any_element(),
            // Several tags cannot share one field without losing their
            // boundaries, so they are shown and kept as written.
            None => Label::new(tags.join(", "))
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
        });
        body
    }

    fn render_gitgraph_type_menu(
        &self,
        index: usize,
        current: Option<GitCommitType>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let choices = [None]
            .into_iter()
            .chain(GitCommitType::ALL.map(Some))
            .map(|commit_type| {
                (
                    SharedString::from(commit_type_label(commit_type)),
                    commit_type,
                )
            })
            .collect();
        self.render_gitgraph_menu(
            SharedString::from(format!("mermaid-git-type-{index}")),
            commit_type_label(current).into(),
            choices,
            current,
            move |this, commit_type, _window, cx| {
                this.set_gitgraph_commit_type(index, commit_type, cx)
            },
            window,
            cx,
        )
    }

    fn render_gitgraph_branch_menu(
        &self,
        index: usize,
        current: &str,
        branches: &[&str],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let choices = branches
            .iter()
            .map(|name| (SharedString::from(name.to_string()), name.to_string()))
            .collect();
        self.render_gitgraph_menu(
            SharedString::from(format!("mermaid-git-branch-{index}")),
            current.to_string().into(),
            choices,
            current.to_string(),
            move |this, branch, _window, cx| this.set_gitgraph_branch(index, branch, cx),
            window,
            cx,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn render_gitgraph_menu<T: Clone + PartialEq + 'static>(
        &self,
        id: SharedString,
        trigger: SharedString,
        choices: Vec<(SharedString, T)>,
        current: T,
        on_pick: impl Fn(&mut MermaidEditor, T, &mut Window, &mut Context<MermaidEditor>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let on_pick: Pick<T> = Rc::new(on_pick);
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            for (label, value) in choices {
                let this = this.clone();
                let on_pick = on_pick.clone();
                let toggled = value == current;
                menu = menu.toggleable_entry(
                    label,
                    toggled,
                    IconPosition::Start,
                    None,
                    move |window, cx| {
                        let value = value.clone();
                        let on_pick = on_pick.clone();
                        this.update(cx, |this, cx| on_pick(this, value, window, cx))
                            .log_err();
                    },
                );
            }
            menu
        });
        DropdownMenu::new(id, trigger, menu)
            .style(DropdownStyle::Outlined)
            .trigger_size(ButtonSize::Compact)
            .into_any_element()
    }
}
