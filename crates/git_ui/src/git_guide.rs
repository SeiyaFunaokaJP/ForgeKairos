use gpui::{
    Action, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, ScrollHandle,
    Stateful, Window,
};
use notifications::status_toast::StatusToast;
use project::git_store::Repository;
use ui::{Divider, Tooltip, prelude::*};
use workspace::{ModalView, Workspace};

use crate::git_graph::{GitGraph, open_or_reuse_graph};
use crate::git_panel::GitPanel;
use git::repository::LogSource;

/// What the grip beside a graph row says it can be dragged to.
pub(crate) const COMMIT_DRAG_HINT: &str = i18n::msg!(
    "Drag: onto a branch to cherry-pick / onto another row to reorder / into the MR box"
);
/// What the grip beside a git panel file says it can be dragged to.
pub(crate) const FILE_DRAG_HINT: &str =
    i18n::msg!("Drag: into Staged / onto a commit to fix it up");

/// The ⋮⋮ that says "this row can be dragged", with what dropping it does in its tooltip. The
/// caller decides when it shows; it is only a signpost, the drag itself belongs to the row.
/// `hint` is one of the hints above, in English: it is translated here.
pub(crate) fn drag_grip(id: ElementId, hint: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .flex()
        .items_center()
        .cursor_grab()
        .child(
            Icon::new(IconName::Menu)
                .size(IconSize::XSmall)
                .color(Color::Muted),
        )
        .tooltip(Tooltip::text(i18n::tr(hint)))
}

/// The `topic` of `git::ShowGuide` that the MR box's "?" asks for.
pub(crate) const MERGE_REQUEST_TOPIC: &str = "merge_request";

/// The "?" that opens the guide from wherever the user is stuck, with the entries about `topic`
/// first.
pub(crate) fn guide_button(id: impl Into<ElementId>, topic: Option<&'static str>) -> IconButton {
    let action = git::ShowGuide {
        topic: topic.map(str::to_string),
    };
    IconButton::new(id, IconName::CircleHelp)
        .icon_size(IconSize::Small)
        // The binding people can press anywhere is the plain one, not this topic's.
        .tooltip(|_window, cx| {
            Tooltip::for_action(
                i18n::t!("Git Guide (find an operation by what you want to do)"),
                &git::ShowGuide,
                cx,
            )
        })
        .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
}

/// One entry of the guide: something the user wants to do, how to do it, and the action that
/// starts it when there is one.
pub(crate) struct GuideTopic {
    /// English, translated where it is shown, as is `description`.
    pub(crate) title: &'static str,
    pub(crate) description: &'static str,
    pub(crate) action: Option<fn() -> Box<dyn Action>>,
    /// The `git::ShowGuide` topics this entry comes first for.
    pub(crate) topics: &'static [&'static str],
}

pub(crate) const GUIDE_TOPICS: &[GuideTopic] = &[
    GuideTopic {
        title: i18n::msg!("Fix the last commit (amend)"),
        description: i18n::msg!(
            "For when you notice something missing right after committing. Check \"Amend\" in the Git panel and the last commit's message comes back; committing then replaces the last commit."
        ),
        action: Some(|| git::StartAmend.boxed_clone()),
        topics: &[],
    },
    GuideTopic {
        title: i18n::msg!("Add a fix to an older commit (fixup)"),
        description: i18n::msg!(
            "Once you have a fix that belongs in a commit a few back, pick that commit in the Git Graph and press \"Add Current Changes (fixup)\". You can also drag files from the Git panel onto the commit's row."
        ),
        action: Some(|| git::ChooseFixupCommit.boxed_clone()),
        topics: &[],
    },
    GuideTopic {
        title: i18n::msg!("Fix a commit message (reword)"),
        description: i18n::msg!(
            "Opens a screen to rewrite the last commit's message. For an older commit, pick it in the Git Graph and press \"Reword Message\" (the SHAs of the commits from there on change)."
        ),
        action: Some(|| git::RewordCommit::default().boxed_clone()),
        topics: &[],
    },
    GuideTopic {
        title: i18n::msg!("Squash, reorder or drop commits (squash / drop)"),
        description: i18n::msg!(
            "In the Git Graph, pick rows while holding Ctrl / Shift and press \"Reorder / Squash / Drop…\" above to edit the order, what to squash and the messages. You can also reorder a single commit by dragging it onto another row. Mind that the changes of dropped commits are lost (the SHAs of the commits from there on change)."
        ),
        action: Some(|| crate::git_graph::Open.boxed_clone()),
        topics: &[],
    },
    GuideTopic {
        title: i18n::msg!("Bring over a commit from another branch (cherry-pick)"),
        description: i18n::msg!(
            "Pick the commit in the Git Graph and press \"Cherry-Pick\" to add its changes to the current branch as a new commit. With several picked through Ctrl / Shift, \"Cherry-Pick N Commits\" adds them oldest first. You can also drag a row onto a branch name. Changes to the same places can end in a conflict."
        ),
        action: Some(|| crate::git_graph::Open.boxed_clone()),
        topics: &[],
    },
    GuideTopic {
        title: i18n::msg!("Make one commit for an MR (MR box)"),
        description: i18n::msg!(
            "Collect a development branch's commits in the \"MR box\" and they are squashed into one commit on a new branch for the MR. The original branch stays as it is."
        ),
        action: Some(|| git::OpenMergeRequestBox.boxed_clone()),
        topics: &[MERGE_REQUEST_TOPIC],
    },
    GuideTopic {
        title: i18n::msg!("Using the MR box (add, exclude, create and compare)"),
        description: i18n::msg!(
            "1. Choose the \"development branch\" (where the commits come from) and the \"base\" (what the MR targets). 2. Drag rows of the Git Graph into the box, or pick them and press \"Add to MR Box\". Set aside what should stay out of the MR by right-clicking its row and choosing \"Exclude from MR\". 3. When \"unplaced\" under the box shows 0, nothing was forgotten (unplaced rows get a small ● in the graph, and \"Go to First Unplaced\" jumps there). 4. Enter a message and an MR branch name and press \"Create and Compare\": it creates the MR branch as one commit on top of the base and compares it with the development branch. It cannot be created while there are uncommitted changes, so commit or stash them first."
        ),
        action: Some(|| git::OpenMergeRequestBox.boxed_clone()),
        topics: &[MERGE_REQUEST_TOPIC],
    },
    GuideTopic {
        title: i18n::msg!("Recreate an MR branch"),
        description: i18n::msg!(
            "To create it under the same MR branch name, check \"Recreate the existing …\". An MR branch that was already pushed needs a force push (with lease) once recreated. The old MR branch can be restored from the reflog."
        ),
        action: Some(|| git::OpenMergeRequestBox.boxed_clone()),
        topics: &[MERGE_REQUEST_TOPIC],
    },
    GuideTopic {
        title: i18n::msg!("Create or switch branches"),
        description: i18n::msg!(
            "Switch from the branch list. Type a name that is not in the list to create a new branch. To start from an older commit, pick that commit in the Git Graph and press \"Branch from Here\"."
        ),
        action: Some(|| zed_actions::git::Switch.boxed_clone()),
        topics: &[],
    },
    GuideTopic {
        title: i18n::msg!("See an older commit as it was (checkout)"),
        description: i18n::msg!(
            "Pick a commit in the Git Graph and press \"Check Out\" to put the working tree in that commit's state (detached HEAD). Commits made from there belong to no branch, so to keep working, use \"Branch from Here\". To go back, switch to the original branch from the branch list."
        ),
        action: Some(|| crate::git_graph::Open.boxed_clone()),
        topics: &[],
    },
    GuideTopic {
        title: i18n::msg!("Undo a mistake (reset / revert / undo)"),
        description: i18n::msg!(
            "Pick a commit in the Git Graph, then press \"Revert\" to make a commit that undoes its changes, or \"Reset to This Commit ▾\" to move the branch back (hard throws the changes away). After a rewrite, \"Undo…\" on the notification or the reflog brings things back."
        ),
        action: Some(|| crate::git_graph::Open.boxed_clone()),
        topics: &[],
    },
    GuideTopic {
        title: i18n::msg!("How soft / mixed / hard differ in \"Reset to This Commit\""),
        description: i18n::msg!(
            "All three move the current branch back to the chosen commit and undo the commits after it. soft keeps the undone commits' changes as staged changes and mixed as unstaged ones, so you can commit them again. hard throws away the undone commits' changes and the changes you have not committed yet as well (uncommitted changes cannot be recovered)."
        ),
        action: None,
        topics: &[],
    },
    GuideTopic {
        title: i18n::msg!("I changed commits I had already pushed"),
        description: i18n::msg!(
            "After rewriting pushed commits with amend, fixup, reordering and so on, push with \"Force Push (with lease)\". Because of the lease, commits someone else pushed first are not overwritten."
        ),
        action: Some(|| git::ConfirmForcePush.boxed_clone()),
        topics: &[],
    },
    GuideTopic {
        title: i18n::msg!("Compare two branches"),
        description: i18n::msg!(
            "Choose two branches / tags to list the commits only one of them has and how their contents differ. Useful to check that nothing was left out when porting."
        ),
        action: Some(|| git::CompareRefs.boxed_clone()),
        topics: &[],
    },
    GuideTopic {
        title: i18n::msg!("Find or add a tag (tag)"),
        description: i18n::msg!(
            "Search for a tag in the Git Graph's \"Tags\" and pick it to jump to its commit. To add a tag, right-click the commit's row and choose \"Tag Here…\" (an empty message makes a lightweight tag)."
        ),
        action: Some(|| crate::git_graph::Open.boxed_clone()),
        topics: &[],
    },
    GuideTopic {
        title: i18n::msg!("Find past changes (search)"),
        description: i18n::msg!(
            "Search in the Git Graph's search box. Add author: tag: branch: file: since: until: change: merge: to narrow it down (for example author:alice file:src/main.rs since:2026-01-01)."
        ),
        action: Some(|| crate::git_graph::Open.boxed_clone()),
        topics: &[],
    },
];

/// The order the guide lists `GUIDE_TOPICS` in: the entries about `topic` first, the rest after,
/// each keeping its place relative to the others.
pub(crate) fn topic_order(topic: Option<&str>) -> Vec<usize> {
    let is_about = |entry: &GuideTopic| topic.is_some_and(|topic| entry.topics.contains(&topic));
    let (about, rest): (Vec<_>, Vec<_>) = GUIDE_TOPICS
        .iter()
        .enumerate()
        .partition(|(_, entry)| is_about(entry));
    about
        .into_iter()
        .chain(rest)
        .map(|(index, _)| index)
        .collect()
}

/// "Git の操作ガイド": the things people want to do with git, each with a button that starts it.
pub(crate) struct GitGuide {
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
    order: Vec<usize>,
}

impl GitGuide {
    pub(crate) fn new(topic: Option<&str>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            scroll_handle: ScrollHandle::new(),
            order: topic_order(topic),
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    /// Closes the guide first and runs the topic's action from where the user was, so that an
    /// action that moves focus (into the git panel, say) is not undone by the modal handing focus
    /// back when it closes.
    fn try_topic(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(action) = GUIDE_TOPICS.get(index).and_then(|topic| topic.action) else {
            return;
        };
        let action = action();
        cx.emit(DismissEvent);
        window.defer(cx, move |window, cx| window.dispatch_action(action, cx));
    }

    fn render_topic(
        &self,
        index: usize,
        topic: &GuideTopic,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .debug_selector(move || format!("git-guide-topic-{index}"))
            .w_full()
            .gap_3()
            .py_2()
            .items_start()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(Label::new(i18n::tr(topic.title)))
                    .child(
                        Label::new(i18n::tr(topic.description))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .when(topic.action.is_some(), |this| {
                this.child(
                    div()
                        .flex_none()
                        .debug_selector(move || format!("git-guide-try-{index}"))
                        .child(
                            Button::new(("git-guide-try", index), i18n::t!("Try It"))
                                .style(ButtonStyle::Outlined)
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.try_topic(index, window, cx);
                                })),
                        ),
                )
            })
    }
}

impl EventEmitter<DismissEvent> for GitGuide {}
impl ModalView for GitGuide {}

impl Focusable for GitGuide {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for GitGuide {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let topics = self
            .order
            .iter()
            .enumerate()
            .filter_map(|(position, index)| Some((position, *index, GUIDE_TOPICS.get(*index)?)))
            .map(|(position, index, topic)| {
                let topic = self.render_topic(index, topic, cx).into_any_element();
                if position == 0 {
                    topic
                } else {
                    v_flex()
                        .child(Divider::horizontal())
                        .child(topic)
                        .into_any_element()
                }
            })
            .collect::<Vec<_>>();
        v_flex()
            .key_context("GitGuide")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .elevation_3(cx)
            .w(rems(40.))
            .p_3()
            .gap_2()
            .child(
                h_flex()
                    .gap_1p5()
                    .child(Icon::new(IconName::GitBranch).size(IconSize::Small))
                    .child(Headline::new(i18n::t!("Git Guide")).size(HeadlineSize::Small)),
            )
            .child(
                Label::new(i18n::t!(
                    "Find what you want to do and press \"Try It\" to start that operation."
                ))
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(
                div()
                    .id("git-guide-topics")
                    .max_h(vh(0.7, window))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .child(v_flex().children(topics)),
            )
    }
}

/// The entry points the Git menu, the guide and the graph's buttons need that no other action
/// provides.
pub(crate) fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, action: &git::ShowGuide, window, cx| {
        let topic = action.topic.clone();
        workspace.toggle_modal(window, cx, |_window, cx| {
            GitGuide::new(topic.as_deref(), cx)
        });
    });
    workspace.register_action(|workspace, _: &git::StartAmend, window, cx| {
        let Some(panel) = workspace.focus_panel::<GitPanel>(window, cx) else {
            return;
        };
        let started = panel.update(cx, |panel, cx| panel.begin_amend(window, cx));
        if !started {
            show_toast(
                workspace,
                i18n::t!("There are no commits yet. You can only amend once there is a commit."),
                cx,
            );
        }
    });
    workspace.register_action(|workspace, _: &git::ChooseFixupCommit, window, cx| {
        let Some(repository) = workspace.project().read(cx).active_repository(cx) else {
            show_toast(workspace, no_repository(), cx);
            return;
        };
        if let Some(reason) = crate::commit_context_menu::fixup_menu_refusal(repository.read(cx))
        {
            show_toast(workspace, reason, cx);
            return;
        }
        if open_graph(workspace, &repository, window, cx).is_none() {
            return;
        }
        show_toast(
            workspace,
            i18n::t!(
                "Pick the commit to add your current changes to, then press \"Add Current Changes (fixup)\" above."
            ),
            cx,
        );
    });
    workspace.register_action(|workspace, _: &git::CompareRefs, window, cx| {
        let Some(repository) = workspace.project().read(cx).active_repository(cx) else {
            show_toast(workspace, no_repository(), cx);
            return;
        };
        let Some(graph) = open_graph(workspace, &repository, window, cx) else {
            return;
        };
        // The pickers open as workspace modals, which cannot happen while this handler still
        // holds the workspace.
        window.defer(cx, move |window, cx| {
            graph.update(cx, |graph, cx| graph.start_ref_comparison(window, cx));
        });
    });
}

fn no_repository() -> &'static str {
    i18n::t!("No Git repository is open.")
}

/// The whole-history graph of `repository`, opened (or brought forward) in the workspace.
fn open_graph(
    workspace: &mut Workspace,
    repository: &Entity<Repository>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<GitGraph>> {
    let repo_id = repository.read(cx).id;
    let git_store = workspace.project().read(cx).git_store().clone();
    open_or_reuse_graph(
        workspace,
        repo_id,
        git_store,
        LogSource::All,
        None,
        window,
        cx,
    );
    workspace.items_of_type::<GitGraph>(cx).find(|graph| {
        let graph = graph.read(cx);
        graph.repository_id() == repo_id && graph.can_hold_merge_request_box()
    })
}

fn show_toast(
    workspace: &mut Workspace,
    message: impl Into<SharedString>,
    cx: &mut Context<Workspace>,
) {
    let message = message.into();
    let toast = StatusToast::new(message, cx, |this, _cx| {
        this.icon(
            Icon::new(IconName::Info)
                .size(IconSize::Small)
                .color(Color::Muted),
        )
        .dismiss_button(true)
    });
    workspace.toggle_status_toast(toast, cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Modifiers, TestAppContext, VisualTestContext};
    use std::cell::RefCell;
    use std::rc::Rc;

    fn record<A: Action>(log: &Rc<RefCell<Vec<String>>>, cx: &mut App) {
        let log = log.clone();
        cx.on_action(move |action: &A, _cx| log.borrow_mut().push(action.name().to_string()));
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
    }

    fn draw(element: impl Fn() -> AnyElement + 'static, cx: &mut VisualTestContext) {
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(px(900.), px(6000.)),
            move |_, _| element(),
        );
    }

    /// Every "Try It" dispatches the action its topic names, and closes the guide. Entries
    /// that only explain have no button.
    #[gpui::test]
    async fn test_guide_buttons_dispatch_their_actions(cx: &mut TestAppContext) {
        init_test(cx);
        let log = Rc::new(RefCell::new(Vec::new()));
        cx.update(|cx| {
            record::<git::StartAmend>(&log, cx);
            record::<git::ChooseFixupCommit>(&log, cx);
            record::<git::RewordCommit>(&log, cx);
            record::<crate::git_graph::Open>(&log, cx);
            record::<git::OpenMergeRequestBox>(&log, cx);
            record::<zed_actions::git::Switch>(&log, cx);
            record::<git::ConfirmForcePush>(&log, cx);
            record::<git::CompareRefs>(&log, cx);
        });
        let (guide, cx) = cx.add_window_view(|_window, cx| GitGuide::new(None, cx));
        // The list scrolls within 70% of the window's height, and a click only lands on what is
        // on screen.
        cx.simulate_resize(gpui::size(px(900.), px(6000.)));
        let dismissed = Rc::new(RefCell::new(0));
        cx.update(|_window, cx| {
            let dismissed = dismissed.clone();
            cx.subscribe(&guide, move |_, _: &DismissEvent, _| {
                *dismissed.borrow_mut() += 1;
            })
            .detach();
        });

        let mut explained_only = Vec::new();
        for (index, topic) in GUIDE_TOPICS.iter().enumerate() {
            let guide = guide.clone();
            draw(move || guide.clone().into_any_element(), cx);
            // `debug_bounds` looks selectors up by `&'static str`.
            let selector: &'static str = format!("git-guide-try-{index}").leak();
            let Some(bounds) = cx.debug_bounds(selector) else {
                assert!(topic.action.is_none(), "topic {index} has its button");
                explained_only.push(index);
                continue;
            };
            assert!(topic.action.is_some(), "topic {index} has nothing to try");
            cx.simulate_click(bounds.center(), Modifiers::none());
            cx.run_until_parked();
        }

        assert_eq!(
            *log.borrow(),
            [
                "git::StartAmend",
                "git::ChooseFixupCommit",
                "git::RewordCommit",
                "git_graph::Open",
                "git_graph::Open",
                "git::OpenMergeRequestBox",
                "git::OpenMergeRequestBox",
                "git::OpenMergeRequestBox",
                "git::Switch",
                "git_graph::Open",
                "git_graph::Open",
                "git::ConfirmForcePush",
                "git::CompareRefs",
                "git_graph::Open",
                "git_graph::Open",
            ]
        );
        let expected = GUIDE_TOPICS
            .iter()
            .filter_map(|topic| Some((topic.action?)().name().to_string()))
            .collect::<Vec<_>>();
        assert_eq!(*log.borrow(), expected);
        assert_eq!(*dismissed.borrow(), expected.len());
        assert_eq!(explained_only.len(), GUIDE_TOPICS.len() - expected.len());
        assert!(!explained_only.is_empty());
    }

    /// A topic brings its entries to the top in their usual order, and every other entry still
    /// follows once.
    #[test]
    fn test_topic_order() {
        let usual = (0..GUIDE_TOPICS.len()).collect::<Vec<_>>();
        assert_eq!(topic_order(None), usual);
        assert_eq!(topic_order(Some("no-such-topic")), usual);

        let order = topic_order(Some(MERGE_REQUEST_TOPIC));
        let about = GUIDE_TOPICS
            .iter()
            .enumerate()
            .filter(|(_, topic)| topic.topics.contains(&MERGE_REQUEST_TOPIC))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        assert!(about.len() >= 2, "the MR box has entries about it");
        assert_ne!(
            about.first(),
            Some(&0),
            "the topic actually moves something"
        );
        assert_eq!(order[..about.len()], about[..]);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, usual);
        let rest = order[about.len()..].to_vec();
        assert!(rest.is_sorted());
    }

    /// Opened with a topic, the guide draws that topic's entries at the top of the list.
    #[gpui::test]
    async fn test_guide_opened_with_a_topic_shows_it_first(cx: &mut TestAppContext) {
        init_test(cx);
        let (guide, cx) =
            cx.add_window_view(|_window, cx| GitGuide::new(Some(MERGE_REQUEST_TOPIC), cx));
        draw(move || guide.clone().into_any_element(), cx);

        let top_of = |index: usize, cx: &mut VisualTestContext| {
            let selector: &'static str = format!("git-guide-topic-{index}").leak();
            cx.debug_bounds(selector)
                .unwrap_or_else(|| panic!("topic {index} is drawn"))
                .top()
        };
        let drawn_order = {
            let mut tops = (0..GUIDE_TOPICS.len())
                .map(|index| (top_of(index, cx), index))
                .collect::<Vec<_>>();
            tops.sort();
            tops.into_iter().map(|(_, index)| index).collect::<Vec<_>>()
        };
        assert_eq!(drawn_order, topic_order(Some(MERGE_REQUEST_TOPIC)));
        let first = drawn_order
            .first()
            .and_then(|index| GUIDE_TOPICS.get(*index));
        assert!(first.is_some_and(|topic| topic.topics.contains(&MERGE_REQUEST_TOPIC)));
    }

    struct GuideButtonHost {
        topic: Option<&'static str>,
    }

    impl Render for GuideButtonHost {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(guide_button("git-guide-help", self.topic))
        }
    }

    /// The "?" opens the guide with its topic, and the plain one without.
    #[gpui::test]
    async fn test_guide_button_dispatches_show_guide(cx: &mut TestAppContext) {
        init_test(cx);
        let topics = Rc::new(RefCell::new(Vec::new()));
        cx.update(|cx| {
            let topics = topics.clone();
            cx.on_action(move |action: &git::ShowGuide, _cx| {
                topics.borrow_mut().push(action.topic.clone());
            });
        });
        for topic in [Some(MERGE_REQUEST_TOPIC), None] {
            let (host, cx) = cx.add_window_view(|_window, _cx| GuideButtonHost { topic });
            draw(move || host.clone().into_any_element(), cx);
            let bounds = cx
                .debug_bounds("ICON-CircleHelp")
                .unwrap_or_else(|| panic!("the \"?\" is drawn"));
            cx.simulate_click(bounds.center(), Modifiers::none());
            cx.run_until_parked();
        }

        assert_eq!(
            *topics.borrow(),
            [Some(MERGE_REQUEST_TOPIC.to_string()), None]
        );
    }
}
