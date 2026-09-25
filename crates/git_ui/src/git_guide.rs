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
pub(crate) const COMMIT_DRAG_HINT: &str =
    "ドラッグ: ブランチに落とすと取り込み / 別の行に落とすと並べ替え / MR の箱へ";
/// What the grip beside a git panel file says it can be dragged to.
pub(crate) const FILE_DRAG_HINT: &str = "ドラッグ: ステージ済みへ / コミットに落とすと fixup";

/// The ⋮⋮ that says "this row can be dragged", with what dropping it does in its tooltip. The
/// caller decides when it shows; it is only a signpost, the drag itself belongs to the row.
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
        .tooltip(Tooltip::text(hint))
}

/// One entry of the guide: something the user wants to do, how to do it, and the action that
/// starts it.
pub(crate) struct GuideTopic {
    pub(crate) title: &'static str,
    pub(crate) description: &'static str,
    pub(crate) action: fn() -> Box<dyn Action>,
}

pub(crate) const GUIDE_TOPICS: &[GuideTopic] = &[
    GuideTopic {
        title: "直前のコミットを直したい (amend)",
        description: "コミットした直後に書き漏れや入れ忘れに気づいたとき。Git パネルの「直前のコミットを修正 (amend)」にチェックを入れると直前のメッセージが入り、コミットすると直前のコミットが置き換わります。",
        action: || git::StartAmend.boxed_clone(),
    },
    GuideTopic {
        title: "昔のコミットに修正を足したい (fixup)",
        description: "数個前のコミットに入れるべき修正を用意したら、コミットグラフでそのコミットを選んで「今の変更を入れる (fixup)」を押します。Git パネルのファイルをコミットの行にドラッグしても入れられます。",
        action: || git::ChooseFixupCommit.boxed_clone(),
    },
    GuideTopic {
        title: "コミットメッセージを直したい",
        description: "直前のコミットのメッセージを書き換える画面を開きます。昔のコミットは、コミットグラフで選んで「メッセージを書き換え」を押します (以降のコミットの SHA が変わります)。",
        action: || git::RewordCommit::default().boxed_clone(),
    },
    GuideTopic {
        title: "コミットをまとめたい・並べ替えたい",
        description: "コミットグラフで Ctrl / Shift を押しながら行を選び、上の「並べ替え / まとめる / 捨てる…」を押します。1 件を別の行にドラッグしても並べ替えられます。",
        action: || crate::git_graph::Open.boxed_clone(),
    },
    GuideTopic {
        title: "MR 用に 1 コミットにしたい (MR の箱)",
        description: "開発ブランチのコミットを「MR の箱」に集めると、MR 用の新しいブランチに 1 コミットとしてまとめます。元のブランチはそのまま残ります。",
        action: || git::OpenMergeRequestBox.boxed_clone(),
    },
    GuideTopic {
        title: "ブランチを作りたい・切り替えたい",
        description: "ブランチの一覧から切り替えます。一覧に無い名前を入力すると新しいブランチを作れます。昔のコミットから作るときは、コミットグラフでそのコミットを選んで「ここからブランチ」を押します。",
        action: || zed_actions::git::Switch.boxed_clone(),
    },
    GuideTopic {
        title: "間違えたので戻したい (reset / revert / 元に戻す)",
        description: "コミットグラフでコミットを選び、変更を打ち消すコミットを作るなら「打ち消す (revert)」、ブランチごと戻すなら「このコミットまで戻す ▾」(hard は変更を捨てます)。書き換えの後はトーストの「元に戻す」か reflog から戻せます。",
        action: || crate::git_graph::Open.boxed_clone(),
    },
    GuideTopic {
        title: "プッシュ済みのコミットを直した",
        description: "amend・fixup・並べ替えなどでプッシュ済みのコミットを書き換えたら、「強制プッシュ (lease 付き)」でプッシュします。lease 付きなので、他の人が先にプッシュしたコミットは上書きしません。",
        action: || git::ConfirmForcePush.boxed_clone(),
    },
    GuideTopic {
        title: "2 つのブランチを比べたい",
        description: "2 つのブランチ / タグを選ぶと、片方にしか無いコミットと中身の差を一覧にします。移植漏れの確認に使えます。",
        action: || git::CompareRefs.boxed_clone(),
    },
    GuideTopic {
        title: "過去の変更を探したい (検索)",
        description: "コミットグラフの検索欄で探します。著者: タグ: ブランチ: ファイル: 以降: 以前: 変更: マージ: を付けると絞り込めます (例: 著者:山田 ファイル:src/main.rs 以降:2026-01-01)。",
        action: || crate::git_graph::Open.boxed_clone(),
    },
];

/// "Git の操作ガイド": the things people want to do with git, each with a button that starts it.
pub(crate) struct GitGuide {
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
}

impl GitGuide {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            scroll_handle: ScrollHandle::new(),
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    /// Closes the guide first and runs the topic's action from where the user was, so that an
    /// action that moves focus (into the git panel, say) is not undone by the modal handing focus
    /// back when it closes.
    fn try_topic(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(topic) = GUIDE_TOPICS.get(index) else {
            return;
        };
        let action = (topic.action)();
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
            .w_full()
            .gap_3()
            .py_2()
            .items_start()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(Label::new(topic.title))
                    .child(
                        Label::new(topic.description)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .debug_selector(move || format!("git-guide-try-{index}"))
                    .child(
                        Button::new(("git-guide-try", index), "やってみる")
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.try_topic(index, window, cx);
                            })),
                    ),
            )
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
        let topics = GUIDE_TOPICS
            .iter()
            .enumerate()
            .map(|(index, topic)| {
                let topic = self.render_topic(index, topic, cx).into_any_element();
                if index == 0 {
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
                    .child(Headline::new("Git の操作ガイド").size(HeadlineSize::Small)),
            )
            .child(
                Label::new("やりたいことを探して「やってみる」を押すと、その操作を始めます。")
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
    workspace.register_action(|workspace, _: &git::ShowGuide, window, cx| {
        workspace.toggle_modal(window, cx, |_window, cx| GitGuide::new(cx));
    });
    workspace.register_action(|workspace, _: &git::StartAmend, window, cx| {
        let Some(panel) = workspace.focus_panel::<GitPanel>(window, cx) else {
            return;
        };
        let started = panel.update(cx, |panel, cx| panel.begin_amend(window, cx));
        if !started {
            show_toast(
                workspace,
                "まだコミットがありません。修正 (amend) できるのはコミットがあるときだけです。",
                cx,
            );
        }
    });
    workspace.register_action(|workspace, _: &git::ChooseFixupCommit, window, cx| {
        let Some(repository) = workspace.project().read(cx).active_repository(cx) else {
            show_toast(workspace, NO_REPOSITORY, cx);
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
            "今の変更を入れる先のコミットを選び、上の「今の変更を入れる (fixup)」を押してください。",
            cx,
        );
    });
    workspace.register_action(|workspace, _: &git::CompareRefs, window, cx| {
        let Some(repository) = workspace.project().read(cx).active_repository(cx) else {
            show_toast(workspace, NO_REPOSITORY, cx);
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

const NO_REPOSITORY: &str = "Git リポジトリが開かれていません。";

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

    /// Every "やってみる" dispatches the action its topic names, and closes the guide.
    #[gpui::test]
    async fn test_guide_buttons_dispatch_their_actions(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
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
        let (guide, cx) = cx.add_window_view(|_window, cx| GitGuide::new(cx));
        let dismissed = Rc::new(RefCell::new(0));
        cx.update(|_window, cx| {
            let dismissed = dismissed.clone();
            cx.subscribe(&guide, move |_, _: &DismissEvent, _| {
                *dismissed.borrow_mut() += 1;
            })
            .detach();
        });

        for index in 0..GUIDE_TOPICS.len() {
            let guide = guide.clone();
            let cx: &mut VisualTestContext = cx;
            cx.draw(
                gpui::point(px(0.), px(0.)),
                gpui::size(px(900.), px(2000.)),
                |_, _| guide.into_any_element(),
            );
            // `debug_bounds` looks selectors up by `&'static str`.
            let selector: &'static str = format!("git-guide-try-{index}").leak();
            let bounds = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("topic {index} has its button"));
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
                "git::OpenMergeRequestBox",
                "git::Switch",
                "git_graph::Open",
                "git::ConfirmForcePush",
                "git::CompareRefs",
                "git_graph::Open",
            ]
        );
        assert_eq!(*dismissed.borrow(), GUIDE_TOPICS.len());
        let expected = GUIDE_TOPICS
            .iter()
            .map(|topic| (topic.action)().name().to_string())
            .collect::<Vec<_>>();
        assert_eq!(*log.borrow(), expected);
    }
}
