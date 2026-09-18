use db::kvp::KeyValueStore;
use editor::Editor;
use git::repository::RewriteGroup;
use gpui::{
    App, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Task, Window,
};
use menu::{Cancel, Confirm, SecondaryConfirm};
use std::path::Path;
use ui::{ContextMenu, PopoverMenu, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::ModalView;

/// Where the branch a rewrite started from is remembered, keyed by the repository and the scratch
/// branch the rewrite builds on.
///
/// A rewrite that stops on a conflict parks the user on the scratch branch, and its name says
/// nothing about where they came from. Zed's own key-value store rather than anything under
/// `.git`: git's own state is the sequencer's, this is ours, and it has to survive a restart
/// because a conflict can take a while to resolve.
fn rewrite_origin_key(work_directory_abs_path: &Path, scratch_branch: &str) -> String {
    format!(
        "git-rewrite-origin:{}:{scratch_branch}",
        work_directory_abs_path.to_string_lossy()
    )
}

/// Remembers which branch a rewrite has to put the user back on.
pub(crate) fn remember_rewrite_origin(
    work_directory_abs_path: &Path,
    scratch_branch: &str,
    origin_branch: &str,
    cx: &App,
) -> Task<()> {
    let key = rewrite_origin_key(work_directory_abs_path, scratch_branch);
    let origin_branch = origin_branch.to_string();
    let store = KeyValueStore::global(cx);
    cx.background_spawn(async move {
        store.write_kvp(key, origin_branch).await.log_err();
    })
}

/// The branch a rewrite that is stopped on `scratch_branch` started from, or `None` when nothing
/// was remembered: the banner then says it cannot take the user back rather than guessing.
pub(crate) fn read_rewrite_origin(
    work_directory_abs_path: &Path,
    scratch_branch: &str,
    cx: &App,
) -> Task<Option<String>> {
    let key = rewrite_origin_key(work_directory_abs_path, scratch_branch);
    let store = KeyValueStore::global(cx);
    cx.background_spawn(async move { store.read_kvp(&key).log_err().flatten() })
}

/// Drops the mapping once the scratch branch is gone, so that a later rewrite of the same range
/// cannot be told to return to a branch that no longer exists.
pub(crate) fn forget_rewrite_origin(
    work_directory_abs_path: &Path,
    scratch_branch: &str,
    cx: &App,
) -> Task<()> {
    let key = rewrite_origin_key(work_directory_abs_path, scratch_branch);
    let store = KeyValueStore::global(cx);
    cx.background_spawn(async move {
        store.delete_kvp(key).await.log_err();
    })
}

/// One commit of the range being rewritten, as the todo list shows it.
#[derive(Clone, Debug)]
pub(crate) struct RewriteCommitRow {
    pub sha: String,
    pub subject: SharedString,
    /// The commit's whole message, which is what the user starts from when rewording it.
    pub message: SharedString,
}

/// What should happen to one commit of the range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RewriteRowAction {
    /// Keep the commit as it is, message and all.
    Keep,
    /// Fold into the commit above it, which is what `git rebase -i` calls squash.
    Fold,
    /// Keep the change, replace the message.
    Reword,
    /// Leave the commit out of the rewritten history.
    Drop,
}

impl RewriteRowAction {
    fn label(self) -> &'static str {
        match self {
            Self::Keep => "そのまま",
            Self::Fold => "直前にまとめる",
            Self::Reword => "メッセージを変更",
            Self::Drop => "捨てる",
        }
    }

    fn all() -> [Self; 4] {
        [Self::Keep, Self::Fold, Self::Reword, Self::Drop]
    }
}

/// The todo list as the plain data the rules below are decided from, with whatever the user has
/// typed already read out of the editors.
#[derive(Clone, Debug)]
struct TodoEntry {
    sha: String,
    subject: SharedString,
    /// The commit's own message.
    message: SharedString,
    action: RewriteRowAction,
    /// What the user typed, when they chose to reword this commit.
    reworded: Option<String>,
}

struct TodoRow {
    commit: RewriteCommitRow,
    action: RewriteRowAction,
    /// Only while the row is being reworded, so that switching away from it cannot leave a
    /// message behind that nothing shows any more.
    message_editor: Option<Entity<Editor>>,
}

/// What the modal hands back: the commits of the rewritten range, oldest first.
pub(crate) type RewriteConfirm = Box<dyn FnOnce(Vec<RewriteGroup>, &mut Window, &mut App)>;

/// The commits the rewritten range ends up with, or why the list cannot be used as it stands.
///
/// Reading the list from the top: every entry starts a new commit except a folded one, which
/// joins whatever came before it. Dropped entries are simply not there, so a dropped commit
/// between a commit and the one folded into it does not break them apart.
fn build_rewrite_groups(entries: &[TodoEntry]) -> Result<Vec<RewriteGroup>, SharedString> {
    let mut groups: Vec<RewriteGroup> = Vec::new();
    // Where in `entries` each group started, so that a group of several can be given the same
    // message the reword field would have shown for it.
    let mut group_starts: Vec<usize> = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        match entry.action {
            RewriteRowAction::Drop => continue,
            RewriteRowAction::Fold => {
                let Some(group) = groups.last_mut() else {
                    return Err(
                        "一番上のコミットは「直前にまとめる」にできません。まとめる先のコミットを上に置いてください。"
                            .into(),
                    );
                };
                group.shas.push(entry.sha.clone());
            }
            RewriteRowAction::Keep | RewriteRowAction::Reword => {
                let message = match &entry.reworded {
                    Some(message) => {
                        if message.trim().is_empty() {
                            return Err(format!(
                                "{} のメッセージが空です。入力してください。",
                                short_sha(&entry.sha)
                            )
                            .into());
                        }
                        Some(message.clone())
                    }
                    None => None,
                };
                groups.push(RewriteGroup {
                    shas: vec![entry.sha.clone()],
                    message,
                });
                group_starts.push(index);
            }
        }
    }
    // A group of several has to say something explicitly, and what it says is what the reword
    // field would have shown for it: the first commit's whole message, then the subjects folded
    // into it. Using the subjects alone would drop the first commit's body without telling anyone.
    for (group, start) in groups.iter_mut().zip(group_starts) {
        if group.shas.len() > 1 && group.message.is_none() {
            group.message = Some(default_message_for(entries, start));
        }
    }
    Ok(groups)
}

/// The message the commit at `index` ends up with if the user does not type one: its own message,
/// then the subject of everything folded into it.
fn default_message_for(entries: &[TodoEntry], index: usize) -> String {
    let Some(entry) = entries.get(index) else {
        return String::new();
    };
    let mut message = entry.message.trim_end().to_string();
    for folded in entries.iter().skip(index + 1) {
        match folded.action {
            RewriteRowAction::Drop => continue,
            RewriteRowAction::Fold => {
                message.push('\n');
                message.push_str(folded.subject.as_ref());
            }
            _ => break,
        }
    }
    message
}

/// Whether the list still says exactly what the range already is, in which case running git would
/// rewrite every SHA for nothing.
fn is_unchanged(entries: &[TodoEntry], original_shas: &[String]) -> bool {
    entries.len() == original_shas.len()
        && entries
            .iter()
            .zip(original_shas)
            .all(|(entry, sha)| entry.action == RewriteRowAction::Keep && &entry.sha == sha)
}

/// The todo list `git rebase -i` would open in an editor, as a modal: the picked commits in the
/// order they will be replayed, each with what should happen to it.
///
/// Nothing here runs git. The modal only builds the [`RewriteGroup`]s and hands them to its
/// caller, which confirms them with the user and runs them.
pub(crate) struct RewriteRangeModal {
    focus_handle: FocusHandle,
    /// Oldest first, the order git replays them in.
    rows: Vec<TodoRow>,
    /// The SHAs as they were picked, so that reordering alone still counts as a change.
    original_shas: Vec<String>,
    confirm: Option<RewriteConfirm>,
    /// Why the list cannot be used as it stands, shown under it so that the modal can stay open
    /// with the user's work still in it.
    error: Option<SharedString>,
}

impl RewriteRangeModal {
    pub(crate) fn new(
        commits: Vec<RewriteCommitRow>,
        confirm: RewriteConfirm,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            original_shas: commits.iter().map(|commit| commit.sha.clone()).collect(),
            rows: commits
                .into_iter()
                .map(|commit| TodoRow {
                    commit,
                    action: RewriteRowAction::Keep,
                    message_editor: None,
                })
                .collect(),
            confirm: Some(confirm),
            error: None,
        }
    }

    /// The list as plain data, with what the user has typed read out of the editors.
    fn entries(&self, cx: &App) -> Vec<TodoEntry> {
        self.rows
            .iter()
            .map(|row| TodoEntry {
                sha: row.commit.sha.clone(),
                subject: row.commit.subject.clone(),
                message: row.commit.message.clone(),
                action: row.action,
                reworded: row
                    .message_editor
                    .as_ref()
                    .map(|editor| editor.read(cx).text(cx)),
            })
            .collect()
    }

    fn move_row(&mut self, index: usize, offset: isize, cx: &mut Context<Self>) {
        let Some(target) = index.checked_add_signed(offset) else {
            return;
        };
        if index >= self.rows.len() || target >= self.rows.len() {
            return;
        }
        self.rows.swap(index, target);
        self.error = None;
        cx.notify();
    }

    fn set_action(
        &mut self,
        index: usize,
        action: RewriteRowAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.rows.get(index) else {
            return;
        };
        if row.action == action {
            return;
        }
        // Read the list before the row changes, so that the field starts from what the result
        // would have said anyway: folding throws the folded commits' messages away.
        let mut entries = self.entries(cx);
        if let Some(entry) = entries.get_mut(index) {
            entry.action = action;
        }
        let initial = default_message_for(&entries, index);
        let editor = (action == RewriteRowAction::Reword).then(|| {
            cx.new(|cx| {
                let mut editor = Editor::auto_height(1, 6, window, cx);
                editor.set_text(initial, window, cx);
                editor
            })
        });
        let Some(row) = self.rows.get_mut(index) else {
            return;
        };
        row.action = action;
        row.message_editor = editor.clone();
        self.error = None;
        if let Some(editor) = editor {
            editor.focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entries = self.entries(cx);
        if is_unchanged(&entries, &self.original_shas) {
            self.error = Some("変更がありません。並べ替えるか、操作を選んでください。".into());
            cx.notify();
            return;
        }
        let groups = match build_rewrite_groups(&entries) {
            Ok(groups) => groups,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        let Some(confirm) = self.confirm.take() else {
            return;
        };
        confirm(groups, window, cx);
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    /// Enter only reaches the modal when no message field has it, because an auto-height editor
    /// keeps Enter for a newline; Ctrl+Enter confirms from inside one.
    fn confirm_action(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.submit(window, cx);
    }

    fn secondary_confirm(
        &mut self,
        _: &SecondaryConfirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.submit(window, cx);
    }

    /// Tab walks the message fields rather than indenting: they are form fields here.
    fn focus_next_message(
        &mut self,
        _: &editor::actions::Tab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_message(true, window, cx);
    }

    fn focus_previous_message(
        &mut self,
        _: &editor::actions::Backtab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_message(false, window, cx);
    }

    fn focus_message(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let editors = self
            .rows
            .iter()
            .filter_map(|row| row.message_editor.clone())
            .collect::<Vec<_>>();
        if editors.is_empty() {
            return;
        }
        let focused = editors
            .iter()
            .position(|editor| editor.focus_handle(cx).contains_focused(window, cx));
        let next = match (focused, forward) {
            (Some(index), true) => (index + 1) % editors.len(),
            (Some(index), false) => (index + editors.len() - 1) % editors.len(),
            (None, true) => 0,
            (None, false) => editors.len() - 1,
        };
        if let Some(editor) = editors.get(next) {
            editor.focus_handle(cx).focus(window, cx);
            cx.stop_propagation();
        }
    }

    fn render_row(&self, index: usize, row: &TodoRow, cx: &mut Context<Self>) -> AnyElement {
        let is_first = index == 0;
        let is_last = index + 1 == self.rows.len();
        let dropped = row.action == RewriteRowAction::Drop;
        let modal = cx.entity();
        let current = row.action;
        v_flex()
            .w_full()
            .gap_0p5()
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        IconButton::new(("rewrite-row-up", index), IconName::ArrowUp)
                            .icon_size(IconSize::XSmall)
                            .disabled(is_first)
                            .tooltip(Tooltip::text("1 つ上へ"))
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.move_row(index, -1, cx);
                            })),
                    )
                    .child(
                        IconButton::new(("rewrite-row-down", index), IconName::ArrowDown)
                            .icon_size(IconSize::XSmall)
                            .disabled(is_last)
                            .tooltip(Tooltip::text("1 つ下へ"))
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.move_row(index, 1, cx);
                            })),
                    )
                    .child(
                        Label::new(short_sha(&row.commit.sha))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(row.commit.subject.clone())
                                .size(LabelSize::Small)
                                .color(if dropped {
                                    Color::Muted
                                } else {
                                    Color::Default
                                })
                                .when(dropped, |label| label.strikethrough())
                                .truncate(),
                        ),
                    )
                    .child(
                        PopoverMenu::new(("rewrite-row-action", index))
                            .menu(move |window, cx| {
                                let modal = modal.clone();
                                Some(ContextMenu::build(
                                    window,
                                    cx,
                                    move |mut menu, _window, _cx| {
                                        for action in RewriteRowAction::all() {
                                            let modal = modal.clone();
                                            menu = menu.toggleable_entry(
                                                action.label(),
                                                action == current,
                                                ui::IconPosition::End,
                                                None,
                                                move |window, cx| {
                                                    modal.update(cx, |modal, cx| {
                                                        modal.set_action(index, action, window, cx);
                                                    });
                                                },
                                            );
                                        }
                                        menu
                                    },
                                ))
                            })
                            .trigger(
                                Button::new(("rewrite-row-trigger", index), current.label())
                                    .label_size(LabelSize::Small)
                                    .end_icon(
                                        Icon::new(IconName::ChevronDown)
                                            .size(IconSize::XSmall)
                                            .color(Color::Muted),
                                    ),
                            ),
                    ),
            )
            .children(row.message_editor.clone().map(|editor| {
                div()
                    .w_full()
                    .px_1p5()
                    .py_1()
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .rounded_md()
                    .bg(cx.theme().colors().editor_background)
                    .child(editor)
            }))
            .into_any_element()
    }
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(git::SHORT_SHA_LENGTH).collect()
}

impl EventEmitter<DismissEvent> for RewriteRangeModal {}
impl ModalView for RewriteRangeModal {}

impl Focusable for RewriteRangeModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RewriteRangeModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.rows.len();
        let rows = self
            .rows
            .iter()
            .enumerate()
            .map(|(index, row)| self.render_row(index, row, cx))
            .collect::<Vec<_>>();
        v_flex()
            .key_context("GitGraphRewriteRange")
            .track_focus(&self.focus_handle)
            .elevation_3(cx)
            .w(rems(42.))
            .p_3()
            .gap_2()
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm_action))
            .on_action(cx.listener(Self::secondary_confirm))
            .capture_action(cx.listener(Self::focus_next_message))
            .capture_action(cx.listener(Self::focus_previous_message))
            .child(Label::new(format!("{count} 件のコミットを編集")).size(LabelSize::Large))
            .child(
                Label::new(
                    "上が古いコミットです。並べ替え・まとめる・メッセージの変更・捨てるを選んでから実行してください。",
                )
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(
                v_flex()
                    .id("rewrite-range-rows")
                    .w_full()
                    .gap_1()
                    .max_h(rems(24.))
                    .overflow_y_scroll()
                    .children(rows),
            )
            .children(
                self.error
                    .clone()
                    .map(|error| Label::new(error).size(LabelSize::Small).color(Color::Error)),
            )
            .child(
                h_flex()
                    .w_full()
                    .justify_end()
                    .gap_1()
                    .child(
                        Button::new("rewrite-range-cancel", "キャンセル").on_click(cx.listener(
                            |_, _, _window, cx| {
                                cx.emit(DismissEvent);
                            },
                        )),
                    )
                    .child(
                        Button::new("rewrite-range-confirm", "この内容で書き換える…")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.submit(window, cx);
                            })),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    /// The way back from a stopped rewrite has to survive a restart, so it lives in the database
    /// rather than in the panel, and it is kept per repository: two checkouts of the same history
    /// produce the same scratch branch name.
    #[gpui::test]
    async fn test_rewrite_origin_is_remembered_per_repository(cx: &mut TestAppContext) {
        let scratch = "zed-rewrite/abc1234";
        let repository = Path::new("/project-a");
        let other_repository = Path::new("/project-b");

        cx.update(|cx| remember_rewrite_origin(repository, scratch, "feature/work", cx))
            .await;
        assert_eq!(
            cx.update(|cx| read_rewrite_origin(repository, scratch, cx))
                .await,
            Some("feature/work".to_string())
        );
        assert_eq!(
            cx.update(|cx| read_rewrite_origin(other_repository, scratch, cx))
                .await,
            None,
            "another repository's scratch branch of the same name is not this one"
        );

        cx.update(|cx| forget_rewrite_origin(repository, scratch, cx))
            .await;
        assert_eq!(
            cx.update(|cx| read_rewrite_origin(repository, scratch, cx))
                .await,
            None,
            "a rewrite that finished must not leave a way back to a branch behind"
        );
    }

    fn entry(sha: &str, subject: &str, action: RewriteRowAction) -> TodoEntry {
        TodoEntry {
            sha: sha.to_string(),
            subject: subject.into(),
            message: format!("{subject}\n\n本文").into(),
            action,
            reworded: None,
        }
    }

    /// The order the rows are in is the order the commits are replayed in, a folded row joins the
    /// commit above it, and a dropped row is simply not there.
    #[test]
    fn test_build_rewrite_groups_folds_and_drops_in_list_order() {
        let entries = vec![
            entry("ccc", "C", RewriteRowAction::Keep),
            entry("aaa", "A", RewriteRowAction::Keep),
            entry("ddd", "D", RewriteRowAction::Drop),
            entry("bbb", "B", RewriteRowAction::Fold),
        ];
        let groups = build_rewrite_groups(&entries).unwrap();
        assert_eq!(
            groups,
            vec![
                RewriteGroup {
                    shas: vec!["ccc".to_string()],
                    message: None,
                },
                RewriteGroup {
                    // A dropped row between them must not break the fold apart, and the commit
                    // being folded into keeps its whole message, body included.
                    shas: vec!["aaa".to_string(), "bbb".to_string()],
                    message: Some("A\n\n本文\nB".to_string()),
                },
            ]
        );
        assert!(
            groups
                .iter()
                .all(|group| !group.shas.contains(&"ddd".to_string())),
            "the dropped commit must not be applied: {groups:?}"
        );
    }

    /// A message the user typed wins over the subjects, for the folded commit as well.
    #[test]
    fn test_build_rewrite_groups_use_the_typed_message() {
        let mut entries = vec![
            entry("aaa", "A", RewriteRowAction::Reword),
            entry("bbb", "B", RewriteRowAction::Fold),
        ];
        entries[0].reworded = Some("まとめた話".to_string());
        assert_eq!(
            build_rewrite_groups(&entries).unwrap(),
            vec![RewriteGroup {
                shas: vec!["aaa".to_string(), "bbb".to_string()],
                message: Some("まとめた話".to_string()),
            }]
        );

        entries[0].reworded = Some("   ".to_string());
        let error = build_rewrite_groups(&entries).expect_err("an empty message must be refused");
        assert!(error.contains("メッセージが空です"), "{error}");
    }

    /// There is nothing above the first commit to fold it into, so the list is refused rather than
    /// quietly turned into something else.
    #[test]
    fn test_build_rewrite_groups_refuse_a_fold_at_the_top() {
        let entries = vec![
            entry("ddd", "D", RewriteRowAction::Drop),
            entry("aaa", "A", RewriteRowAction::Fold),
        ];
        let error = build_rewrite_groups(&entries)
            .expect_err("nothing comes before the first kept commit to fold into");
        assert!(error.contains("一番上のコミット"), "{error}");
    }

    /// Dropping everything is a rewrite too: the range goes away.
    #[test]
    fn test_build_rewrite_groups_with_everything_dropped() {
        let entries = vec![
            entry("aaa", "A", RewriteRowAction::Drop),
            entry("bbb", "B", RewriteRowAction::Drop),
        ];
        assert_eq!(build_rewrite_groups(&entries).unwrap(), Vec::new());
    }

    /// The field a reword starts from says what the commit would have said anyway, folded rows
    /// included, so that folding does not throw a message away behind the user's back.
    #[test]
    fn test_default_message_gathers_the_folded_subjects() {
        let entries = vec![
            entry("aaa", "A", RewriteRowAction::Reword),
            entry("ddd", "D", RewriteRowAction::Drop),
            entry("bbb", "B", RewriteRowAction::Fold),
            entry("ccc", "C", RewriteRowAction::Keep),
            entry("eee", "E", RewriteRowAction::Fold),
        ];
        assert_eq!(default_message_for(&entries, 0), "A\n\n本文\nB");
        assert_eq!(default_message_for(&entries, 3), "C\n\n本文\nE");
        assert_eq!(default_message_for(&entries, 99), "");
    }

    /// Folding the whole range into its first commit keeps every message: the first one's body is
    /// still there and each folded commit contributes its subject.
    #[test]
    fn test_build_rewrite_groups_folding_everything_keeps_every_message() {
        let entries = vec![
            entry("aaa", "A", RewriteRowAction::Keep),
            entry("bbb", "B", RewriteRowAction::Fold),
            entry("ccc", "C", RewriteRowAction::Fold),
        ];
        assert_eq!(
            build_rewrite_groups(&entries).unwrap(),
            vec![RewriteGroup {
                shas: vec!["aaa".to_string(), "bbb".to_string(), "ccc".to_string()],
                message: Some("A\n\n本文\nB\nC".to_string()),
            }]
        );
    }

    /// Two commits with one of them dropped: what is left is a single commit that keeps its own
    /// message, so git is told to move it rather than to rebuild it.
    #[test]
    fn test_build_rewrite_groups_two_commits_with_one_dropped() {
        let entries = vec![
            entry("aaa", "A", RewriteRowAction::Keep),
            entry("bbb", "B", RewriteRowAction::Drop),
        ];
        assert_eq!(
            build_rewrite_groups(&entries).unwrap(),
            vec![RewriteGroup {
                shas: vec!["aaa".to_string()],
                message: None,
            }]
        );
    }

    /// A message with quotes and newlines reaches git as one argument, spelled exactly as typed:
    /// nothing here builds a command line out of a string, so there is nothing to escape.
    #[test]
    fn test_a_message_with_quotes_and_newlines_stays_one_argument() {
        let typed = "\"引用\" と改行\n\n2 行目 -- --exec echo";
        let mut entries = vec![
            entry("aaa", "A", RewriteRowAction::Reword),
            entry("bbb", "B", RewriteRowAction::Fold),
        ];
        entries[0].reworded = Some(typed.to_string());
        let groups = build_rewrite_groups(&entries).unwrap();
        assert_eq!(groups[0].message.as_deref(), Some(typed));

        let commands = git::repository::rewrite_range_commands(
            "0000000",
            &groups,
            "4444444",
            "main",
            "zed-rewrite/4444444",
        )
        .unwrap();
        let commit = commands
            .iter()
            .find(|command| command.args.first().map(String::as_str) == Some("commit"))
            .expect("the folded commit is rebuilt with `commit -m`");
        assert_eq!(
            commit.args,
            vec!["commit".to_string(), "-m".to_string(), typed.to_string()]
        );
        for command in &commands {
            git::repository::validate_git_command(&command.args)
                .unwrap_or_else(|error| panic!("`git {}`: {error:#}", command.args.join(" ")));
        }
    }

    /// Reordering alone is a change; leaving the list exactly as it was picked is not.
    #[test]
    fn test_is_unchanged_notices_a_reorder() {
        let original = vec!["aaa".to_string(), "bbb".to_string()];
        let untouched = vec![
            entry("aaa", "A", RewriteRowAction::Keep),
            entry("bbb", "B", RewriteRowAction::Keep),
        ];
        assert!(is_unchanged(&untouched, &original));

        let reordered = vec![
            entry("bbb", "B", RewriteRowAction::Keep),
            entry("aaa", "A", RewriteRowAction::Keep),
        ];
        assert!(!is_unchanged(&reordered, &original));

        let dropped = vec![
            entry("aaa", "A", RewriteRowAction::Keep),
            entry("bbb", "B", RewriteRowAction::Drop),
        ];
        assert!(!is_unchanged(&dropped, &original));
    }
}
