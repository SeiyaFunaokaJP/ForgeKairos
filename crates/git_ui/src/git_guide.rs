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

/// The `topic` of `git::ShowGuide` that the MR box's "?" asks for.
pub(crate) const MERGE_REQUEST_TOPIC: &str = "merge_request";

const GUIDE_BUTTON_TOOLTIP: &str = "Git の操作ガイド (やりたいことから操作を探せます)";

/// The "?" that opens the guide from wherever the user is stuck, with the entries about `topic`
/// first.
pub(crate) fn guide_button(id: impl Into<ElementId>, topic: Option<&'static str>) -> IconButton {
    let action = git::ShowGuide {
        topic: topic.map(str::to_string),
    };
    IconButton::new(id, IconName::CircleHelp)
        .icon_size(IconSize::Small)
        // The binding people can press anywhere is the plain one, not this topic's.
        .tooltip(|_window, cx| Tooltip::for_action(GUIDE_BUTTON_TOOLTIP, &git::ShowGuide, cx))
        .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
}

/// One entry of the guide: something the user wants to do, how to do it, and the action that
/// starts it when there is one.
pub(crate) struct GuideTopic {
    pub(crate) title: &'static str,
    pub(crate) description: &'static str,
    pub(crate) action: Option<fn() -> Box<dyn Action>>,
    /// The `git::ShowGuide` topics this entry comes first for.
    pub(crate) topics: &'static [&'static str],
}

pub(crate) const GUIDE_TOPICS: &[GuideTopic] = &[
    GuideTopic {
        title: "直前のコミットを直したい (amend)",
        description: "コミットした直後に書き漏れや入れ忘れに気づいたとき。Git パネルの「直前のコミットを修正 (amend)」にチェックを入れると直前のメッセージが入り、コミットすると直前のコミットが置き換わります。",
        action: Some(|| git::StartAmend.boxed_clone()),
        topics: &[],
    },
    GuideTopic {
        title: "昔のコミットに修正を足したい (fixup)",
        description: "数個前のコミットに入れるべき修正を用意したら、コミットグラフでそのコミットを選んで「今の変更を入れる (fixup)」を押します。Git パネルのファイルをコミットの行にドラッグしても入れられます。",
        action: Some(|| git::ChooseFixupCommit.boxed_clone()),
        topics: &[],
    },
    GuideTopic {
        title: "コミットメッセージを直したい (reword)",
        description: "直前のコミットのメッセージを書き換える画面を開きます。昔のコミットは、コミットグラフで選んで「メッセージを書き換え」を押します (以降のコミットの SHA が変わります)。",
        action: Some(|| git::RewordCommit::default().boxed_clone()),
        topics: &[],
    },
    GuideTopic {
        title: "コミットをまとめたい・並べ替えたい・捨てたい (squash / drop)",
        description: "コミットグラフで Ctrl / Shift を押しながら行を選び、上の「並べ替え / まとめる / 捨てる…」を押すと、順番・まとめ方・メッセージを編集できます。1 件を別の行にドラッグしても並べ替えられます。捨てたコミットの変更は消えるので注意してください (以降のコミットの SHA が変わります)。",
        action: Some(|| crate::git_graph::Open.boxed_clone()),
        topics: &[],
    },
    GuideTopic {
        title: "別のブランチのコミットを持ってきたい (cherry-pick)",
        description: "コミットグラフで持ってきたいコミットを選び「取り込む (cherry-pick)」を押すと、その変更を現在のブランチに新しいコミットとして積みます。Ctrl / Shift で複数選ぶと「N 件を取り込む」で古い順に積みます。行をブランチ名にドラッグしても取り込めます。同じ場所を変えているとコンフリクトになることがあります。",
        action: Some(|| crate::git_graph::Open.boxed_clone()),
        topics: &[],
    },
    GuideTopic {
        title: "MR 用に 1 コミットにしたい (MR の箱)",
        description: "開発ブランチのコミットを「MR の箱」に集めると、MR 用の新しいブランチに 1 コミットとしてまとめます。元のブランチはそのまま残ります。",
        action: Some(|| git::OpenMergeRequestBox.boxed_clone()),
        topics: &[MERGE_REQUEST_TOPIC],
    },
    GuideTopic {
        title: "MR の箱の使い方 (入れる・除外・作成して比較)",
        description: "1. 「開発ブランチ」(コミットの出どころ) と「基点」(MR の向け先) を選びます。2. コミットグラフの行を箱にドラッグするか、選んで「MR の箱に入れる」を押します。下の未配置の一覧の「+ 箱へ」でも入れられ、MR に入れないものは「除外」で脇に置きます。3. 未配置が 0 件なら入れ忘れはありません。4. メッセージと MR ブランチ名を入れて「作成して比較」を押すと、基点の上で 1 コミットにまとめた MR ブランチを作り、開発ブランチと比べます。未コミットの変更があると作れないので、先にコミットかスタッシュしてください。",
        action: Some(|| git::OpenMergeRequestBox.boxed_clone()),
        topics: &[MERGE_REQUEST_TOPIC],
    },
    GuideTopic {
        title: "MR ブランチを作り直したい",
        description: "同じ MR ブランチ名で作るときは「既存の … を作り直す」にチェックを入れます。プッシュ済みの MR ブランチを作り直したら、強制プッシュ (lease 付き) が必要です。元の MR ブランチは reflog から戻せます。",
        action: Some(|| git::OpenMergeRequestBox.boxed_clone()),
        topics: &[MERGE_REQUEST_TOPIC],
    },
    GuideTopic {
        title: "ブランチを作りたい・切り替えたい",
        description: "ブランチの一覧から切り替えます。一覧に無い名前を入力すると新しいブランチを作れます。昔のコミットから作るときは、コミットグラフでそのコミットを選んで「ここからブランチ」を押します。",
        action: Some(|| zed_actions::git::Switch.boxed_clone()),
        topics: &[],
    },
    GuideTopic {
        title: "昔のコミットの状態を見たい (チェックアウト)",
        description: "コミットグラフでコミットを選んで「チェックアウト」を押すと、作業ツリーがそのコミットの状態になります (detached HEAD)。このままコミットするとどのブランチにも属さないので、続けて作業するなら「ここからブランチ」を使ってください。戻るときはブランチの一覧から元のブランチに切り替えます。",
        action: Some(|| crate::git_graph::Open.boxed_clone()),
        topics: &[],
    },
    GuideTopic {
        title: "間違えたので戻したい (reset / revert / 元に戻す)",
        description: "コミットグラフでコミットを選び、変更を打ち消すコミットを作るなら「打ち消す (revert)」、ブランチごと戻すなら「このコミットまで戻す ▾」(hard は変更を捨てます)。書き換えの後はトーストの「元に戻す」か reflog から戻せます。",
        action: Some(|| crate::git_graph::Open.boxed_clone()),
        topics: &[],
    },
    GuideTopic {
        title: "「このコミットまで戻す」の soft / mixed / hard の違い",
        description: "どれも現在のブランチを選んだコミットまで戻し、それより後のコミットを取り消します。soft は取り消したコミットの変更をステージ済みに、mixed は未ステージの変更として残すので、コミットし直せます。hard は取り消したコミットの変更も、まだコミットしていない変更も捨てます (未コミットの変更は元に戻せません)。",
        action: None,
        topics: &[],
    },
    GuideTopic {
        title: "プッシュ済みのコミットを直した",
        description: "amend・fixup・並べ替えなどでプッシュ済みのコミットを書き換えたら、「強制プッシュ (lease 付き)」でプッシュします。lease 付きなので、他の人が先にプッシュしたコミットは上書きしません。",
        action: Some(|| git::ConfirmForcePush.boxed_clone()),
        topics: &[],
    },
    GuideTopic {
        title: "2 つのブランチを比べたい",
        description: "2 つのブランチ / タグを選ぶと、片方にしか無いコミットと中身の差を一覧にします。移植漏れの確認に使えます。",
        action: Some(|| git::CompareRefs.boxed_clone()),
        topics: &[],
    },
    GuideTopic {
        title: "タグを探したい・付けたい (tag)",
        description: "コミットグラフの「タグ一覧」でタグを検索して選ぶと、そのコミットまで移動します。タグを付けるときは、コミットの行を右クリックして「ここにタグを付ける…」を選びます (メッセージを空にすると軽量タグ)。",
        action: Some(|| crate::git_graph::Open.boxed_clone()),
        topics: &[],
    },
    GuideTopic {
        title: "過去の変更を探したい (検索)",
        description: "コミットグラフの検索欄で探します。著者: タグ: ブランチ: ファイル: 以降: 以前: 変更: マージ: を付けると絞り込めます (例: 著者:山田 ファイル:src/main.rs 以降:2026-01-01)。",
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
                    .child(Label::new(topic.title))
                    .child(
                        Label::new(topic.description)
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
                            Button::new(("git-guide-try", index), "やってみる")
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

    /// Every "やってみる" dispatches the action its topic names, and closes the guide. Entries
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
