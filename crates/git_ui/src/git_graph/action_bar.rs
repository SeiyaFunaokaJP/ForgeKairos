//! The row of labelled buttons for the picked commits, the first-run hints and the drag grips:
//! the graph's operations were otherwise only reachable through right-click menus and drags
//! nothing on screen hinted at.

use super::*;
use crate::commit_context_menu::{
    CommitFacts, CommitMenuOperation, commit_operation_refusal, run_commit_menu_operation,
};
use db::kvp::Dismissable;
use gpui::canvas;

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
}

impl GraphGuideState {
    pub(super) fn new(log_source: &LogSource, cx: &App) -> Self {
        Self {
            hints_visible: !matches!(log_source, LogSource::Path(_))
                && !GitGraphHints::dismissed(cx),
            bar_width: Rc::new(Cell::new(None)),
            selection_facts: None,
        }
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
    pub(super) tooltip: SharedString,
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
        contiguous: bool,
        on_current_branch: bool,
    },
}

/// Why the todo editor (and squashing) cannot take the picked rows: both rewrite the checked-out
/// branch from the oldest picked commit up, so the rows have to be one unbroken chain on it.
pub(super) fn range_rewrite_refusal(
    contiguous: bool,
    on_current_branch: bool,
) -> Option<&'static str> {
    if !contiguous {
        Some("連続した範囲を選んでください")
    } else if !on_current_branch {
        Some("現在のブランチ上のコミットを選んでください")
    } else {
        None
    }
}

const MERGE_REQUEST_BOX_REFUSAL: &str =
    "ファイル履歴のタブでは MR の箱を使えません。コミットグラフのタブで開いてください。";

/// The buttons the row shows for `selection`, in order.
pub(super) fn commit_bar_buttons(
    selection: &CommitBarSelection,
    can_hold_merge_request_box: bool,
) -> Vec<CommitBarButton> {
    let button =
        |action, label: &str, tooltip: &str, refusal: Option<SharedString>| CommitBarButton {
            action,
            label: SharedString::from(label.to_string()),
            tooltip: SharedString::from(tooltip.to_string()),
            refusal,
        };
    let merge_request_box_refusal =
        (!can_hold_merge_request_box).then_some(SharedString::from(MERGE_REQUEST_BOX_REFUSAL));
    match selection {
        CommitBarSelection::Nothing => Vec::new(),
        CommitBarSelection::Single { facts } => {
            let menu = |operation, label: &str, tooltip: &str| {
                button(
                    CommitBarAction::Menu(operation),
                    label,
                    tooltip,
                    commit_operation_refusal(operation, facts),
                )
            };
            let mut buttons = Vec::new();
            if facts.is_head {
                buttons.push(button(
                    CommitBarAction::Amend,
                    "直前のコミットを修正 (amend)",
                    "Git パネルで、今の変更とメッセージで直前のコミットを置き換えます",
                    None,
                ));
            }
            buttons.extend([
                menu(
                    CommitMenuOperation::CreateBranch,
                    "ここからブランチ",
                    "このコミットを基点に新しいブランチを作ります",
                ),
                menu(
                    CommitMenuOperation::Checkout,
                    "チェックアウト",
                    "このコミットをチェックアウトします (detached HEAD になります)",
                ),
                menu(
                    CommitMenuOperation::CherryPick,
                    "取り込む (cherry-pick)",
                    "このコミットの変更を現在のブランチに新しいコミットとして積みます",
                ),
                menu(
                    CommitMenuOperation::Revert,
                    "打ち消す (revert)",
                    "このコミットの変更を打ち消すコミットを作ります (履歴は書き換えません)",
                ),
                menu(
                    CommitMenuOperation::Reword,
                    "メッセージを書き換え",
                    "このコミットのメッセージを書き換えます (以降のコミットの SHA が変わります)",
                ),
                menu(
                    CommitMenuOperation::Fixup,
                    "今の変更を入れる (fixup)",
                    "今の変更をこのコミットに入れます (以降のコミットの SHA が変わります)",
                ),
                button(
                    CommitBarAction::ResetMenu,
                    "このコミットまで戻す",
                    "現在のブランチをこのコミットまで戻します (soft / mixed / hard を選びます)",
                    None,
                ),
                button(
                    CommitBarAction::MergeRequestBox,
                    "MR の箱に入れる",
                    "このコミットを MR の箱に入れます",
                    merge_request_box_refusal,
                ),
            ]);
            buttons
        }
        CommitBarSelection::Multiple {
            count,
            contiguous,
            on_current_branch,
        } => vec![
            button(
                CommitBarAction::CherryPickSelection,
                &format!("{count} 件を取り込む"),
                "選んだコミットの変更を、古い順に現在のブランチに積みます (cherry-pick)",
                None,
            ),
            button(
                CommitBarAction::RewriteSelection,
                "並べ替え / まとめる / 捨てる…",
                "選んだ範囲のコミットの順番・まとめ方・メッセージを編集します",
                range_rewrite_refusal(*contiguous, *on_current_branch)
                    .map(|reason| SharedString::from(format!("{reason}。"))),
            ),
            button(
                CommitBarAction::MergeRequestBox,
                &format!("MR の箱に入れる ({count} 件)"),
                "選んだコミットを MR の箱に入れます",
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
const MORE_BUTTON_LABEL: &str = "その他";
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
    let more = estimated_button_width(MORE_BUTTON_LABEL) + CHEVRON_WIDTH + BAR_GAP;
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
                    contiguous,
                    on_current_branch,
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
                                "Git パネルでは別のリポジトリが選ばれています。Git パネルでこのリポジトリに切り替えてから修正 (amend) してください。"
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
            (CommitBarSelection::Nothing, _) => {
                "コミットを選ぶと、ここに操作ボタンが出ます (右クリックでも同じ操作ができます)"
                    .into()
            }
            (CommitBarSelection::Single { .. }, Some(sha)) => {
                format!("{}:", sha.display_short()).into()
            }
            (CommitBarSelection::Multiple { count, .. }, _) => format!("{count} 件:").into(),
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
            .child(
                Label::new(leading_label)
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate(),
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
        let tooltip = button.refusal.clone().unwrap_or(button.tooltip.clone());
        if button.action == CommitBarAction::ResetMenu {
            let Some(sha) = sha else {
                return Empty.into_any_element();
            };
            let workspace = self.workspace.clone();
            return div()
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
        div()
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
        div()
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
                                menu = match button.refusal.clone() {
                                    Some(reason) => menu.item(
                                        ContextMenuEntry::new(button.label.clone())
                                            .disabled(true)
                                            .documentation_aside(
                                                ui::DocumentationSide::Right,
                                                move |_cx| {
                                                    Label::new(reason.clone()).into_any_element()
                                                },
                                            ),
                                    ),
                                    None => {
                                        let handler = handler.clone();
                                        menu.entry(button.label.clone(), None, move |window, cx| {
                                            handler(window, cx)
                                        })
                                    }
                                };
                            }
                            menu
                        }))
                    })
                    .trigger_with_tooltip(
                        Button::new("git-graph-action-more", MORE_BUTTON_LABEL)
                            .label_size(LabelSize::Small)
                            .end_icon(
                                Icon::new(IconName::ChevronDown)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            ),
                        Tooltip::text("幅が足りずに入らなかった操作"),
                    ),
            )
            .into_any_element()
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
                        .child(Label::new("コミットグラフの使い方").size(LabelSize::Small))
                        .child(div().flex_1())
                        .child(
                            Button::new("git-graph-hints-guide", "操作ガイド")
                                .label_size(LabelSize::Small)
                                .tooltip(Tooltip::text("やりたいことから Git の操作を探せます"))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(git::ShowGuide.boxed_clone(), cx)
                                }),
                        )
                        .child(
                            div()
                                .debug_selector(|| "git-graph-hints-dismiss".into())
                                .child(
                                    Button::new("git-graph-hints-dismiss", "次回から表示しない")
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
                                .tooltip(Tooltip::text("閉じる (次に開いたときはまた出ます)"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.guide.hints_visible = false;
                                    cx.notify();
                                })),
                        ),
                )
                .child(hint("コミットを選ぶと上に操作ボタンが出ます"))
                .child(hint(
                    "コミットはドラッグでブランチに取り込み・並べ替え・MR の箱へ",
                ))
                .child(hint(
                    "Git パネルのファイルをコミットに落とすと、そのコミットに修正を足せます (fixup)",
                ))
                .into_any_element(),
        )
    }

    /// Where `git::CompareRefs` lands: the graph's own "比較…" button, or why it cannot compare.
    pub(crate) fn start_ref_comparison(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.comparable_refs(cx).len() < 2 {
            self.report_refused(
                "比べるには、ブランチかタグが 2 つ以上必要です。".to_string(),
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
        (CommitMenuOperation::ResetSoft, "soft: 変更をステージに残す"),
        (
            CommitMenuOperation::ResetMixed,
            "mixed: 変更を作業ツリーに残す",
        ),
        (CommitMenuOperation::ResetHard, "hard: 変更を捨てる…"),
    ]
    .into_iter()
    .fold(menu, |menu, (operation, label)| {
        let repository = repository.clone();
        let workspace = workspace.clone();
        let sha = sha.clone();
        menu.entry(label, Some(operation.action(&sha)), move |window, cx| {
            let (Some(repository), Some(workspace)) = (
                repository
                    .as_ref()
                    .and_then(|repository| repository.upgrade()),
                workspace.upgrade(),
            ) else {
                return;
            };
            run_commit_menu_operation(operation, repository, workspace, sha.clone(), window, cx);
        })
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
                "ここからブランチ",
                "チェックアウト",
                "取り込む (cherry-pick)",
                "打ち消す (revert)",
                "メッセージを書き換え",
                "今の変更を入れる (fixup)",
                "このコミットまで戻す",
                "MR の箱に入れる",
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
        assert_eq!(buttons[0].label.as_ref(), "直前のコミットを修正 (amend)");
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
        let buttons = commit_bar_buttons(&single(merge), true);
        assert_eq!(
            refusal(
                &buttons,
                CommitBarAction::Menu(CommitMenuOperation::CherryPick)
            )
            .as_deref(),
            Some("マージコミットは取り込めません。")
        );
        assert_eq!(
            refusal(&buttons, CommitBarAction::Menu(CommitMenuOperation::Revert)).as_deref(),
            Some("マージコミットは打ち消せません。")
        );
        assert!(
            !buttons
                .iter()
                .any(|button| button.action == CommitBarAction::Amend)
        );

        // The fixup button says what the menu's fixup item says.
        let nothing_to_fold = CommitFacts {
            fixup_refusal: Some("コミットに入れる変更がありません。".into()),
            ..plain
        };
        let buttons = commit_bar_buttons(&single(nothing_to_fold), true);
        assert_eq!(
            refusal(&buttons, CommitBarAction::Menu(CommitMenuOperation::Fixup)).as_deref(),
            Some("コミットに入れる変更がありません。")
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
            contiguous,
            on_current_branch,
        };
        let buttons = commit_bar_buttons(&selection(true, true), true);
        assert_eq!(
            labels(&buttons),
            [
                "3 件を取り込む",
                "並べ替え / まとめる / 捨てる…",
                "MR の箱に入れる (3 件)",
            ]
        );
        assert!(buttons.iter().all(|button| button.refusal.is_none()));

        let buttons = commit_bar_buttons(&selection(false, true), true);
        assert_eq!(
            refusal(&buttons, CommitBarAction::RewriteSelection).as_deref(),
            Some("連続した範囲を選んでください。")
        );
        assert!(refusal(&buttons, CommitBarAction::CherryPickSelection).is_none());
        let buttons = commit_bar_buttons(&selection(true, false), true);
        assert_eq!(
            refusal(&buttons, CommitBarAction::RewriteSelection).as_deref(),
            Some("現在のブランチ上のコミットを選んでください。")
        );
    }

    #[test]
    fn test_range_rewrite_refusal() {
        assert_eq!(range_rewrite_refusal(true, true), None);
        assert_eq!(
            range_rewrite_refusal(false, true),
            Some("連続した範囲を選んでください")
        );
        assert_eq!(
            range_rewrite_refusal(false, false),
            Some("連続した範囲を選んでください")
        );
        assert_eq!(
            range_rewrite_refusal(true, false),
            Some("現在のブランチ上のコミットを選んでください")
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
        // What stays inline, with "その他 ▾" after it, fits.
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
                leading + estimated_button_width(MORE_BUTTON_LABEL) + CHEVRON_WIDTH + BAR_GAP,
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
        cx.simulate_prompt_answer("取り込む");
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
}
