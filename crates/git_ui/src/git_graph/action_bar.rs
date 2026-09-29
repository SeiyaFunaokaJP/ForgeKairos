//! The row of labelled buttons for the picked commits, the first-run hints and the drag grips:
//! the graph's operations were otherwise only reachable through right-click menus and drags
//! nothing on screen hinted at.

use super::*;
use crate::commit_context_menu::{
    CommitFacts, CommitMenuOperation, commit_operation_refusal, run_commit_menu_operation,
};
use crate::git_terms::{
    OperationHelp, RESET_HARD_SUMMARY, RESET_MIXED_SUMMARY, RESET_SOFT_SUMMARY, amend_help,
    cherry_pick_selection_help, commit_operation_help, compare_refs_help, help_aside,
    log_source_help, merge_request_box_help, merge_request_toolbar_help, more_menu_help,
    reset_menu_help, rewrite_selection_help, tag_list_help,
};
use db::kvp::Dismissable;
use gpui::{Stateful, canvas};

/// "次回から表示しない" on the graph's hint band.
pub(super) struct GitGraphHints;

impl Dismissable for GitGraphHints {
    const KEY: &'static str = "git_graph_operation_hints_dismissed";
}

/// What the graph keeps for the button row and the hint band.
pub(super) struct GraphGuideState {
    pub(super) hints_visible: bool,
    /// The button row's width at the last paint, which decides how many buttons fit before the
    /// rest fold into "その他".
    bar_width: Rc<Cell<Option<Pixels>>>,
    /// The multi-selection facts, which walk the loaded history, for the selection, commit count
    /// and HEAD they were worked out for.
    selection_facts: Option<(BTreeSet<usize>, usize, Option<Oid>, bool, bool)>,
    /// What the mouse is over, whose explanation the line under the button row shows.
    pub(super) hovered_help: Option<HoveredHelp>,
}

impl GraphGuideState {
    pub(super) fn new(log_source: &LogSource, cx: &App) -> Self {
        Self {
            hints_visible: !matches!(log_source, LogSource::Path(_))
                && !GitGraphHints::dismissed(cx),
            bar_width: Rc::new(Cell::new(None)),
            selection_facts: None,
            hovered_help: None,
        }
    }
}

/// A control whose explanation the line under the button row shows while the mouse is over it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum HoveredHelp {
    /// A button of the row, for the rows picked when the mouse came over it: a button that is
    /// taken off screen under the mouse (by a click that changes the selection) never hears that
    /// the mouse left, and must not go on explaining itself for another selection.
    BarButton {
        action: CommitBarAction,
        rows: Vec<usize>,
    },
    MoreMenu {
        rows: Vec<usize>,
    },
    Toolbar(ToolbarHelp),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ToolbarHelp {
    LogSource,
    TagList,
    CompareRefs,
    MergeRequest,
}

impl ToolbarHelp {
    pub(super) fn help(self) -> OperationHelp {
        match self {
            Self::LogSource => log_source_help(),
            Self::TagList => tag_list_help(),
            Self::CompareRefs => compare_refs_help(),
            Self::MergeRequest => merge_request_toolbar_help(),
        }
    }
}

/// What the row's buttons are about, whatever their label says for the selection.
pub(super) fn commit_bar_help(action: CommitBarAction) -> OperationHelp {
    match action {
        CommitBarAction::Menu(operation) => commit_operation_help(operation),
        CommitBarAction::ResetMenu => reset_menu_help(),
        CommitBarAction::Amend => amend_help(),
        CommitBarAction::MergeRequestBox => merge_request_box_help(),
        CommitBarAction::CherryPickSelection => cherry_pick_selection_help(),
        CommitBarAction::RewriteSelection => rewrite_selection_help(),
    }
}

fn single_selection_description() -> &'static str {
    i18n::t!("Hover over a button to see what it does here. The Git guide explains the terms too")
}

/// The line under the button row: what the hovered control does (or why it cannot be pressed),
/// otherwise what the selection allows. With nothing picked the row itself already says what to
/// do, so the line stays empty.
pub(super) fn action_description(
    selection: &CommitBarSelection,
    buttons: &[CommitBarButton],
    rows: &[usize],
    hovered: Option<&HoveredHelp>,
) -> SharedString {
    match hovered {
        Some(HoveredHelp::Toolbar(toolbar)) => return toolbar.help().line().into(),
        Some(HoveredHelp::BarButton {
            action,
            rows: hovered_rows,
        }) if hovered_rows.as_slice() == rows => {
            if let Some(button) = buttons.iter().find(|button| button.action == *action) {
                return match &button.refusal {
                    Some(refusal) => {
                        let label = &button.label;
                        i18n::t!("\"{label}\" is unavailable right now: {refusal}").into()
                    }
                    None => button.help.line().into(),
                };
            }
        }
        Some(HoveredHelp::MoreMenu { rows: hovered_rows }) if hovered_rows.as_slice() == rows => {
            return more_menu_help().line().into();
        }
        _ => {}
    }
    match selection {
        CommitBarSelection::Nothing => SharedString::default(),
        CommitBarSelection::Single { .. } => single_selection_description().into(),
        CommitBarSelection::Multiple { count, .. } => i18n::tn!(
            count,
            "Cherry-pick the {count} picked commit, reorder it or add it to the MR box. Hover over a button to learn more",
            "Cherry-pick the {count} picked commits together, reorder them or add them to the MR box. Hover over a button to learn more"
        )
        .into(),
    }
}

/// What a button of the row does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CommitBarAction {
    /// The commit menu's item of the same name.
    Menu(CommitMenuOperation),
    /// "このコミットまで戻す ▾": the menu's three reset items.
    ResetMenu,
    /// Starts an amend of HEAD in the git panel.
    Amend,
    MergeRequestBox,
    /// The multi-selection menu's cherry-pick.
    CherryPickSelection,
    /// The multi-selection menu's todo editor.
    RewriteSelection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CommitBarButton {
    pub(super) action: CommitBarAction,
    pub(super) label: SharedString,
    pub(super) help: OperationHelp,
    /// Why the button cannot be pressed now; it shows greyed out with this as its tooltip.
    pub(super) refusal: Option<SharedString>,
}

/// What is picked in the graph, as far as the button row cares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum CommitBarSelection {
    Nothing,
    Single {
        facts: CommitFacts,
    },
    Multiple {
        count: usize,
        rewrite: RangeRewriteFacts,
    },
}

/// What decides whether the picked rows can be rewritten (reordered, squashed, dropped), known
/// before the menu or the button row is drawn so that a refusal shows there rather than after
/// the user picked the operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RangeRewriteFacts {
    pub(super) contiguous: bool,
    pub(super) on_current_branch: bool,
    /// HEAD is a branch, which is what the rewrite moves at the end.
    pub(super) head_is_branch: bool,
    /// The working tree has uncommitted changes, which the replay could collide with.
    pub(super) worktree_dirty: bool,
}

/// Why the todo editor (and squashing) cannot take the picked rows: both rewrite the checked-out
/// branch from the oldest picked commit up, so the rows have to be one unbroken chain on it, and
/// both replay commits over the working tree.
pub(super) fn range_rewrite_refusal(facts: RangeRewriteFacts) -> Option<&'static str> {
    if !facts.contiguous {
        Some(i18n::t!("Pick an unbroken range"))
    } else if !facts.on_current_branch {
        Some(i18n::t!("Pick commits on the current branch"))
    } else if !facts.head_is_branch {
        Some(i18n::t!(
            "A detached HEAD cannot be rewritten. Switch to a branch first"
        ))
    } else if facts.worktree_dirty {
        Some(i18n::t!("Commit or stash the uncommitted changes first"))
    } else {
        None
    }
}

pub(super) fn merge_request_box_refusal() -> &'static str {
    i18n::t!("A file history tab has no MR box. Open it in a Git Graph tab.")
}

/// The buttons the row shows for `selection`, in order.
pub(super) fn commit_bar_buttons(
    selection: &CommitBarSelection,
    can_hold_merge_request_box: bool,
) -> Vec<CommitBarButton> {
    let button = |action, label: &str, refusal: Option<SharedString>| CommitBarButton {
        action,
        label: SharedString::from(label.to_string()),
        help: commit_bar_help(action),
        refusal,
    };
    let merge_request_box_refusal =
        (!can_hold_merge_request_box).then(|| SharedString::from(merge_request_box_refusal()));
    match selection {
        CommitBarSelection::Nothing => Vec::new(),
        CommitBarSelection::Single { facts } => {
            let menu = |operation, label: &str| {
                button(
                    CommitBarAction::Menu(operation),
                    label,
                    commit_operation_refusal(operation, facts),
                )
            };
            let mut buttons = Vec::new();
            if facts.is_head {
                buttons.push(button(
                    CommitBarAction::Amend,
                    i18n::tc!("commit action", "Amend Last Commit"),
                    None,
                ));
            }
            buttons.extend([
                menu(
                    CommitMenuOperation::CreateBranch,
                    i18n::tc!("commit action", "Branch from Here"),
                ),
                menu(
                    CommitMenuOperation::Checkout,
                    i18n::tc!("commit action", "Check Out"),
                ),
                menu(
                    CommitMenuOperation::CherryPick,
                    i18n::tc!("commit action", "Cherry-Pick"),
                ),
                menu(
                    CommitMenuOperation::Revert,
                    i18n::tc!("commit action", "Revert"),
                ),
                menu(
                    CommitMenuOperation::Reword,
                    i18n::tc!("commit action", "Reword Message"),
                ),
                menu(
                    CommitMenuOperation::Fixup,
                    i18n::tc!("commit action", "Add Current Changes (fixup)"),
                ),
                button(
                    CommitBarAction::ResetMenu,
                    i18n::tc!("commit action", "Reset to This Commit"),
                    None,
                ),
                button(
                    CommitBarAction::MergeRequestBox,
                    i18n::tc!("commit action", "Add to MR Box"),
                    merge_request_box_refusal,
                ),
            ]);
            buttons
        }
        CommitBarSelection::Multiple { count, rewrite } => vec![
            button(
                CommitBarAction::CherryPickSelection,
                &i18n::tcn!(
                    "commit action",
                    count,
                    "Cherry-Pick {count} Commit",
                    "Cherry-Pick {count} Commits"
                ),
                None,
            ),
            button(
                CommitBarAction::RewriteSelection,
                i18n::tc!("commit action", "Reorder / Squash / Drop…"),
                range_rewrite_refusal(*rewrite)
                    .map(|reason| SharedString::from(i18n::t!("{reason}."))),
            ),
            button(
                CommitBarAction::MergeRequestBox,
                &i18n::tcn!(
                    "commit action",
                    count,
                    "Add to MR Box ({count} Commit)",
                    "Add to MR Box ({count} Commits)"
                ),
                merge_request_box_refusal,
            ),
        ],
    }
}

/// Roughly how wide a small button with `label` draws: the row decides from this which buttons
/// fold into "その他" before it has laid them out.
pub(super) fn estimated_button_width(label: &str) -> Pixels {
    let text: f32 = label
        .chars()
        .map(|character| if character.is_ascii() { 7.0 } else { 12.5 })
        .sum();
    px(text + 20.0)
}

const BAR_GAP: Pixels = px(4.0);
fn more_button_label() -> &'static str {
    i18n::tc!("commit action", "More")
}
/// The chevron that follows "その他" and "このコミットまで戻す".
const CHEVRON_WIDTH: Pixels = px(16.0);

/// How many of `buttons` fit in `available` after `leading` (the selection's label), keeping room
/// for "その他 ▾" whenever some have to fold into it. `None` (not measured yet) fits them all.
pub(super) fn inline_button_count(
    buttons: &[CommitBarButton],
    leading: Pixels,
    available: Option<Pixels>,
) -> usize {
    let Some(available) = available else {
        return buttons.len();
    };
    let width = |button: &CommitBarButton| {
        let chevron = if button.action == CommitBarAction::ResetMenu {
            CHEVRON_WIDTH
        } else {
            px(0.)
        };
        estimated_button_width(&button.label) + chevron + BAR_GAP
    };
    let total = buttons
        .iter()
        .map(width)
        .fold(leading, |sum, width| sum + width);
    if total <= available {
        return buttons.len();
    }
    let more = estimated_button_width(more_button_label()) + CHEVRON_WIDTH + BAR_GAP;
    let mut used = leading + more;
    let mut count = 0;
    for button in buttons {
        used += width(button);
        if used > available {
            break;
        }
        count += 1;
    }
    count
}

type BarHandler = Rc<dyn Fn(&mut Window, &mut App)>;

impl GitGraph {
    pub(super) fn commit_bar_selection(&mut self, cx: &App) -> CommitBarSelection {
        let rows = self.selected_rows();
        match rows.as_slice() {
            [] => CommitBarSelection::Nothing,
            [row] => {
                let Some(commit) = self.graph_data.commits.get(*row) else {
                    return CommitBarSelection::Nothing;
                };
                let sha = commit.data.sha;
                let is_merge = commit.data.parents.len() > 1;
                let facts = self
                    .get_repository(cx)
                    .map(|repository| CommitFacts::read(repository.read(cx), sha, Some(is_merge)))
                    .unwrap_or(CommitFacts {
                        is_merge: Some(is_merge),
                        ..CommitFacts::default()
                    });
                CommitBarSelection::Single { facts }
            }
            rows => {
                let picked: BTreeSet<usize> = rows.iter().copied().collect();
                let commit_count = self.graph_data.commits.len();
                let head = self.head_commit_sha(cx);
                let (contiguous, on_current_branch) = match &self.guide.selection_facts {
                    Some((cached_rows, cached_count, cached_head, contiguous, on_branch))
                        if *cached_rows == picked
                            && *cached_count == commit_count
                            && *cached_head == head =>
                    {
                        (*contiguous, *on_branch)
                    }
                    _ => {
                        let contiguous =
                            is_contiguous_commit_range(&self.graph_data.commits, &picked);
                        let on_current_branch = self.selection_is_on_current_branch(cx);
                        self.guide.selection_facts = Some((
                            picked.clone(),
                            commit_count,
                            head,
                            contiguous,
                            on_current_branch,
                        ));
                        (contiguous, on_current_branch)
                    }
                };
                CommitBarSelection::Multiple {
                    count: picked.len(),
                    rewrite: RangeRewriteFacts {
                        contiguous,
                        on_current_branch,
                        head_is_branch: self.head_is_branch(cx),
                        worktree_dirty: self.worktree_is_dirty(cx),
                    },
                }
            }
        }
    }

    fn selected_single_sha(&self) -> Option<Oid> {
        match self.selected_rows().as_slice() {
            [row] => self
                .graph_data
                .commits
                .get(*row)
                .map(|commit| commit.data.sha),
            _ => None,
        }
    }

    /// What pressing a button (or its entry under "その他") runs: the commit menu's own operation
    /// for the single-commit buttons, the multi-selection menu's graph methods for the others.
    fn commit_bar_handler(
        &self,
        action: CommitBarAction,
        sha: Option<Oid>,
        cx: &mut Context<Self>,
    ) -> BarHandler {
        let graph = cx.weak_entity();
        let repository = self
            .get_repository(cx)
            .map(|repository| repository.downgrade());
        let workspace = self.workspace.clone();
        let repo_id = self.repo_id;
        Rc::new(move |window, cx| match action {
            CommitBarAction::Menu(operation) => {
                let (Some(sha), Some(repository), Some(workspace)) = (
                    sha,
                    repository
                        .as_ref()
                        .and_then(|repository| repository.upgrade()),
                    workspace.upgrade(),
                ) else {
                    return;
                };
                run_commit_menu_operation(
                    operation,
                    repository,
                    workspace,
                    sha.to_string(),
                    window,
                    cx,
                );
            }
            CommitBarAction::ResetMenu => {}
            CommitBarAction::Amend => {
                // The git panel amends the HEAD of its own repository, which in a project with
                // several need not be the one this graph shows.
                let panel_has_this_repository = workspace.upgrade().is_some_and(|workspace| {
                    workspace
                        .read(cx)
                        .project()
                        .read(cx)
                        .active_repository(cx)
                        .is_some_and(|active| active.read(cx).id == repo_id)
                });
                if !panel_has_this_repository {
                    graph
                        .update(cx, |graph, cx| {
                            graph.report_refused(
                                i18n::t!(
                                    "The Git panel has another repository selected. Switch the Git panel to this repository, then amend."
                                )
                                .to_string(),
                                cx,
                            )
                        })
                        .ok();
                    return;
                }
                window.dispatch_action(git::StartAmend.boxed_clone(), cx)
            }
            CommitBarAction::MergeRequestBox => {
                graph
                    .update(cx, |graph, cx| {
                        graph.add_selection_to_merge_request_box(window, cx)
                    })
                    .ok();
            }
            CommitBarAction::CherryPickSelection => {
                graph
                    .update(cx, |graph, cx| graph.cherry_pick_rows(window, cx))
                    .ok();
            }
            CommitBarAction::RewriteSelection => {
                graph
                    .update(cx, |graph, cx| graph.open_rewrite_range_modal(window, cx))
                    .ok();
            }
        })
    }

    /// The row of buttons above the table. With nothing picked it says where the buttons come
    /// from, so that the row is there (and the table does not jump) before the first click.
    pub(super) fn render_commit_action_bar(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.graph_data.commits.is_empty() {
            return None;
        }
        let selection = self.commit_bar_selection(cx);
        let buttons = commit_bar_buttons(&selection, self.can_hold_merge_request_box());
        let sha = self.selected_single_sha();
        let leading_label: SharedString = match (&selection, sha) {
            (CommitBarSelection::Nothing, _) => i18n::t!(
                "Pick a commit to get its operations here (right-clicking offers the same)"
            )
            .into(),
            (CommitBarSelection::Single { .. }, Some(sha)) => {
                format!("{}:", sha.display_short()).into()
            }
            (CommitBarSelection::Multiple { count, .. }, _) => {
                i18n::tn!(count, "{count} commit:", "{count} commits:").into()
            }
            (CommitBarSelection::Single { .. }, None) => SharedString::default(),
        };
        let leading_width = estimated_button_width(&leading_label);
        let inline = inline_button_count(&buttons, leading_width, self.guide.bar_width.get());
        let (inline_buttons, folded_buttons) = buttons.split_at(inline.min(buttons.len()));
        let repository = self
            .get_repository(cx)
            .map(|repository| repository.downgrade());

        let mut bar = h_flex()
            .id("git-graph-action-bar")
            .debug_selector(|| "git-graph-action-bar".into())
            .relative()
            .w_full()
            .min_w_0()
            // `overflow_hidden` lets a flex item shrink to nothing, so the row has to opt out of
            // shrinking or it is the first thing a crowded tab squeezes.
            .flex_none()
            .overflow_hidden()
            .px_1p5()
            .py_1()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child({
                let bar_width = self.guide.bar_width.clone();
                canvas(
                    move |bounds, window, _cx| {
                        if bar_width.get() != Some(bounds.size.width) {
                            bar_width.set(Some(bounds.size.width));
                            window.request_animation_frame();
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full()
            })
            // As tall as a button, so that the row does not change height between nothing
            // picked (the label alone) and a pick (the buttons).
            .child(
                h_flex().min_w_0().h(ButtonSize::Default.rems()).child(
                    Label::new(leading_label)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                ),
            );

        for (index, button) in inline_buttons.iter().enumerate() {
            bar = bar.child(self.render_bar_button(index, button, sha, repository.clone(), cx));
        }
        if !folded_buttons.is_empty() {
            bar = bar.child(self.render_more_menu(folded_buttons, sha, repository, cx));
        }
        Some(bar.into_any_element())
    }

    fn render_bar_button(
        &self,
        index: usize,
        button: &CommitBarButton,
        sha: Option<Oid>,
        repository: Option<WeakEntity<Repository>>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selector = format!("git-graph-action-{}", commit_bar_selector(button.action));
        let tooltip: SharedString = button
            .refusal
            .clone()
            .unwrap_or_else(|| button.help.sentence().into());
        let hover_target = HoveredHelp::BarButton {
            action: button.action,
            rows: self.selected_rows(),
        };
        if button.action == CommitBarAction::ResetMenu {
            let Some(sha) = sha else {
                return Empty.into_any_element();
            };
            let workspace = self.workspace.clone();
            return self
                .help_hover_area(("git-graph-action-hover", index), hover_target, cx)
                .flex_none()
                .debug_selector(move || selector)
                .child(
                    PopoverMenu::new("git-graph-action-reset-menu")
                        .menu(move |window, cx| {
                            let repository = repository.clone();
                            let workspace = workspace.clone();
                            Some(ContextMenu::build(window, cx, move |menu, _, _| {
                                reset_menu_entries(menu, sha, repository, workspace)
                            }))
                        })
                        .trigger_with_tooltip(
                            Button::new(("git-graph-action", index), button.label.clone())
                                .label_size(LabelSize::Small)
                                .end_icon(
                                    Icon::new(IconName::ChevronDown)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                ),
                            Tooltip::text(tooltip),
                        ),
                )
                .into_any_element();
        }
        let handler = self.commit_bar_handler(button.action, sha, cx);
        self.help_hover_area(("git-graph-action-hover", index), hover_target, cx)
            .flex_none()
            .debug_selector(move || selector)
            .child(
                Button::new(("git-graph-action", index), button.label.clone())
                    .label_size(LabelSize::Small)
                    .style(ButtonStyle::Outlined)
                    .disabled(button.refusal.is_some())
                    .tooltip(Tooltip::text(tooltip))
                    .on_click(move |_, window, cx| handler(window, cx)),
            )
            .into_any_element()
    }

    fn render_more_menu(
        &self,
        folded: &[CommitBarButton],
        sha: Option<Oid>,
        repository: Option<WeakEntity<Repository>>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entries = folded
            .iter()
            .map(|button| {
                (
                    button.clone(),
                    self.commit_bar_handler(button.action, sha, cx),
                )
            })
            .collect::<Vec<_>>();
        let workspace = self.workspace.clone();
        let hover_target = HoveredHelp::MoreMenu {
            rows: self.selected_rows(),
        };
        self.help_hover_area("git-graph-action-more-hover", hover_target, cx)
            .flex_none()
            .debug_selector(|| "git-graph-action-more".into())
            .child(
                PopoverMenu::new("git-graph-action-more-menu")
                    .menu(move |window, cx| {
                        let entries = entries.clone();
                        let repository = repository.clone();
                        let workspace = workspace.clone();
                        Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                            for (button, handler) in entries.iter() {
                                if button.action == CommitBarAction::ResetMenu {
                                    let Some(sha) = sha else {
                                        continue;
                                    };
                                    let repository = repository.clone();
                                    let workspace = workspace.clone();
                                    menu = menu.submenu(button.label.clone(), move |menu, _, _| {
                                        reset_menu_entries(
                                            menu,
                                            sha,
                                            repository.clone(),
                                            workspace.clone(),
                                        )
                                    });
                                    continue;
                                }
                                let entry = ContextMenuEntry::new(button.label.clone())
                                    .documentation_aside(
                                        ui::DocumentationSide::Right,
                                        help_aside(button.help, button.refusal.clone()),
                                    );
                                menu = menu.item(if button.refusal.is_some() {
                                    entry.disabled(true)
                                } else {
                                    let handler = handler.clone();
                                    entry.handler(move |window, cx| handler(window, cx))
                                });
                            }
                            menu
                        }))
                    })
                    .trigger_with_tooltip(
                        Button::new("git-graph-action-more", more_button_label())
                            .label_size(LabelSize::Small)
                            .end_icon(
                                Icon::new(IconName::ChevronDown)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            ),
                        Tooltip::text(more_menu_help().sentence()),
                    ),
            )
            .into_any_element()
    }

    /// A box around a control that tells the line under the button row when the mouse is over
    /// the control.
    pub(super) fn help_hover_area(
        &self,
        id: impl Into<ElementId>,
        target: HoveredHelp,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let id = id.into();
        div()
            .id(id.clone())
            .debug_selector(move || id.to_string())
            .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                this.set_hovered_help(target.clone(), *hovered, cx);
            }))
    }

    fn set_hovered_help(&mut self, target: HoveredHelp, hovered: bool, cx: &mut Context<Self>) {
        if hovered {
            if self.guide.hovered_help.as_ref() != Some(&target) {
                self.guide.hovered_help = Some(target);
                cx.notify();
            }
        } else if self.guide.hovered_help.as_ref() == Some(&target) {
            self.guide.hovered_help = None;
            cx.notify();
        }
    }

    pub(super) fn action_description_text(&mut self, cx: &App) -> SharedString {
        let selection = self.commit_bar_selection(cx);
        let buttons = commit_bar_buttons(&selection, self.can_hold_merge_request_box());
        action_description(
            &selection,
            &buttons,
            &self.selected_rows(),
            self.guide.hovered_help.as_ref(),
        )
    }

    /// The line under the button row. It is always there, one line high, so that hovering or
    /// picking a commit never moves the table.
    pub(super) fn render_action_description(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.graph_data.commits.is_empty() {
            return None;
        }
        let text = self.action_description_text(cx);
        Some(
            h_flex()
                .debug_selector(|| "git-graph-action-description".into())
                .flex_none()
                .w_full()
                .min_w_0()
                .h_6()
                .px_2()
                .overflow_hidden()
                .border_b_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    Label::new(text)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .single_line()
                        .truncate(),
                )
                .into_any_element(),
        )
    }

    /// Three lines on what the graph can do that nothing on screen shows, until the user says
    /// they have read them.
    pub(super) fn render_first_run_hints(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.guide.hints_visible || self.graph_data.commits.is_empty() {
            return None;
        }
        let hint = |text: &'static str| {
            h_flex()
                .gap_1()
                .child(
                    Icon::new(IconName::ArrowRight)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .child(Label::new(text).size(LabelSize::Small))
        };
        Some(
            v_flex()
                .debug_selector(|| "git-graph-hints".into())
                .w_full()
                .px_2()
                .py_1p5()
                .gap_0p5()
                .border_b_1()
                .border_color(cx.theme().colors().border_variant)
                .bg(cx.theme().status().info_background.opacity(0.4))
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Icon::new(IconName::Info)
                                .size(IconSize::Small)
                                .color(Color::Info),
                        )
                        .child(
                            Label::new(i18n::t!("Using the Git Graph")).size(LabelSize::Small),
                        )
                        .child(div().flex_1())
                        .child(
                            Button::new(
                                "git-graph-hints-guide",
                                i18n::tc!("git graph hints", "Guide"),
                            )
                            .label_size(LabelSize::Small)
                            .tooltip(Tooltip::text(i18n::t!(
                                "Find a Git operation by what you want to do"
                            )))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(git::ShowGuide.boxed_clone(), cx)
                                }),
                        )
                        .child(
                            div()
                                .debug_selector(|| "git-graph-hints-dismiss".into())
                                .child(
                                    Button::new(
                                        "git-graph-hints-dismiss",
                                        i18n::t!("Don't Show Again"),
                                    )
                                        .label_size(LabelSize::Small)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            GitGraphHints::set_dismissed(true, cx);
                                            this.guide.hints_visible = false;
                                            cx.notify();
                                        })),
                                ),
                        )
                        .child(
                            IconButton::new("git-graph-hints-close", IconName::Close)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text(i18n::t!(
                                    "Close (they show again next time)"
                                )))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.guide.hints_visible = false;
                                    cx.notify();
                                })),
                        ),
                )
                .child(hint(i18n::t!(
                    "Pick a commit and the buttons for it show above"
                )))
                .child(hint(i18n::t!(
                    "Drag a commit onto a branch to cherry-pick it, onto a row to reorder, or into the MR box"
                )))
                .child(hint(i18n::t!(
                    "Drop files from the Git panel onto a commit to add the fix to that commit (fixup)"
                )))
                .into_any_element(),
        )
    }

    /// Where `git::CompareRefs` lands: the graph's own "比較…" button, or why it cannot compare.
    pub(crate) fn start_ref_comparison(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.comparable_refs(cx).len() < 2 {
            self.report_refused(
                i18n::t!("Comparing needs at least two branches or tags.").to_string(),
                cx,
            );
            return;
        }
        self.open_ref_comparison(window, cx);
    }
}

/// The commit menu's three reset items, for the "このコミットまで戻す ▾" button.
fn reset_menu_entries(
    menu: ContextMenu,
    sha: Oid,
    repository: Option<WeakEntity<Repository>>,
    workspace: WeakEntity<Workspace>,
) -> ContextMenu {
    let sha = sha.to_string();
    [
        (
            CommitMenuOperation::ResetSoft,
            format!("soft: {}", i18n::tr(RESET_SOFT_SUMMARY)),
        ),
        (
            CommitMenuOperation::ResetMixed,
            format!("mixed: {}", i18n::tr(RESET_MIXED_SUMMARY)),
        ),
        (
            CommitMenuOperation::ResetHard,
            format!("hard: {}…", i18n::tr(RESET_HARD_SUMMARY)),
        ),
    ]
    .into_iter()
    .fold(menu, |menu, (operation, label)| {
        let repository = repository.clone();
        let workspace = workspace.clone();
        let action = operation.action(&sha);
        let sha = sha.clone();
        menu.item(
            ContextMenuEntry::new(label)
                .action(action)
                .handler(move |window, cx| {
                    let (Some(repository), Some(workspace)) = (
                        repository
                            .as_ref()
                            .and_then(|repository| repository.upgrade()),
                        workspace.upgrade(),
                    ) else {
                        return;
                    };
                    run_commit_menu_operation(
                        operation,
                        repository,
                        workspace,
                        sha.clone(),
                        window,
                        cx,
                    );
                })
                .documentation_aside(
                    ui::DocumentationSide::Right,
                    help_aside(commit_operation_help(operation), None),
                ),
        )
    })
}

/// The drag grip at the start of a commit's subject, shown while its row is hovered.
pub(super) fn commit_drag_grip(index: usize, visible: bool) -> AnyElement {
    crate::git_guide::drag_grip(
        ElementId::NamedInteger("git-graph-commit-grip".into(), index as u64),
        crate::git_guide::COMMIT_DRAG_HINT,
    )
    .when(!visible, |grip| grip.invisible())
    .into_any_element()
}

fn commit_bar_selector(action: CommitBarAction) -> &'static str {
    match action {
        CommitBarAction::Menu(CommitMenuOperation::CreateBranch) => "branch",
        CommitBarAction::Menu(CommitMenuOperation::Checkout) => "checkout",
        CommitBarAction::Menu(CommitMenuOperation::CherryPick) => "cherry-pick",
        CommitBarAction::Menu(CommitMenuOperation::Revert) => "revert",
        CommitBarAction::Menu(CommitMenuOperation::Reword) => "reword",
        CommitBarAction::Menu(CommitMenuOperation::Fixup) => "fixup",
        CommitBarAction::Menu(_) => "other",
        CommitBarAction::ResetMenu => "reset",
        CommitBarAction::Amend => "amend",
        CommitBarAction::MergeRequestBox => "merge-request-box",
        CommitBarAction::CherryPickSelection => "cherry-pick-selection",
        CommitBarAction::RewriteSelection => "rewrite-selection",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_graph::tests::{draw_workspace, git_invocations, open_graph};
    use gpui::{TestAppContext, VisualTestContext};
    use std::path::Path;

    fn labels(buttons: &[CommitBarButton]) -> Vec<&str> {
        buttons.iter().map(|button| button.label.as_ref()).collect()
    }

    fn refusal(buttons: &[CommitBarButton], action: CommitBarAction) -> Option<SharedString> {
        buttons
            .iter()
            .find(|button| button.action == action)
            .and_then(|button| button.refusal.clone())
    }

    fn single(facts: CommitFacts) -> CommitBarSelection {
        CommitBarSelection::Single { facts }
    }

    #[test]
    fn test_commit_bar_buttons_for_one_commit() {
        let plain = CommitFacts {
            is_head: false,
            is_merge: Some(false),
            fixup_refusal: None,
        };
        let buttons = commit_bar_buttons(&single(plain.clone()), true);
        assert_eq!(
            labels(&buttons),
            [
                "Branch from Here",
                "Check Out",
                "Cherry-Pick",
                "Revert",
                "Reword Message",
                "Add Current Changes (fixup)",
                "Reset to This Commit",
                "Add to MR Box",
            ]
        );
        assert!(buttons.iter().all(|button| button.refusal.is_none()));
        assert_eq!(
            buttons
                .iter()
                .filter_map(|button| match button.action {
                    CommitBarAction::Menu(operation) => Some(operation),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [
                CommitMenuOperation::CreateBranch,
                CommitMenuOperation::Checkout,
                CommitMenuOperation::CherryPick,
                CommitMenuOperation::Revert,
                CommitMenuOperation::Reword,
                CommitMenuOperation::Fixup,
            ],
            "the buttons run the commit menu's own operations"
        );

        // HEAD leads with amend and has nothing to cherry-pick.
        let head = CommitFacts {
            is_head: true,
            ..plain.clone()
        };
        let buttons = commit_bar_buttons(&single(head), true);
        assert_eq!(buttons[0].action, CommitBarAction::Amend);
        assert_eq!(buttons[0].label.as_ref(), "Amend Last Commit");
        assert!(
            refusal(
                &buttons,
                CommitBarAction::Menu(CommitMenuOperation::CherryPick)
            )
            .is_some()
        );
        assert!(refusal(&buttons, CommitBarAction::Menu(CommitMenuOperation::Revert)).is_none());

        // A merge commit can be neither cherry-picked nor reverted without `-m`.
        let merge = CommitFacts {
            is_merge: Some(true),
            ..plain.clone()
        };
        let buttons = commit_bar_buttons(&single(merge.clone()), true);
        for operation in [CommitMenuOperation::CherryPick, CommitMenuOperation::Revert] {
            let menu_refusal = commit_operation_refusal(operation, &merge);
            assert!(menu_refusal.is_some(), "{operation:?}");
            assert_eq!(
                refusal(&buttons, CommitBarAction::Menu(operation)),
                menu_refusal,
                "the button says what the menu's item says"
            );
        }
        assert!(
            !buttons
                .iter()
                .any(|button| button.action == CommitBarAction::Amend)
        );

        // The fixup button says what the menu's fixup item says.
        let nothing_to_fold = CommitFacts {
            fixup_refusal: Some("There is nothing to put into the commit.".into()),
            ..plain
        };
        let buttons = commit_bar_buttons(&single(nothing_to_fold), true);
        assert_eq!(
            refusal(&buttons, CommitBarAction::Menu(CommitMenuOperation::Fixup)).as_deref(),
            Some("There is nothing to put into the commit.")
        );

        // A file-history tab has no merge-request box.
        let buttons = commit_bar_buttons(&single(plain), false);
        assert!(refusal(&buttons, CommitBarAction::MergeRequestBox).is_some());
        assert_eq!(commit_bar_buttons(&CommitBarSelection::Nothing, true), []);
    }

    #[test]
    fn test_commit_bar_buttons_for_several_commits() {
        let selection = |contiguous, on_current_branch| CommitBarSelection::Multiple {
            count: 3,
            rewrite: RangeRewriteFacts {
                contiguous,
                on_current_branch,
                head_is_branch: true,
                worktree_dirty: false,
            },
        };
        let buttons = commit_bar_buttons(&selection(true, true), true);
        assert_eq!(
            labels(&buttons),
            [
                "Cherry-Pick 3 Commits",
                "Reorder / Squash / Drop…",
                "Add to MR Box (3 Commits)",
            ]
        );
        assert!(buttons.iter().all(|button| button.refusal.is_none()));

        let buttons = commit_bar_buttons(&selection(false, true), true);
        assert_eq!(
            refusal(&buttons, CommitBarAction::RewriteSelection).as_deref(),
            Some("Pick an unbroken range.")
        );
        assert!(refusal(&buttons, CommitBarAction::CherryPickSelection).is_none());
        let buttons = commit_bar_buttons(&selection(true, false), true);
        assert_eq!(
            refusal(&buttons, CommitBarAction::RewriteSelection).as_deref(),
            Some("Pick commits on the current branch.")
        );
    }

    #[test]
    fn test_range_rewrite_refusal() {
        let facts = |contiguous, on_current_branch| RangeRewriteFacts {
            contiguous,
            on_current_branch,
            head_is_branch: true,
            worktree_dirty: false,
        };
        assert_eq!(range_rewrite_refusal(facts(true, true)), None);
        assert_eq!(
            range_rewrite_refusal(facts(false, true)),
            Some("Pick an unbroken range")
        );
        assert_eq!(
            range_rewrite_refusal(facts(false, false)),
            Some("Pick an unbroken range")
        );
        assert_eq!(
            range_rewrite_refusal(facts(true, false)),
            Some("Pick commits on the current branch")
        );
        // What the rewrite would refuse only once asked is refused up front as well.
        assert_eq!(
            range_rewrite_refusal(RangeRewriteFacts {
                head_is_branch: false,
                ..facts(true, true)
            }),
            Some("A detached HEAD cannot be rewritten. Switch to a branch first")
        );
        assert_eq!(
            range_rewrite_refusal(RangeRewriteFacts {
                worktree_dirty: true,
                ..facts(true, true)
            }),
            Some("Commit or stash the uncommitted changes first")
        );
        assert_eq!(
            range_rewrite_refusal(RangeRewriteFacts {
                head_is_branch: false,
                worktree_dirty: true,
                ..facts(false, true)
            }),
            Some("Pick an unbroken range"),
            "the selection itself is what to fix first"
        );
    }

    #[test]
    fn test_inline_button_count_folds_what_does_not_fit() {
        let buttons = commit_bar_buttons(
            &single(CommitFacts {
                is_merge: Some(false),
                ..CommitFacts::default()
            }),
            true,
        );
        let leading = px(60.);
        assert_eq!(inline_button_count(&buttons, leading, None), buttons.len());
        assert_eq!(
            inline_button_count(&buttons, leading, Some(px(5000.))),
            buttons.len()
        );
        assert_eq!(inline_button_count(&buttons, leading, Some(px(0.))), 0);

        let narrow = inline_button_count(&buttons, leading, Some(px(500.)));
        assert!(0 < narrow && narrow < buttons.len(), "{narrow}");
        // What stays inline, with "More ▾" after it, fits.
        let used = buttons[..narrow]
            .iter()
            .map(|button| {
                estimated_button_width(&button.label)
                    + if button.action == CommitBarAction::ResetMenu {
                        CHEVRON_WIDTH
                    } else {
                        px(0.)
                    }
                    + BAR_GAP
            })
            .fold(
                leading + estimated_button_width(more_button_label()) + CHEVRON_WIDTH + BAR_GAP,
                |sum, width| sum + width,
            );
        assert!(used <= px(500.));
        assert!(estimated_button_width("取り込む") > estimated_button_width("abcd"));
    }

    fn commit(sha: u8, parents: &[u8]) -> Arc<InitialGraphCommitData> {
        Arc::new(InitialGraphCommitData {
            sha: Oid::from_bytes(&[sha; 20]).unwrap(),
            parents: parents
                .iter()
                .map(|parent| Oid::from_bytes(&[*parent; 20]).unwrap())
                .collect(),
            ref_names: Vec::new(),
        })
    }

    fn click(
        selector: &'static str,
        multi_workspace: &Entity<workspace::MultiWorkspace>,
        cx: &mut VisualTestContext,
    ) {
        draw_workspace(multi_workspace, cx);
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} is on screen"));
        cx.simulate_click(bounds.center(), Modifiers::none());
        cx.run_until_parked();
    }

    fn select_rows(
        graph: &Entity<GitGraph>,
        rows: &[usize],
        multi_workspace: &Entity<workspace::MultiWorkspace>,
        cx: &mut VisualTestContext,
    ) {
        graph.update(cx, |graph, cx| {
            if rows.len() == 1 {
                graph.multi_selection.clear();
                graph.selected_entry_idx = rows.first().copied();
            } else {
                graph.select_rows_for_test(rows);
            }
            cx.notify();
        });
        draw_workspace(multi_workspace, cx);
    }

    /// A button runs what the commit menu's item of the same name runs: the same git command,
    /// with the same arguments.
    #[gpui::test]
    async fn test_bar_buttons_run_what_the_commit_menu_runs(cx: &mut TestAppContext) {
        let commits = vec![commit(3, &[2]), commit(2, &[1]), commit(1, &[])];
        let picked = commits[1].sha.to_string();
        let (fs, _repository, multi_workspace, graph, cx) = open_graph(commits, cx).await;

        assert!(
            cx.debug_bounds("git-graph-action-bar").is_some(),
            "the row is there before anything is picked"
        );
        assert!(cx.debug_bounds("git-graph-action-revert").is_none());

        select_rows(&graph, &[1], &multi_workspace, cx);
        click("git-graph-action-revert", &multi_workspace, cx);
        let from_button = git_invocations(&fs, "revert");
        assert_eq!(from_button, [vec!["revert".to_string(), picked.clone()]]);

        // The same item of the right-click menu: 差分を表示, SHA をコピー, then (タグをコピー is
        // greyed out without tags) ブランチ, チェックアウト, タグ, cherry-pick and revert.
        graph.update_in(cx, |graph, window, cx| {
            graph.deploy_entry_context_menu(point(px(20.), px(20.)), 1, None, window, cx);
        });
        cx.run_until_parked();
        let menu = graph.read_with(&*cx, |graph, _| {
            graph
                .context_menu
                .as_ref()
                .expect("the menu is open")
                .menu
                .clone()
        });
        menu.update_in(cx, |menu, window, cx| {
            menu.select_first(&SelectFirst, window, cx);
            for _ in 0..6 {
                menu.select_next(&SelectNext, window, cx);
            }
            menu.confirm(&menu::Confirm, window, cx);
        });
        cx.run_until_parked();
        let from_both = git_invocations(&fs, "revert");
        assert_eq!(from_both.len(), 2, "{from_both:?}");
        assert_eq!(from_both[0], from_both[1]);

        select_rows(&graph, &[1], &multi_workspace, cx);
        click("git-graph-action-checkout", &multi_workspace, cx);
        assert_eq!(
            git_invocations(&fs, "checkout"),
            [vec!["checkout".to_string(), "--detach".to_string(), picked]]
        );
    }

    /// A button the operation cannot run for stays greyed out: pressing it runs nothing.
    #[gpui::test]
    async fn test_refused_bar_buttons_run_nothing(cx: &mut TestAppContext) {
        let commits = vec![
            commit(4, &[3, 2]),
            commit(3, &[1]),
            commit(2, &[1]),
            commit(1, &[]),
        ];
        let (fs, _repository, multi_workspace, graph, cx) = open_graph(commits, cx).await;

        select_rows(&graph, &[0], &multi_workspace, cx);
        click("git-graph-action-cherry-pick", &multi_workspace, cx);
        click("git-graph-action-revert", &multi_workspace, cx);
        assert!(git_invocations(&fs, "cherry-pick").is_empty());
        assert!(git_invocations(&fs, "revert").is_empty());

        // A plain commit next to it is not held back.
        select_rows(&graph, &[1], &multi_workspace, cx);
        click("git-graph-action-revert", &multi_workspace, cx);
        assert_eq!(git_invocations(&fs, "revert").len(), 1);
    }

    #[gpui::test]
    async fn test_bar_buttons_for_head_and_for_several_commits(cx: &mut TestAppContext) {
        let commits = vec![commit(3, &[2]), commit(2, &[1]), commit(1, &[])];
        let shas = commits.iter().map(|commit| commit.sha).collect::<Vec<_>>();
        let (fs, _repository, multi_workspace, graph, cx) = open_graph(commits, cx).await;
        fs.set_branch_name(Path::new("/project/.git"), Some("main"));
        fs.set_head_for_repo(Path::new("/project/.git"), &[], shas[0].to_string());
        cx.run_until_parked();

        select_rows(&graph, &[0], &multi_workspace, cx);
        assert!(cx.debug_bounds("git-graph-action-amend").is_some());
        click("git-graph-action-cherry-pick", &multi_workspace, cx);
        assert!(
            git_invocations(&fs, "cherry-pick").is_empty(),
            "HEAD has nothing to cherry-pick"
        );
        select_rows(&graph, &[1], &multi_workspace, cx);
        assert!(cx.debug_bounds("git-graph-action-amend").is_none());

        // Two picked rows: the multi-selection menu's cherry-pick, oldest first, after asking.
        select_rows(&graph, &[0, 2], &multi_workspace, cx);
        assert!(cx.debug_bounds("git-graph-action-revert").is_none());
        click(
            "git-graph-action-cherry-pick-selection",
            &multi_workspace,
            cx,
        );
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Cherry-Pick");
        cx.run_until_parked();
        assert_eq!(
            git_invocations(&fs, "cherry-pick"),
            [vec![
                "cherry-pick".to_string(),
                shas[2].to_string(),
                shas[0].to_string(),
            ]]
        );

        // Rows with a gap cannot be rewritten together. (The cherry-pick reloaded the graph, which
        // drops the selection.)
        select_rows(&graph, &[0, 2], &multi_workspace, cx);
        click("git-graph-action-rewrite-selection", &multi_workspace, cx);
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());
        workspace.read_with(&*cx, |workspace, cx| {
            assert!(workspace.active_modal::<RewriteRangeModal>(cx).is_none());
        });
    }

    /// The amend button amends through the git panel, which works on its own active repository:
    /// while that is another repository than the graph's, the button refuses instead of starting
    /// an amend of the other repository's HEAD.
    #[gpui::test]
    async fn test_amend_button_only_amends_the_graphs_repository(cx: &mut TestAppContext) {
        let commits = vec![commit(2, &[1]), commit(1, &[])];
        let head = commits[0].sha.to_string();
        let (fs, repository, multi_workspace, graph, cx) = open_graph(commits, cx).await;
        fs.set_branch_name(Path::new("/project/.git"), Some("main"));
        fs.set_head_for_repo(Path::new("/project/.git"), &[], head);
        fs.insert_tree(
            Path::new("/other"),
            serde_json::json!({
                ".git": {},
                "other.txt": "content",
            }),
        )
        .await;
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = crate::git_panel::GitPanel::new_test(workspace, window, cx);
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        let project = workspace.read_with(&*cx, |workspace, _| workspace.project().clone());
        project
            .update(cx, |project, cx| {
                project.find_or_create_worktree("/other", true, cx)
            })
            .await
            .expect("the second repository opens");
        cx.run_until_parked();
        let other = project
            .read_with(&*cx, |project, cx| {
                project
                    .repositories(cx)
                    .values()
                    .find(|repository| {
                        repository.read(cx).work_directory_abs_path.as_ref() == Path::new("/other")
                    })
                    .cloned()
            })
            .expect("the second repository is known");
        other.update(cx, |other, cx| other.set_as_active_repository(cx));
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        cx.run_until_parked();

        select_rows(&graph, &[0], &multi_workspace, cx);
        click("git-graph-action-amend", &multi_workspace, cx);
        panel.read_with(&*cx, |panel, _| {
            assert!(
                !panel.amend_pending(),
                "the panel shows the other repository, whose HEAD is not this graph's"
            );
        });

        repository.update(cx, |repository, cx| repository.set_as_active_repository(cx));
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        cx.run_until_parked();
        select_rows(&graph, &[0], &multi_workspace, cx);
        click("git-graph-action-amend", &multi_workspace, cx);
        panel.read_with(&*cx, |panel, _| assert!(panel.amend_pending()));
    }

    /// The row is as tall with nothing picked as with the buttons of a pick, and opening the MR
    /// box next to the graph, in a tab too short for the box's contents, leaves it so with its
    /// buttons whole inside it.
    #[gpui::test]
    async fn test_bar_keeps_its_height_when_the_box_opens(cx: &mut TestAppContext) {
        let commits = vec![commit(3, &[2]), commit(2, &[1]), commit(1, &[])];
        let (_fs, _repository, multi_workspace, graph, cx) = open_graph(commits, cx).await;
        cx.simulate_resize(gpui::size(px(1200.), px(500.)));
        graph.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        let nothing_picked = cx
            .debug_bounds("git-graph-action-bar")
            .expect("the row is there before anything is picked");
        select_rows(&graph, &[1], &multi_workspace, cx);
        let measure = |window_height: Option<Pixels>, cx: &mut VisualTestContext| {
            // The row measures its width on the first frame and settles on the next.
            for _ in 0..2 {
                graph.update(cx, |_, cx| cx.notify());
                cx.run_until_parked();
            }
            let bar = cx
                .debug_bounds("git-graph-action-bar")
                .expect("the row is on screen");
            let button = cx
                .debug_bounds("git-graph-action-branch")
                .expect("Branch from Here is on screen");
            assert!(
                bar.top() <= button.top() && button.bottom() <= bar.bottom(),
                "{button:?} is cut off by {bar:?}"
            );
            let description = cx
                .debug_bounds("git-graph-action-description")
                .expect("the line under the row is on screen");
            // Keeping the rows whole is not enough: a graph (or box) taller than what is left
            // would run off the bottom of the tab, out of reach of its own scrolling.
            if let Some(window_height) = window_height {
                let content = cx
                    .debug_bounds("git-graph-content")
                    .expect("the graph is on screen");
                assert!(
                    content.bottom() <= window_height,
                    "{content:?} runs past the window's {window_height:?}"
                );
            }
            (bar.size.height, description.size.height)
        };
        let closed = measure(Some(px(500.)), cx);
        assert_eq!(closed.0, nothing_picked.size.height);

        graph.update_in(cx, |graph, window, cx| {
            graph.toggle_merge_request_box(window, cx);
        });
        cx.run_until_parked();
        assert!(graph.read_with(&*cx, |graph, _| graph.visible_merge_request_box().is_some()));
        let open = measure(Some(px(500.)), cx);
        assert_eq!(open, closed);

        // Shorter still, the rows keep their heights and the graph and the box give way.
        cx.simulate_resize(gpui::size(px(1200.), px(400.)));
        let short = measure(Some(px(400.)), cx);
        assert_eq!(short, closed);

        // Too short for the rows themselves, the graph has nothing left to give, and the row
        // still does not give way to the rows above it.
        cx.simulate_resize(gpui::size(px(1200.), px(160.)));
        let cramped = measure(None, cx);
        assert_eq!(cramped, closed);
    }

    #[gpui::test]
    async fn test_hints_stay_away_once_dismissed(cx: &mut TestAppContext) {
        // A database of its own: the one tests share would carry the dismissal into other tests.
        cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
        let commits = vec![commit(2, &[1]), commit(1, &[])];
        let (_fs, repository, multi_workspace, graph, cx) = open_graph(commits, cx).await;
        assert!(cx.debug_bounds("git-graph-hints").is_some());

        click("git-graph-hints-dismiss", &multi_workspace, cx);
        draw_workspace(&multi_workspace, cx);
        assert!(cx.debug_bounds("git-graph-hints").is_none());
        assert!(cx.update(|_, cx| GitGraphHints::dismissed(cx)));

        // A graph opened afterwards starts without them.
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());
        let git_store = graph.read_with(&*cx, |graph, _| graph.git_store.clone());
        let reopened = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                git_store,
                workspace.downgrade(),
                None,
                window,
                cx,
            )
        });
        reopened.read_with(&*cx, |graph, _| assert!(!graph.guide.hints_visible));
    }

    /// Wider than this, the line under the row would not fit next to the details panel at 1280px.
    const DESCRIPTION_LINE_WIDTH: Pixels = px(900.);

    #[test]
    fn test_every_bar_button_is_explained() {
        let plain = CommitFacts {
            is_head: false,
            is_merge: Some(false),
            fixup_refusal: None,
        };
        let selections = [
            single(plain.clone()),
            single(CommitFacts {
                is_head: true,
                ..plain.clone()
            }),
            single(CommitFacts {
                is_merge: Some(true),
                ..plain
            }),
            CommitBarSelection::Multiple {
                count: 2,
                rewrite: RangeRewriteFacts {
                    contiguous: true,
                    on_current_branch: true,
                    head_is_branch: true,
                    worktree_dirty: false,
                },
            },
        ];
        let mut explained = Vec::new();
        for selection in &selections {
            for button in commit_bar_buttons(selection, true) {
                assert_eq!(button.help, commit_bar_help(button.action));
                if !explained.contains(&button.action) {
                    explained.push(button.action);
                }
            }
        }
        let every_action = [
            CommitBarAction::Amend,
            CommitBarAction::Menu(CommitMenuOperation::CreateBranch),
            CommitBarAction::Menu(CommitMenuOperation::Checkout),
            CommitBarAction::Menu(CommitMenuOperation::CherryPick),
            CommitBarAction::Menu(CommitMenuOperation::Revert),
            CommitBarAction::Menu(CommitMenuOperation::Reword),
            CommitBarAction::Menu(CommitMenuOperation::Fixup),
            CommitBarAction::ResetMenu,
            CommitBarAction::MergeRequestBox,
            CommitBarAction::CherryPickSelection,
            CommitBarAction::RewriteSelection,
        ];
        for action in every_action {
            assert!(explained.contains(&action), "{action:?} is on the row");
        }
        assert_eq!(explained.len(), every_action.len(), "{explained:?}");
        // The row's own buttons carry the words of what they do, so that two of them trading
        // explanations is caught (the commit menu's operations are checked in `git_terms`).
        for (action, in_title, in_explanation) in [
            (CommitBarAction::Amend, "Amend", "last commit"),
            (CommitBarAction::ResetMenu, "Reset", "soft / mixed / hard"),
            (CommitBarAction::MergeRequestBox, "MR Box", "for the MR"),
            (
                CommitBarAction::CherryPickSelection,
                "Cherry-Pick",
                "oldest first",
            ),
            (CommitBarAction::RewriteSelection, "Reorder", "squashes"),
        ] {
            let help = commit_bar_help(action);
            assert!(help.title.contains(in_title), "{action:?}: {help:?}");
            assert!(
                help.explanation.contains(in_explanation),
                "{action:?}: {help:?}"
            );
        }

        let helps = every_action
            .iter()
            .map(|action| commit_bar_help(*action))
            .chain(
                [
                    CommitMenuOperation::CreateTag,
                    CommitMenuOperation::Drop,
                    CommitMenuOperation::ResetSoft,
                    CommitMenuOperation::ResetMixed,
                    CommitMenuOperation::ResetHard,
                    CommitMenuOperation::CopyTagDistance,
                ]
                .map(commit_operation_help),
            )
            .chain(
                [
                    ToolbarHelp::LogSource,
                    ToolbarHelp::TagList,
                    ToolbarHelp::CompareRefs,
                    ToolbarHelp::MergeRequest,
                ]
                .map(ToolbarHelp::help),
            )
            .chain([more_menu_help(), crate::git_terms::squash_selection_help()])
            .collect::<Vec<_>>();
        for (index, help) in helps.iter().enumerate() {
            assert!(!help.title.is_empty() && !help.explanation.is_empty());
            assert!(
                !help.explanation.ends_with('.'),
                "the sentence adds the full stop: {help:?}"
            );
            assert!(
                estimated_button_width(&help.line()) <= DESCRIPTION_LINE_WIDTH,
                "fits on the line under the row: {}",
                help.line()
            );
            assert!(
                helps[..index]
                    .iter()
                    .all(|other| other.explanation != help.explanation),
                "no two operations share an explanation: {help:?}"
            );
        }
        for text in [
            single_selection_description().to_string(),
            action_description(
                &selections[3],
                &commit_bar_buttons(&selections[3], true),
                &[0, 1],
                None,
            )
            .to_string(),
        ] {
            assert!(
                estimated_button_width(&text) <= DESCRIPTION_LINE_WIDTH,
                "{text}"
            );
        }
    }

    #[test]
    fn test_action_description_follows_the_hovered_control() {
        let facts = CommitFacts {
            is_head: false,
            is_merge: Some(true),
            fixup_refusal: None,
        };
        let revert_refusal = commit_operation_refusal(CommitMenuOperation::Revert, &facts)
            .expect("a merge commit cannot be reverted");
        let selection = single(facts);
        let buttons = commit_bar_buttons(&selection, true);
        let describe = |hovered: Option<HoveredHelp>| {
            action_description(&selection, &buttons, &[1], hovered.as_ref()).to_string()
        };
        let hovering = |action| HoveredHelp::BarButton {
            action,
            rows: vec![1],
        };

        assert_eq!(describe(None), single_selection_description());
        assert_eq!(
            describe(Some(hovering(CommitBarAction::Menu(
                CommitMenuOperation::Checkout
            )))),
            commit_operation_help(CommitMenuOperation::Checkout).line()
        );
        assert_eq!(
            describe(Some(hovering(CommitBarAction::Menu(
                CommitMenuOperation::Revert
            )))),
            format!("\"Revert\" is unavailable right now: {revert_refusal}"),
            "a greyed-out button says why"
        );
        assert_eq!(
            describe(Some(HoveredHelp::BarButton {
                action: CommitBarAction::Menu(CommitMenuOperation::Checkout),
                rows: vec![2],
            })),
            single_selection_description(),
            "a hover left over from another selection is not this selection's"
        );
        assert_eq!(
            describe(Some(hovering(CommitBarAction::CherryPickSelection))),
            single_selection_description(),
            "a button this selection does not have explains nothing"
        );
        assert_eq!(
            describe(Some(HoveredHelp::Toolbar(ToolbarHelp::CompareRefs))),
            compare_refs_help().line()
        );
        assert_eq!(
            action_description(&CommitBarSelection::Nothing, &[], &[], None),
            SharedString::default()
        );
    }

    /// Moving the mouse onto a button explains it on the line under the row, and off it brings the
    /// selection's hint back, without the line (and so the table) moving.
    #[gpui::test]
    async fn test_hovering_a_bar_button_explains_it(cx: &mut TestAppContext) {
        let commits = vec![commit(3, &[2]), commit(2, &[1]), commit(1, &[])];
        let (_fs, _repository, multi_workspace, graph, cx) = open_graph(commits, cx).await;
        let description = |graph: &Entity<GitGraph>, cx: &mut VisualTestContext| {
            graph.update(cx, |graph, cx| {
                graph.action_description_text(cx).to_string()
            })
        };

        draw_workspace(&multi_workspace, cx);
        let empty_bounds = cx
            .debug_bounds("git-graph-action-description")
            .expect("the line is there before anything is picked");
        assert_eq!(description(&graph, cx), "");

        select_rows(&graph, &[1], &multi_workspace, cx);
        assert_eq!(description(&graph, cx), single_selection_description());
        let picked_bounds = cx
            .debug_bounds("git-graph-action-description")
            .expect("the line stays");
        assert_eq!(
            picked_bounds.size, empty_bounds.size,
            "an empty line is as high as a full one"
        );
        let revert = cx
            .debug_bounds("git-graph-action-revert")
            .expect("the revert button is on screen");
        cx.simulate_mouse_move(revert.center(), None, Modifiers::none());
        draw_workspace(&multi_workspace, cx);
        assert_eq!(
            description(&graph, cx),
            commit_operation_help(CommitMenuOperation::Revert).line()
        );
        let hovered_bounds = cx
            .debug_bounds("git-graph-action-description")
            .expect("the line stays");
        assert_eq!(
            hovered_bounds, picked_bounds,
            "hovering never moves the line"
        );

        let checkout = cx
            .debug_bounds("git-graph-action-checkout")
            .expect("the checkout button is on screen");
        cx.simulate_mouse_move(checkout.center(), None, Modifiers::none());
        draw_workspace(&multi_workspace, cx);
        assert_eq!(
            description(&graph, cx),
            commit_operation_help(CommitMenuOperation::Checkout).line()
        );

        cx.simulate_mouse_move(hovered_bounds.center(), None, Modifiers::none());
        draw_workspace(&multi_workspace, cx);
        assert_eq!(description(&graph, cx), single_selection_description());

        // The toolbar's buttons explain themselves on the same line.
        let compare = cx
            .debug_bounds("git-graph-compare-refs-hover")
            .expect("the compare button is on screen");
        cx.simulate_mouse_move(compare.center(), None, Modifiers::none());
        draw_workspace(&multi_workspace, cx);
        assert_eq!(description(&graph, cx), compare_refs_help().line());
        assert_eq!(
            cx.debug_bounds("git-graph-action-description"),
            Some(picked_bounds)
        );
    }
}
