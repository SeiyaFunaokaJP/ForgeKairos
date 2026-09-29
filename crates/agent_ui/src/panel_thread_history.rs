//! Thread history rendered inside the agent panel, split to the left of the
//! thread itself.
//!
//! Zed reaches its thread history through the window-wide threads sidebar,
//! which swaps the whole sidebar over to [`crate::threads_archive_view`]. This
//! fork keeps that route but adds an always-available list next to the current
//! thread, the way ChatGPT and Copilot Chat present their conversations.

use chrono::{Local, NaiveDate};
use editor::Editor;
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, ListState, Render,
    SharedString, Subscription, WeakEntity, Window, list, prelude::*, px,
};
use itertools::Itertools as _;
use project::{AgentServerStore, Project};
use ui::{
    ContextMenu, ListItem, ListItemSpacing, ScrollAxes, Scrollbars, ThreadItem, Tooltip,
    WithScrollbar, prelude::*, right_click_menu,
};
use util::path_list::PathList;
use workspace::Workspace;

use crate::TerminalId;
use crate::thread_metadata_store::{ThreadId, ThreadMetadata, ThreadMetadataStore};
use crate::threads_archive_view::{TimeBucket, format_history_entry_timestamp};
use crate::{ArchiveSelectedThread, NewThread, draft_prompt_store};

/// Width of the in-panel history list. Narrow enough that the thread keeps the
/// larger half of a default-width panel, wide enough for a generated title.
pub(crate) const PANEL_THREAD_HISTORY_WIDTH: gpui::Pixels = px(240.);

/// 狭いパネルで使う一覧の幅 (ADR-F53)。日付の 2 行目を落として 1 行にすれば
/// この幅でも題名は読めるので、丸ごと引っ込める前にここへ落とす。
pub(crate) const SLIM_PANEL_THREAD_HISTORY_WIDTH: gpui::Pixels = px(160.);

/// The thread half is squeezed below this width before the history list is
/// worth keeping, so the list hides itself instead.
pub(crate) const MIN_THREAD_WIDTH_BESIDE_HISTORY: gpui::Pixels = px(320.);

pub enum PanelThreadHistoryEvent {
    Activate(ThreadMetadata),
    NewThread,
    /// ADR-F36: パネルではなくワークスペースのタブとして開く。`split` なら
    /// 右に分割したペインへ。
    OpenInTab {
        thread: ThreadMetadata,
        split: bool,
    },
    /// 「開いているタブ」の行が選ばれた (ADR-F53)。
    ActivateOpenTab(OpenTabId),
    CloseOpenTab(OpenTabId),
    /// ヘッダーの «。一覧そのものを閉じる。
    Collapse,
}

/// パネルが開いているものの見分け (ADR-F53)。ペインのタブと、パネルが全面に
/// 出す端末スレッドは別の仕組みなので、行の正体も 2 通りある。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenTabId {
    PaneItem(gpui::EntityId),
    PanelTerminal(TerminalId),
}

/// 「開いているタブ」の 1 行。パネルが毎フレーム組み立てて渡す
/// ([`PanelThreadHistory::set_open_tabs`])。
///
/// アイコンは [`Icon`] そのもの: タブが [`workspace::Item::tab_icon`] で出す
/// ものをそのまま出すので、外部の SVG を使うエージェントでもタブと一覧で
/// 食い違わない。
#[derive(Clone, Debug, PartialEq)]
pub struct OpenTabRow {
    pub id: OpenTabId,
    pub title: SharedString,
    pub icon: Icon,
    pub is_active: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum HistoryListItem {
    BucketSeparator(TimeBucket),
    Entry {
        thread: ThreadMetadata,
        highlight_positions: Vec<usize>,
    },
}

pub struct PanelThreadHistory {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    agent_server_store: WeakEntity<AgentServerStore>,
    filter_editor: Entity<Editor>,
    items: Vec<HistoryListItem>,
    open_tabs: Vec<OpenTabRow>,
    list_state: ListState,
    hovered_index: Option<usize>,
    active_thread_id: Option<ThreadId>,
    slim: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PanelThreadHistoryEvent> for PanelThreadHistory {}

impl PanelThreadHistory {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(i18n::t!("Search threads…"), window, cx);
            editor
        });

        let filter_editor_subscription =
            cx.subscribe(&filter_editor, |this: &mut Self, _, event, cx| {
                if let editor::EditorEvent::BufferEdited = event {
                    this.update_items(cx);
                }
            });

        let store_subscription = cx.observe(
            &ThreadMetadataStore::global(cx),
            |this: &mut Self, _, cx| {
                this.update_items(cx);
            },
        );

        // The list is scoped to the panel's project, so it has to be rebuilt
        // whenever the set of worktrees behind that scope changes.
        let project_subscription =
            cx.subscribe(&project, |this, _project, event, cx| match event {
                project::Event::WorktreeAdded(_)
                | project::Event::WorktreeRemoved(_)
                | project::Event::WorktreeOrderChanged
                | project::Event::WorktreePathsChanged { .. } => this.update_items(cx),
                _ => {}
            });

        let agent_server_store = project.read(cx).agent_server_store().downgrade();

        // The panel is built while the workspace entity is leased, so the
        // first pass — which reads the workspace to scope the list — has to
        // wait for that lease to end.
        let this = cx.entity().downgrade();
        cx.defer(move |cx| {
            this.update(cx, |this, cx| this.update_items(cx)).ok();
        });

        Self {
            focus_handle: cx.focus_handle(),
            workspace,
            agent_server_store,
            filter_editor,
            items: Vec::new(),
            open_tabs: Vec::new(),
            list_state: ListState::new(0, gpui::ListAlignment::Top, px(1000.)),
            hovered_index: None,
            active_thread_id: None,
            slim: false,
            _subscriptions: vec![
                filter_editor_subscription,
                store_subscription,
                project_subscription,
            ],
        }
    }

    /// Called by the panel whenever its active thread changes, so the list can
    /// highlight the open thread and keep the active empty draft visible.
    pub fn set_active_thread_id(&mut self, thread_id: Option<ThreadId>, cx: &mut Context<Self>) {
        if self.active_thread_id == thread_id {
            return;
        }
        self.active_thread_id = thread_id;
        self.update_items(cx);
    }

    /// Called by the panel on every render with what it currently has open, so
    /// the list can show the tabs even while the tab bar is hidden (ADR-F53).
    pub fn set_open_tabs(&mut self, open_tabs: Vec<OpenTabRow>, cx: &mut Context<Self>) {
        if self.open_tabs == open_tabs {
            return;
        }
        self.open_tabs = open_tabs;
        cx.notify();
    }

    /// Called by the panel on every render: below
    /// [`PANEL_THREAD_HISTORY_WIDTH`] + the thread's own minimum the list
    /// narrows to one line per thread instead of disappearing (ADR-F53).
    pub fn set_slim(&mut self, slim: bool, cx: &mut Context<Self>) {
        if self.slim == slim {
            return;
        }
        self.slim = slim;
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn items(&self) -> &[HistoryListItem] {
        &self.items
    }

    #[cfg(test)]
    pub(crate) fn is_slim(&self) -> bool {
        self.slim
    }

    #[cfg(test)]
    pub(crate) fn open_tabs(&self) -> &[OpenTabRow] {
        &self.open_tabs
    }

    fn update_items(&mut self, cx: &mut Context<Self>) {
        let threads = self.threads_in_scope(cx);
        let query = self.filter_editor.read(cx).text(cx).to_lowercase();
        let today = Local::now().naive_local().date();

        let saved_scroll = self.list_state.logical_scroll_top();
        self.items = build_history_items(threads, &query, today);
        self.list_state.reset(self.items.len());
        self.list_state.scroll_to(saved_scroll);

        if self
            .hovered_index
            .is_some_and(|index| index >= self.items.len())
        {
            self.hovered_index = None;
        }

        cx.notify();
    }

    /// Threads that belong to the panel's project, with the throwaway empty
    /// drafts every panel creates on open filtered out.
    fn threads_in_scope(&self, cx: &App) -> Vec<ThreadMetadata> {
        let Some(workspace) = self.workspace.upgrade() else {
            return Vec::new();
        };

        let path_list = PathList::new(&workspace.read(cx).root_paths(cx));
        if path_list.paths().is_empty() {
            return Vec::new();
        }
        let remote_connection = workspace
            .read(cx)
            .project()
            .read(cx)
            .remote_connection_options(cx);

        let store = ThreadMetadataStore::global(cx);
        let store = store.read(cx);
        store
            .entries_for_path(&path_list, remote_connection.as_ref())
            .chain(store.entries_for_main_worktree_path(&path_list, remote_connection.as_ref()))
            .unique_by(|thread| thread.thread_id)
            .filter(|thread| self.should_list_thread(thread))
            .cloned()
            .collect()
    }

    /// Every panel opens on a fresh draft, so untitled drafts are only worth a
    /// row while they are the thread the user is looking at.
    fn should_list_thread(&self, thread: &ThreadMetadata) -> bool {
        !thread.is_draft()
            || thread.title().is_some()
            || Some(thread.thread_id) == self.active_thread_id
    }

    fn row_title(&self, thread: &ThreadMetadata, cx: &App) -> SharedString {
        if let Some(title) = thread.title() {
            return title;
        }
        if thread.is_draft() {
            // Deliberately not `display_label_for_draft`: previewing the
            // unsent prompt reads the agent panel, which is mid-render around
            // this list.
            return draft_prompt_store::empty_draft_placeholder_label(
                self.workspace.upgrade().as_ref(),
                &thread.agent_id,
                cx,
            );
        }
        thread.display_title()
    }

    fn render_list_entry(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(item) = self.items.get(index) else {
            return div().into_any_element();
        };

        match item {
            HistoryListItem::BucketSeparator(bucket) => div()
                .w_full()
                .px_2()
                .pt_2p5()
                .pb_1()
                .child(
                    Label::new(bucket.label())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            HistoryListItem::Entry {
                thread,
                highlight_positions,
            } => {
                let thread = thread.clone();
                let highlight_positions = highlight_positions.clone();
                let is_active = Some(thread.thread_id) == self.active_thread_id;
                let is_hovered = self.hovered_index == Some(index);
                let focus_handle = self.focus_handle.clone();

                let icon_from_external_svg = self
                    .agent_server_store
                    .upgrade()
                    .and_then(|store| store.read(cx).agent_icon(&thread.agent_id));
                let is_zed_agent = thread.agent_id.as_ref() == agent::ZED_AGENT_ID.as_ref();
                let icon = if is_zed_agent {
                    IconName::ZedAgent
                } else {
                    IconName::Sparkle
                };
                // ADR-F49 決定 19 (8c): the banner's colour on the icon of a
                // thread whose agent was started outside its sandbox, which is
                // where it comes back. A thread started in the sandbox comes
                // back there whatever the settings say now, so they are not
                // asked.
                let outside_sandbox = !is_zed_agent
                    && crate::thread_metadata_store::thread_started_outside_sandbox(
                        thread.thread_id,
                        cx,
                    );

                let row =
                    ThreadItem::new(("panel-history-entry", index), self.row_title(&thread, cx))
                        .icon(icon)
                        .when_some(icon_from_external_svg, |this, svg| {
                            this.custom_icon_from_external_svg(svg)
                        })
                        .when(outside_sandbox, |this| this.icon_color(Color::Warning))
                        // 狭いときは日付を渡さない = 2 行目ごと消える。
                        .when(!self.slim, |this| {
                            this.timestamp(format_history_entry_timestamp(thread.updated_at))
                        })
                        .highlight_positions(highlight_positions)
                        .selected(is_active)
                        .hovered(is_hovered)
                        .on_hover(cx.listener(move |this, is_hovered: &bool, _window, cx| {
                            let previously_hovered = this.hovered_index;
                            this.hovered_index = if *is_hovered {
                                Some(index)
                            } else {
                                previously_hovered.filter(|hovered| *hovered != index)
                            };
                            if this.hovered_index != previously_hovered {
                                cx.notify();
                            }
                        }))
                        .when(is_hovered && !thread.is_draft(), |this| {
                            let thread_id = thread.thread_id;
                            this.action_slot(
                                IconButton::new(
                                    ("panel-history-archive", index),
                                    IconName::Archive,
                                )
                                .icon_size(IconSize::Small)
                                .tooltip(move |_window, cx| {
                                    Tooltip::for_action_in(
                                        i18n::t!("Archive Thread"),
                                        &ArchiveSelectedThread,
                                        &focus_handle,
                                        cx,
                                    )
                                })
                                .on_click(cx.listener(
                                    move |_this, _, _window, cx| {
                                        ThreadMetadataStore::global(cx).update(cx, |store, cx| {
                                            store.archive(thread_id, None, cx);
                                        });
                                        cx.stop_propagation();
                                    },
                                )),
                            )
                        })
                        .on_click(cx.listener({
                            let thread = thread.clone();
                            move |_this, _, _window, cx| {
                                cx.emit(PanelThreadHistoryEvent::Activate(thread.clone()));
                            }
                        }))
                        .into_any_element();

                // ADR-F36: 行から直接タブへ。パネルの表示は変えないので、
                // 別のスレッドを見ながら並べられる。
                let history = cx.entity();
                right_click_menu(("panel-history-entry-menu", index))
                    .trigger(move |_, _, _| row)
                    .menu(move |window, cx| {
                        let history = history.clone();
                        let thread = thread.clone();
                        ContextMenu::build(window, cx, move |menu, _window, _cx| {
                            menu.entry(i18n::t!("Open in Tab"), None, {
                                let history = history.clone();
                                let thread = thread.clone();
                                move |_window, cx| {
                                    history.update(cx, |_this, cx| {
                                        cx.emit(PanelThreadHistoryEvent::OpenInTab {
                                            thread: thread.clone(),
                                            split: false,
                                        });
                                    });
                                }
                            })
                            .entry(
                                i18n::t!("Open in Right Split"),
                                None,
                                {
                                    move |_window, cx| {
                                        history.update(cx, |_this, cx| {
                                            cx.emit(PanelThreadHistoryEvent::OpenInTab {
                                                thread: thread.clone(),
                                                split: true,
                                            });
                                        });
                                    }
                                },
                            )
                        })
                    })
                    .into_any_element()
            }
        }
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let has_query = !self.filter_editor.read(cx).text(cx).is_empty();

        v_flex()
            .flex_none()
            .p_1()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        div().flex_1().min_w_0().child(
                            Button::new("panel-history-new-thread", i18n::t!("New Thread"))
                                .start_icon(Icon::new(IconName::Plus).size(IconSize::Small))
                                .label_size(LabelSize::Small)
                                .full_width()
                                .on_click(cx.listener(|_this, _, _window, cx| {
                                    cx.emit(PanelThreadHistoryEvent::NewThread);
                                })),
                        ),
                    )
                    .child(
                        IconButton::new("panel-history-collapse", IconName::ChevronLeft)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text(i18n::t!("Close Thread History")))
                            .on_click(cx.listener(|_this, _, _window, cx| {
                                cx.emit(PanelThreadHistoryEvent::Collapse);
                            })),
                    ),
            )
            .child(
                h_flex()
                    .px_1()
                    .gap_1()
                    .child(
                        Icon::new(IconName::MagnifyingGlass)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().min_w_0().flex_1().child(self.filter_editor.clone()))
                    .when(has_query, |this| {
                        this.child(
                            IconButton::new("panel-history-clear-filter", IconName::Close)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text(i18n::t!("Clear Search")))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.filter_editor.update(cx, |editor, cx| {
                                        editor.set_text("", window, cx);
                                    });
                                    this.update_items(cx);
                                })),
                        )
                    }),
            )
    }

    /// 「開いているタブ」の節。タブバーを隠していても、パネルが開いている
    /// ものすべて (ペインのタブ + パネルの端末スレッド) にここから届く。
    fn render_open_tabs(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if self.open_tabs.is_empty() {
            return None;
        }

        let rows =
            self.open_tabs
                .iter()
                .enumerate()
                .map(|(index, row)| {
                    let id = row.id;
                    let list_item = ListItem::new(("panel-open-tab", index))
                        .spacing(ListItemSpacing::Sparse)
                        .toggle_state(row.is_active)
                        .start_slot(row.icon.clone().size(IconSize::Small).color(
                            if row.is_active {
                                Color::Default
                            } else {
                                Color::Muted
                            },
                        ))
                        .child(
                            Label::new(row.title.clone())
                                .size(LabelSize::Small)
                                .truncate(),
                        )
                        .end_slot_on_hover(
                            IconButton::new(("panel-open-tab-close", index), IconName::Close)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text(i18n::t!("Close Tab")))
                                .on_click(cx.listener(move |_this, _, _window, cx| {
                                    cx.emit(PanelThreadHistoryEvent::CloseOpenTab(id));
                                    cx.stop_propagation();
                                })),
                        )
                        .on_click(cx.listener(move |_this, _, _window, cx| {
                            cx.emit(PanelThreadHistoryEvent::ActivateOpenTab(id));
                        }));
                    div()
                        .debug_selector(move || format!("PANEL_OPEN_TAB-{index}"))
                        .child(list_item)
                })
                .collect::<Vec<_>>();

        Some(
            v_flex()
                .flex_none()
                .border_b_1()
                .border_color(cx.theme().colors().border)
                .child(
                    div().px_2().pt_1p5().pb_0p5().child(
                        Label::new(i18n::t!("Open Tabs"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                )
                .child(
                    // 開きすぎたタブで履歴を潰さないよう、この節だけを畳んで
                    // スクロールさせる。
                    v_flex()
                        .id("panel-open-tabs")
                        .max_h(if self.slim { px(120.) } else { px(200.) })
                        .overflow_y_scroll()
                        .pb_1()
                        .children(rows),
                ),
        )
    }
}

/// Sorts newest first, keeps only the threads matching `query`, and inserts the
/// date separators the list renders between groups.
pub(crate) fn build_history_items(
    threads: Vec<ThreadMetadata>,
    query: &str,
    today: NaiveDate,
) -> Vec<HistoryListItem> {
    let ordered = threads
        .into_iter()
        .sorted_by_key(|thread| std::cmp::Reverse(thread.updated_at));

    let mut items = Vec::new();
    let mut current_bucket: Option<TimeBucket> = None;

    for thread in ordered {
        let highlight_positions = if query.is_empty() {
            Vec::new()
        } else {
            let title = thread.display_title();
            match crate::threads_archive_view::fuzzy_match_positions(query, title.as_ref()) {
                Some(positions) => positions,
                None => continue,
            }
        };

        let bucket = TimeBucket::from_dates(
            today,
            thread.updated_at.with_timezone(&Local).naive_local().date(),
        );
        if Some(bucket) != current_bucket {
            current_bucket = Some(bucket);
            items.push(HistoryListItem::BucketSeparator(bucket));
        }

        items.push(HistoryListItem::Entry {
            thread,
            highlight_positions,
        });
    }

    items
}

impl Focusable for PanelThreadHistory {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PanelThreadHistory {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_query = !self.filter_editor.read(cx).text(cx).is_empty();

        let body = if self.items.is_empty() {
            let message = if has_query {
                i18n::t!("No threads match your search.")
            } else {
                i18n::t!("No threads yet.")
            };

            v_flex()
                .flex_1()
                .p_2()
                .justify_center()
                .items_center()
                .child(
                    Label::new(message)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element()
        } else {
            v_flex()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .child(
                    list(
                        self.list_state.clone(),
                        cx.processor(Self::render_list_entry),
                    )
                    .flex_1()
                    .size_full(),
                )
                .custom_scrollbars(
                    Scrollbars::new(ScrollAxes::Vertical).tracked_scroll_handle(&self.list_state),
                    window,
                    cx,
                )
                .into_any_element()
        };

        v_flex()
            .key_context("PanelThreadHistory")
            .debug_selector(|| "PANEL_THREAD_HISTORY".into())
            .track_focus(&self.focus_handle)
            .w(if self.slim {
                SLIM_PANEL_THREAD_HISTORY_WIDTH
            } else {
                PANEL_THREAD_HISTORY_WIDTH
            })
            .flex_none()
            .h_full()
            .border_r_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().panel_background)
            .on_action(cx.listener(|_this, _: &NewThread, _window, cx| {
                cx.emit(PanelThreadHistoryEvent::NewThread);
            }))
            .child(self.render_header(cx))
            .children(self.render_open_tabs(cx))
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeZone as _, Utc};
    use project::{AgentId, WorktreePaths};

    fn thread(title: &str, updated_at: DateTime<Utc>) -> ThreadMetadata {
        ThreadMetadata {
            thread_id: ThreadId::new(),
            session_id: Some(agent_client_protocol::schema::v1::SessionId::new(
                title.to_string(),
            )),
            agent_id: AgentId("zed".into()),
            title: Some(title.to_string().into()),
            title_override: None,
            updated_at,
            created_at: Some(updated_at),
            interacted_at: None,
            worktree_paths: WorktreePaths::from_folder_paths(&PathList::default()),
            remote_connection: None,
            archived: false,
        }
    }

    fn at(day: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, day, hour, 0, 0).unwrap()
    }

    fn titles(items: &[HistoryListItem]) -> Vec<String> {
        items
            .iter()
            .filter_map(|item| match item {
                HistoryListItem::Entry { thread, .. } => Some(thread.display_title().to_string()),
                HistoryListItem::BucketSeparator(_) => None,
            })
            .collect()
    }

    #[test]
    fn orders_newest_first() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 30).unwrap();
        let items = build_history_items(
            vec![
                thread("oldest", at(20, 9)),
                thread("newest", at(30, 9)),
                thread("middle", at(29, 9)),
            ],
            "",
            today,
        );

        assert_eq!(titles(&items), vec!["newest", "middle", "oldest"]);
    }

    #[test]
    fn groups_entries_under_one_separator_per_bucket() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 30).unwrap();
        let items = build_history_items(
            vec![
                thread("today-a", at(30, 9)),
                thread("today-b", at(30, 8)),
                thread("yesterday", at(29, 9)),
            ],
            "",
            today,
        );

        let separators: Vec<TimeBucket> = items
            .iter()
            .filter_map(|item| match item {
                HistoryListItem::BucketSeparator(bucket) => Some(*bucket),
                HistoryListItem::Entry { .. } => None,
            })
            .collect();
        assert_eq!(
            separators,
            vec![TimeBucket::Today, TimeBucket::Yesterday],
            "each bucket gets exactly one separator, in order"
        );
        assert_eq!(items.len(), 5);
    }

    #[test]
    fn filters_by_title_and_records_highlights() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 30).unwrap();
        let items = build_history_items(
            vec![
                thread("rename the panel", at(30, 9)),
                thread("other", at(30, 8)),
            ],
            "panel",
            today,
        );

        assert_eq!(titles(&items), vec!["rename the panel"]);
        let highlights = items.iter().find_map(|item| match item {
            HistoryListItem::Entry {
                highlight_positions,
                ..
            } => Some(highlight_positions.clone()),
            HistoryListItem::BucketSeparator(_) => None,
        });
        assert_eq!(highlights, Some(vec![11, 12, 13, 14, 15]));
    }

    #[test]
    fn drops_the_separator_when_every_entry_is_filtered_out() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 30).unwrap();
        let items = build_history_items(vec![thread("other", at(30, 9))], "panel", today);

        assert!(items.is_empty());
    }
}
