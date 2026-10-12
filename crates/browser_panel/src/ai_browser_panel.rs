//! AI ブラウザーパネル (ADR-F89 追記 10)。
//!
//! AI タブ ([`browser_automation`](crate::browser_automation) が操作する、別プロファイルで
//! 通信が許可リストに縛られたタブ) は人のブラウザーパネルには置かず、ここにだけ並べる。
//! 既定ではエージェントパネルと同じ右ドックにあり、AI がタブを開くと、ドックが出している
//! パネル (既定ではエージェントパネル) の下に積んで見せる。フォーカスは奪わない。
//!
//! タブ列・分割・保存と復元は人のパネルと同じもの ([`PanelTabs`] / [`BrowserTabsPanel`])
//! を使い、違うのは置くタブの種類 ([`PanelKind::Ai`]) と、タブが無いときの案内だけ。
//! 前の版が人のパネルに保存していた AI タブは、読み込みのときにこちらへ移る
//! ([`BrowserPanel::take_migrated_ai_tabs`])。

use anyhow::Result;
use gpui::{
    Action, AnyElement, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle,
    Focusable, KeyContext, MouseDownEvent, Pixels, WeakEntity, Window, actions,
};
use settings::Settings as _;
use ui::{Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    CloseActiveItem, DraggedTab, Pane, Workspace,
    dock::{Dock, DockPosition, Panel, PanelEvent},
};

use crate::{
    BROWSER_PANEL_KEY, BrowserItem, BrowserPanel, BrowserSettings, BrowserTabsPanel, ClosePanelTab,
    FocusNextPaneGroup, HIDDEN_PANEL_INACTIVE, NewAiTab, NewPanelTab, NewTabPlacement, PanelKind,
    PanelTabs, SplitPanelRight, read_serialized_layout, restore, webview::WebViewProfile,
};

const AI_BROWSER_PANEL_KEY: &str = "AiBrowserPanel";
const AI_CONTROL_SETTING: &str = "browser.ai_control";

actions!(
    ai_browser_panel,
    [
        /// Toggles the AI browser panel, where AI agents open the browser tabs they drive.
        Toggle,
        /// Toggles focus on the AI browser panel, where AI agents open the browser tabs they drive.
        ToggleFocus,
    ]
);

pub(crate) fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
        workspace.toggle_panel_focus::<AiBrowserPanel>(window, cx);
    });
    workspace.register_action(|workspace, _: &Toggle, window, cx| {
        if !workspace.toggle_panel_focus::<AiBrowserPanel>(window, cx) {
            workspace.close_panel::<AiBrowserPanel>(window, cx);
        }
    });
    workspace.register_action(|workspace, _: &NewAiTab, window, cx| {
        AiBrowserPanel::open_ai_tab_for_user(workspace, window, cx);
    });
}

/// ドックの AI ブラウザー。AI タブだけを並べる ([`PanelKind::Ai`])。
pub struct AiBrowserPanel {
    pub(crate) tabs: PanelTabs,
}

impl AiBrowserPanel {
    /// [`BrowserPanel::load`] と同じく、ワークスペースの復元の待ち合わせに加わる。
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace
            .update(&mut cx, |workspace, _| workspace.begin_item_restore())
            .log_err();
        let panel = Self::load_inner(workspace.clone(), cx.clone()).await;
        workspace
            .update_in(&mut cx, |workspace, window, cx| {
                workspace.end_item_restore(window, cx);
            })
            .log_err();
        panel
    }

    async fn load_inner(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        let serialized = read_serialized_layout::<Self>(&workspace, &mut cx).await;
        let workspace_id = workspace
            .read_with(&cx, |workspace, _| workspace.database_id())
            .ok()
            .flatten();
        let panel = workspace.update_in(&mut cx, |workspace, window, cx| {
            cx.new(|cx| AiBrowserPanel::new(workspace, window, cx))
        })?;
        // タブが無ければ何も開かない (空の案内を出す)。AI タブは AI か利用者が
        // 開くときにだけ作る。
        if let Some(serialized) = serialized {
            restore(&panel, serialized, workspace_id, &mut cx).await;
        }
        // 人のパネルが先に読み込み終えていれば、前の版の AI タブはそこで待っている。
        // 後なら人のパネルの方がこちらへ渡しに来る。
        workspace
            .update_in(&mut cx, |workspace, window, cx| {
                let migrated = workspace
                    .panel::<BrowserPanel>(cx)
                    .map(|browser_panel| {
                        browser_panel
                            .update(cx, |browser_panel, _| browser_panel.take_migrated_ai_tabs())
                    })
                    .unwrap_or_default();
                panel.update(cx, |panel, cx| panel.adopt_tabs(migrated, window, cx));
            })
            .log_err();
        Ok(panel)
    }

    pub(crate) fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            tabs: PanelTabs::new(workspace, window, cx),
        }
    }

    /// AI タブを今のペインに開く。`url` が `None` なら空のページ。
    pub(crate) fn open_ai_tab(
        &mut self,
        url: Option<String>,
        placement: NewTabPlacement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<BrowserItem> {
        let pane = self.tabs.active_pane.clone();
        self.open_tab_in_pane(&pane, url, placement, window, cx)
    }

    /// 前の版が人のパネルに保存していた AI タブを、裏のタブとして末尾に足す。
    /// パネルが空だったなら最初の 1 枚を前に出す。
    pub(crate) fn adopt_tabs(
        &mut self,
        tabs: Vec<Entity<BrowserItem>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if tabs.is_empty() {
            return;
        }
        log::info!(
            "AI browser panel: moving {} AI tab(s) out of the browser panel",
            tabs.len()
        );
        let pane = self.tabs.active_pane.clone();
        pane.update(cx, |pane, cx| {
            let was_empty = pane.items_len() == 0;
            for item in tabs {
                let index = pane.items_len();
                pane.add_item_inner(Box::new(item), false, false, false, Some(index), window, cx);
            }
            if was_empty {
                pane.activate_item(0, false, false, window, cx);
            }
        });
    }

    /// `NewAiTab`: 利用者が自分で開く AI タブ (テスト対象のアプリに先にサインインして
    /// おくため)。パネルを出してフォーカスも移す。
    fn open_ai_tab_for_user(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(panel) = workspace.panel::<AiBrowserPanel>(cx) else {
            BrowserItem::open_with_profile(workspace, None, WebViewProfile::Ai, window, cx);
            return;
        };
        workspace.focus_panel::<AiBrowserPanel>(window, cx);
        panel.update(cx, |panel, cx| {
            panel.open_ai_tab(None, NewTabPlacement::Foreground, window, cx);
        });
    }

    /// AI がタブを開いたときにパネルを見せる。フォーカスは動かさない。ほかのコマンド
    /// (タブの選択も) は裏のまま動き、パネルを前に出さない。
    ///
    /// - もう画面に出ている (ドックが開いていて上か下の枠にいる) なら何もしない
    /// - 2 枚並べから隠してあった半分なら、元の枠へ戻す
    /// - ドックが別のパネルを出しているなら、その下に積む (人のブラウザーパネルが
    ///   積んであればそれと入れ替わる。人のパネルはステータスバーのボタンで戻せる)
    /// - ドックが何も出していなければ、このパネルを出す
    ///
    /// 閉じたドックは開く。
    pub(crate) fn reveal_without_focus(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut App,
    ) {
        for dock in workspace.all_docks() {
            let Some(index) = dock.read(cx).panel_index_for_type::<AiBrowserPanel>() else {
                continue;
            };
            dock.update(cx, |dock, cx| reveal_in_dock(dock, index, window, cx));
        }
    }

    /// A tab dropped on the empty state (which stands in for the panel's empty pane) moves into
    /// the pane, if it is an AI tab.
    fn drop_tab_on_empty_state(
        &mut self,
        tab: &DraggedTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !PanelKind::Ai.accepts(tab.item.as_ref(), cx) {
            return;
        }
        // Deferred: the tab asks this panel (being updated here) whether it may move in.
        let pane = self.tabs.active_pane.clone();
        let tab = tab.clone();
        window.defer(cx, move |window, cx| {
            pane.update(cx, |pane, cx| {
                pane.handle_tab_drop(&tab, 0, false, window, cx)
            });
        });
    }

    fn render_empty_state(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let ai_control = BrowserSettings::get_global(cx).ai_control;
        let state = if ai_control {
            Label::new(i18n::t!(
                "AI control of the built-in browser is on: AI agents can open and drive tabs here."
            ))
            .size(LabelSize::Small)
            .color(Color::Success)
        } else {
            Label::new(i18n::t!(
                "AI control of the built-in browser is off, so AI agents cannot open tabs here."
            ))
            .size(LabelSize::Small)
            .color(Color::Warning)
        };
        let setting_label = if ai_control {
            i18n::t!("Open the Setting")
        } else {
            i18n::t!("Turn On in Settings")
        };
        v_flex()
            .id("ai-browser-empty-state")
            .size_full()
            .p_4()
            .gap_3()
            .items_center()
            .justify_center()
            .drag_over::<DraggedTab>(|style, tab, _, cx| {
                if PanelKind::Ai.accepts(tab.item.as_ref(), cx) {
                    style.bg(cx.theme().colors().drop_target_background)
                } else {
                    style
                }
            })
            .on_drop(cx.listener(|this, tab: &DraggedTab, window, cx| {
                this.drop_tab_on_empty_state(tab, window, cx);
            }))
            .child(
                Icon::new(IconName::Sparkle)
                    .size(IconSize::Medium)
                    .color(Color::Muted),
            )
            .child(Headline::new(i18n::t!("AI Browser")).size(HeadlineSize::Small))
            .child(
                v_flex()
                    .max_w(rems(28.))
                    .gap_2()
                    .child(
                        Label::new(i18n::t!(
                            "AI agents open the browser tabs they drive here, to debug web apps. These AI tabs use their own browser profile, and their traffic is limited to the network allowlist; localhost is always allowed. Your own browser tabs are never shown to AI agents."
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .child(
                        Label::new(i18n::t!(
                            "You can also open an AI tab yourself, for example to sign in to the app under test first."
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .child(state),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("ai-browser-new-tab", i18n::t!("New AI Tab"))
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_ai_tab(None, NewTabPlacement::Foreground, window, cx);
                            })),
                    )
                    .child(
                        Button::new("ai-browser-open-setting", setting_label)
                            .tooltip(Tooltip::text(AI_CONTROL_SETTING))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(
                                    Box::new(zed_actions::OpenSettingsAt {
                                        path: AI_CONTROL_SETTING.to_string(),
                                        target: None,
                                    }),
                                    cx,
                                );
                            }),
                    ),
            )
            .into_any_element()
    }
}

/// [`AiBrowserPanel::reveal_without_focus`] の 1 つのドックぶん。
fn reveal_in_dock(dock: &mut Dock, index: usize, window: &mut Window, cx: &mut Context<Dock>) {
    let on_screen =
        dock.active_panel_index() == Some(index) || dock.secondary_panel_index() == Some(index);
    if dock.is_open() && on_screen {
        return;
    }
    if !(dock.is_open() && dock.restack_hidden_panel(index, window, cx)) {
        match dock.active_panel_index() {
            Some(active_index) if active_index != index => {
                dock.set_secondary_panel(Some(index), window, cx);
            }
            _ => dock.restore_active_panel(index, window, cx),
        }
    }
    if !dock.is_open() {
        dock.set_open(true, window, cx);
    }
}

impl BrowserTabsPanel for AiBrowserPanel {
    const KIND: PanelKind = PanelKind::Ai;

    fn tabs(&self) -> &PanelTabs {
        &self.tabs
    }

    fn tabs_mut(&mut self) -> &mut PanelTabs {
        &mut self.tabs
    }

    fn restored_foreign_item(&mut self, _: Box<dyn workspace::ItemHandle>, _: &mut Context<Self>) {
        // 行の無い (中身の記録が書かれなかった) タブは人のタブとして戻ってくる。
        // AI のパネルには置けないので捨てる。
        log::info!("AI browser panel: dropping a restored tab that is not an AI tab");
    }
}

impl Render for AiBrowserPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = if self.tabs.has_no_tabs(cx) {
            Some(self.render_empty_state(cx))
        } else {
            self.render_panes(window, cx)
        };
        // ブラウザーのキー割り当て (Ctrl+T / Ctrl+W など) は `BrowserPanel` の文脈に
        // 付いているので、こちらでも同じ文脈を立てる。
        let mut key_context = KeyContext::new_with_defaults();
        key_context.add(BROWSER_PANEL_KEY);
        key_context.add(AI_BROWSER_PANEL_KEY);
        v_flex()
            .key_context(key_context)
            .track_focus(&self.tabs.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .capture_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, _, cx| {
                this.activate_pane_at(event.position, cx);
            }))
            .on_action(cx.listener(|this, _: &NewPanelTab, window, cx| {
                this.open_ai_tab(None, NewTabPlacement::Foreground, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ClosePanelTab, window, cx| {
                let pane = this.tabs.active_pane.clone();
                pane.update(cx, |pane, cx| {
                    pane.close_active_item(&CloseActiveItem::default(), window, cx)
                        .detach_and_log_err(cx);
                });
            }))
            .on_action(cx.listener(|this, _: &SplitPanelRight, window, cx| {
                this.split_active_tab_right(window, cx)
            }))
            .on_action(cx.listener(|this, _: &FocusNextPaneGroup, window, cx| {
                this.focus_next_pane(window, cx)
            }))
            .children(content)
    }
}

impl Focusable for AiBrowserPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.tabs.focus_handle(cx)
    }
}

impl EventEmitter<PanelEvent> for AiBrowserPanel {}

impl Panel for AiBrowserPanel {
    fn persistent_name() -> &'static str {
        "AiBrowserPanel"
    }

    fn panel_key() -> &'static str {
        AI_BROWSER_PANEL_KEY
    }

    fn position(&self, _: &Window, cx: &App) -> DockPosition {
        BrowserSettings::get_global(cx).ai_panel.dock
    }

    fn position_is_valid(&self, _position: DockPosition) -> bool {
        true
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        settings::update_settings_file(self.tabs.fs.clone(), cx, move |settings, _| {
            settings
                .browser
                .get_or_insert_default()
                .ai_panel
                .get_or_insert_default()
                .dock = Some(position.into())
        });
    }

    fn default_size(&self, window: &Window, cx: &App) -> Pixels {
        let settings = &BrowserSettings::get_global(cx).ai_panel;
        match self.position(window, cx) {
            DockPosition::Left | DockPosition::Right => settings.default_width,
            DockPosition::Bottom => settings.default_height,
        }
    }

    /// AI による操作が切ってある間はボタンを出さない (使えない機能の入口を常に
    /// 並べない)。パネル自体はコマンドパレットから出せて、切ってあることを案内する。
    fn icon(&self, _: &Window, cx: &App) -> Option<IconName> {
        let settings = BrowserSettings::get_global(cx);
        (settings.ai_control && settings.ai_panel.button).then_some(IconName::Sparkle)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some(i18n::t!("AI Browser"))
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn pane(&self) -> Option<Entity<Pane>> {
        Some(self.tabs.active_pane.clone())
    }

    fn panes(&self) -> Vec<Entity<Pane>> {
        self.tabs.panes()
    }

    fn is_zoomed(&self, _: &Window, cx: &App) -> bool {
        self.tabs.is_zoomed(cx)
    }

    fn set_zoomed(&mut self, zoomed: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.tabs.set_zoomed(zoomed, cx);
        cx.notify();
    }

    fn set_active(&mut self, active: bool, _: &mut Window, cx: &mut Context<Self>) {
        if !active {
            self.tabs.hide_all_surfaces(HIDDEN_PANEL_INACTIVE, cx);
        }
        cx.notify();
    }

    fn activation_priority(&self) -> u32 {
        // 0..=7 are upstream's, 8 is the browser panel and 9 the call graph panel; duplicates
        // panic in `Dock::add_panel`.
        10
    }

    fn hide_button_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        Some(workspace::HideStatusItem::new(|settings| {
            settings
                .browser
                .get_or_insert_default()
                .ai_panel
                .get_or_insert_default()
                .button = Some(false);
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BrowserHost, BrowserViewEvent, PanelLayoutFormat, SerializedBrowserPanel,
        browser_automation::ai_tabs, browser_item::BrowserItemDb, serialize_pane_group,
    };
    use browser_control::{BrowserCommand, BrowserOutput};
    use futures::FutureExt as _;
    use gpui::{Task, TestAppContext, VisualTestContext};
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use std::time::Duration;
    use url::Url;
    use workspace::{MultiWorkspace, dock::test::TestPanel, item::test::TestItem};

    struct Fixture {
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        ai_panel: Entity<AiBrowserPanel>,
        user_panel: Entity<BrowserPanel>,
        /// Stands in for the agent panel: the right dock's other panel.
        agent: Entity<TestPanel>,
        /// The center tab that has the keyboard focus while the tools work.
        center_item: Entity<TestItem>,
    }

    fn set_ai_control(enabled: bool, cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.update_global(|store: &mut SettingsStore, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.browser.get_or_insert_default().ai_control = Some(enabled);
                });
            });
        });
    }

    async fn fixture(cx: &mut TestAppContext) -> (Fixture, VisualTestContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
        });
        set_ai_control(true, cx);
        // Process-wide; no test expects it to be absent.
        http_client::set_child_process_proxy(Some(
            Url::parse("http://127.0.0.1:49152/").expect("valid url"),
        ));
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle = cx.add_window({
            let project = project.clone();
            |window, cx| MultiWorkspace::test_new(project, window, cx)
        });
        let workspace = window_handle
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("the window holds a workspace");
        let mut visual = VisualTestContext::from_window(window_handle.into(), cx);
        let (agent, user_panel, ai_panel, center_item) =
            workspace.update_in(&mut visual, |workspace, window, cx| {
                let agent = cx.new(|cx| TestPanel::new(DockPosition::Right, 20, cx));
                workspace.add_panel(agent.clone(), window, cx);
                let user_panel = cx.new(|cx| BrowserPanel::new(workspace, window, cx));
                workspace.add_panel(user_panel.clone(), window, cx);
                let ai_panel = cx.new(|cx| AiBrowserPanel::new(workspace, window, cx));
                workspace.add_panel(ai_panel.clone(), window, cx);
                let center_item = cx.new(TestItem::new);
                workspace.add_item_to_active_pane(
                    Box::new(center_item.clone()),
                    None,
                    true,
                    window,
                    cx,
                );
                (agent, user_panel, ai_panel, center_item)
            });
        visual.run_until_parked();
        (
            Fixture {
                project,
                workspace,
                ai_panel,
                user_panel,
                agent,
                center_item,
            },
            visual,
        )
    }

    fn run(
        fixture: &Fixture,
        command: BrowserCommand,
        cx: &mut VisualTestContext,
    ) -> Result<BrowserOutput> {
        let project = fixture.project.clone();
        // Outside of the window's update: the provider looks the window up.
        let app: &mut TestAppContext = cx;
        let task: Task<Result<BrowserOutput>> = app.update(|cx| {
            browser_control::browser_control(cx)
                .expect("browser_panel::init registers the provider")
                .run(&project, command, cx)
        });
        cx.run_until_parked();
        task.now_or_never()
            .expect("the command should have finished")
    }

    /// The right dock as the user sees it: open or not, and the persistent names of the panels
    /// in its two slots.
    fn right_dock(
        fixture: &Fixture,
        cx: &mut VisualTestContext,
    ) -> (bool, Option<&'static str>, Option<&'static str>) {
        fixture.workspace.read_with(cx, |workspace, cx| {
            let dock = workspace.right_dock().read(cx);
            (
                dock.is_open(),
                dock.active_panel().map(|panel| panel.persistent_name()),
                dock.secondary_panel().map(|panel| panel.persistent_name()),
            )
        })
    }

    fn arrange_right_dock(
        fixture: &Fixture,
        open: bool,
        secondary: Option<&str>,
        cx: &mut VisualTestContext,
    ) {
        fixture.workspace.update_in(cx, |workspace, window, cx| {
            workspace.right_dock().update(cx, |dock, cx| {
                let agent_index = dock
                    .panel_index_for_type::<TestPanel>()
                    .expect("the stand-in agent panel is in the right dock");
                dock.set_open(true, window, cx);
                dock.activate_panel(agent_index, window, cx);
                let secondary_index =
                    secondary.and_then(|name| dock.panel_index_for_persistent_name(name, cx));
                dock.set_secondary_panel(secondary_index, window, cx);
                dock.set_open(open, window, cx);
            });
        });
        cx.run_until_parked();
    }

    fn focus_center_item(fixture: &Fixture, cx: &mut VisualTestContext) {
        let item = fixture.center_item.clone();
        cx.update(|window, cx| window.focus(&item.focus_handle(cx), cx));
        cx.run_until_parked();
        assert!(center_item_has_focus(fixture, cx));
    }

    fn center_item_has_focus(fixture: &Fixture, cx: &mut VisualTestContext) -> bool {
        let item = fixture.center_item.clone();
        cx.update(|window, cx| item.focus_handle(cx).is_focused(window))
    }

    fn ai_panel_urls(fixture: &Fixture, cx: &mut VisualTestContext) -> Vec<String> {
        fixture.ai_panel.read_with(cx, |panel, cx| {
            panel
                .tabs
                .panes()
                .iter()
                .flat_map(|pane| {
                    pane.read(cx)
                        .items()
                        .filter_map(|item| item.downcast::<BrowserItem>())
                        .map(|item| item.read(cx).view().read(cx).current_url().to_string())
                        .collect::<Vec<_>>()
                })
                .collect()
        })
    }

    fn open_tab_command(url: &str) -> BrowserCommand {
        BrowserCommand::OpenTab {
            url: Some(url.to_string()),
        }
    }

    #[gpui::test]
    async fn a_tab_a_tool_opens_lands_in_the_ai_panel_without_taking_focus(
        cx: &mut TestAppContext,
    ) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        arrange_right_dock(&fixture, false, None, cx);
        focus_center_item(&fixture, cx);

        run(&fixture, open_tab_command("http://localhost:3000/"), cx).expect("opened");

        assert_eq!(ai_panel_urls(&fixture, cx), vec!["http://localhost:3000/"]);
        assert_eq!(
            right_dock(&fixture, cx),
            (true, Some("TestPanel"), Some("AiBrowserPanel")),
            "the closed dock opens with the AI browser below the panel it shows"
        );
        assert!(
            center_item_has_focus(&fixture, cx),
            "the keyboard focus stays where the user left it"
        );
        fixture.user_panel.read_with(cx, |panel, cx| {
            assert!(
                panel.tabs.has_no_tabs(cx),
                "the user's browser is untouched"
            );
            assert!(!panel.revealed_by_user);
        });
        fixture.agent.read_with(cx, |agent, _| {
            assert!(agent.active, "the panel the dock shows stays on screen");
        });
        // Drawing the revealed panel creates nothing that takes the focus either.
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        assert!(center_item_has_focus(&fixture, cx));
    }

    #[gpui::test]
    async fn the_ai_browser_takes_the_user_browser_s_place_beside_the_agent(
        cx: &mut TestAppContext,
    ) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        // The fork's default pair: the agent panel with the user's browser below it.
        arrange_right_dock(&fixture, true, Some("BrowserPanel"), cx);
        assert_eq!(
            right_dock(&fixture, cx),
            (true, Some("TestPanel"), Some("BrowserPanel"))
        );
        focus_center_item(&fixture, cx);

        run(&fixture, open_tab_command("http://localhost:3000/"), cx).expect("opened");

        assert_eq!(
            right_dock(&fixture, cx),
            (true, Some("TestPanel"), Some("AiBrowserPanel"))
        );
        assert!(center_item_has_focus(&fixture, cx));
        fixture.workspace.read_with(cx, |workspace, cx| {
            assert!(
                workspace
                    .right_dock()
                    .read(cx)
                    .panel_index_for_type::<BrowserPanel>()
                    .is_some(),
                "the user's browser stays in the dock, behind its button"
            );
        });
    }

    #[gpui::test]
    async fn a_hidden_half_of_the_pair_comes_back_for_the_ai(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        arrange_right_dock(&fixture, true, Some("AiBrowserPanel"), cx);
        fixture.ai_panel.update_in(cx, |panel, window, cx| {
            panel.open_ai_tab(
                Some("http://localhost:3000/".to_string()),
                NewTabPlacement::ForegroundWithoutFocus,
                window,
                cx,
            );
        });
        cx.run_until_parked();
        // The user hid the AI browser half of the pair (a click on its button).
        fixture.workspace.update_in(cx, |workspace, window, cx| {
            workspace.right_dock().update(cx, |dock, cx| {
                let index = dock
                    .panel_index_for_type::<AiBrowserPanel>()
                    .expect("the AI browser is in the right dock");
                assert!(dock.hide_stacked_panel(index, window, cx));
            });
        });
        cx.run_until_parked();
        assert_eq!(right_dock(&fixture, cx), (true, Some("TestPanel"), None));
        focus_center_item(&fixture, cx);

        let tab = fixture
            .workspace
            .update(cx, |workspace, cx| ai_tabs(workspace, cx))
            .pop()
            .expect("the AI tab is listed");
        // Selecting a tab works on it where it is: nothing out of sight comes forward.
        run(&fixture, BrowserCommand::SelectTab { tab: tab.id }, cx).expect("selected");
        assert_eq!(right_dock(&fixture, cx), (true, Some("TestPanel"), None));
        assert!(center_item_has_focus(&fixture, cx));

        // Opening a tab does bring the hidden half back.
        run(&fixture, open_tab_command("http://localhost:4000/"), cx).expect("opened");
        assert_eq!(
            right_dock(&fixture, cx),
            (true, Some("TestPanel"), Some("AiBrowserPanel"))
        );
        assert!(center_item_has_focus(&fixture, cx));

        // Already on screen: another tab changes nothing about the dock.
        run(&fixture, open_tab_command("http://localhost:5173/"), cx).expect("opened");
        assert_eq!(
            right_dock(&fixture, cx),
            (true, Some("TestPanel"), Some("AiBrowserPanel"))
        );
        assert_eq!(
            ai_panel_urls(&fixture, cx),
            vec![
                "http://localhost:3000/",
                "http://localhost:4000/",
                "http://localhost:5173/"
            ]
        );

        // Hidden from the upper slot, it goes back to the upper slot.
        fixture.workspace.update_in(cx, |workspace, window, cx| {
            workspace.right_dock().update(cx, |dock, cx| {
                let index = dock
                    .panel_index_for_type::<AiBrowserPanel>()
                    .expect("the AI browser is in the right dock");
                let agent_index = dock
                    .panel_index_for_type::<TestPanel>()
                    .expect("the stand-in agent panel is in the right dock");
                dock.activate_panel(index, window, cx);
                dock.set_secondary_panel(Some(agent_index), window, cx);
                assert!(dock.hide_stacked_panel(index, window, cx));
            });
        });
        cx.run_until_parked();
        assert_eq!(right_dock(&fixture, cx), (true, Some("TestPanel"), None));
        run(&fixture, open_tab_command("http://localhost:8080/"), cx).expect("opened");
        assert_eq!(
            right_dock(&fixture, cx),
            (true, Some("AiBrowserPanel"), Some("TestPanel"))
        );
    }

    #[gpui::test]
    async fn opening_beside_an_emptied_user_browser_keeps_the_focus(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        // The dock closed with the user's browser on top after its last tab was closed: the
        // browser opens a tab of its own the next time it is shown.
        fixture.workspace.update_in(cx, |workspace, window, cx| {
            workspace.right_dock().update(cx, |dock, cx| {
                let index = dock
                    .panel_index_for_type::<BrowserPanel>()
                    .expect("the user's browser is in the right dock");
                dock.set_open(false, window, cx);
                dock.restore_active_panel(index, window, cx);
            });
        });
        cx.run_until_parked();
        assert_eq!(
            right_dock(&fixture, cx),
            (false, Some("BrowserPanel"), None)
        );
        fixture
            .user_panel
            .read_with(cx, |panel, cx| assert!(panel.tabs.has_no_tabs(cx)));
        focus_center_item(&fixture, cx);

        run(&fixture, open_tab_command("http://localhost:3000/"), cx).expect("opened");

        assert_eq!(
            right_dock(&fixture, cx),
            (true, Some("BrowserPanel"), Some("AiBrowserPanel"))
        );
        fixture.user_panel.read_with(cx, |panel, cx| {
            assert!(
                !panel.tabs.has_no_tabs(cx),
                "the user's browser has its tab"
            );
        });
        assert!(
            center_item_has_focus(&fixture, cx),
            "the user's browser does not take the focus for the tab it opens"
        );

        // Asked for by the user, the emptied browser hands its new tab the focus as before.
        fixture.workspace.update_in(cx, |workspace, window, cx| {
            workspace
                .right_dock()
                .update(cx, |dock, cx| dock.set_open(false, window, cx));
        });
        fixture.user_panel.update_in(cx, |panel, window, cx| {
            let pane = panel.tabs.active_pane.clone();
            pane.update(cx, |pane, cx| {
                pane.close_active_item(&CloseActiveItem::default(), window, cx)
                    .detach_and_log_err(cx);
            });
        });
        cx.run_until_parked();
        fixture
            .user_panel
            .read_with(cx, |panel, cx| assert!(panel.tabs.has_no_tabs(cx)));
        fixture.workspace.update_in(cx, |workspace, window, cx| {
            workspace.focus_panel::<BrowserPanel>(window, cx);
        });
        cx.run_until_parked();
        let user_panel = fixture.user_panel.clone();
        assert!(
            cx.update(|window, cx| user_panel.read(cx).tabs.contains_focus(window, cx)),
            "the tab the user's browser opens for them has the focus"
        );
    }

    #[gpui::test]
    async fn open_in_center_moves_the_tab_of_the_browser_that_has_the_focus(
        cx: &mut TestAppContext,
    ) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        fixture.user_panel.update_in(cx, |panel, window, cx| {
            panel.open_tab(
                Some("https://outlook.office.com/".to_string()),
                NewTabPlacement::ForegroundWithoutFocus,
                window,
                cx,
            );
        });
        fixture.workspace.update_in(cx, |workspace, window, cx| {
            AiBrowserPanel::open_ai_tab_for_user(workspace, window, cx);
        });
        cx.run_until_parked();

        cx.dispatch_action(crate::OpenInCenter);
        cx.run_until_parked();

        assert!(
            fixture
                .ai_panel
                .read_with(cx, |panel, cx| panel.tabs.has_no_tabs(cx)),
            "the AI tab in front of the focused AI browser moved"
        );
        fixture.user_panel.read_with(cx, |panel, cx| {
            assert!(!panel.tabs.has_no_tabs(cx), "the user's tab stays put");
        });
        fixture.workspace.read_with(cx, |workspace, cx| {
            let center_ai_tabs = workspace
                .active_pane()
                .read(cx)
                .items_of_type::<BrowserItem>()
                .filter(|item| item.read(cx).view().read(cx).is_ai_tab())
                .count();
            assert_eq!(center_ai_tabs, 1);
        });
    }

    #[gpui::test]
    async fn an_ai_tab_dragged_to_another_window_lands_only_where_ai_tabs_may_be(
        cx: &mut TestAppContext,
    ) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        run(&fixture, open_tab_command("http://localhost:3000/"), cx).expect("opened");
        let (source_pane, item_id) = fixture.ai_panel.read_with(cx, |panel, cx| {
            let pane = panel.tabs.active_pane.clone();
            let item_id = pane
                .read(cx)
                .active_item()
                .expect("the AI tab is open")
                .item_id();
            (pane, item_id)
        });

        // A second window on the same project (a torn-off tab's window), with both browsers and
        // the user's browser on screen.
        let other_window = {
            let project = fixture.project.clone();
            // A second `Workspace::test_new` over the same client panics registering its
            // handlers again, so the window shares the first one's app state.
            let app_state = fixture
                .workspace
                .read_with(cx, |workspace, _| workspace.app_state().clone());
            let app: &mut TestAppContext = cx;
            app.add_window(|window, cx| {
                let workspace =
                    cx.new(|cx| Workspace::new(Default::default(), project, app_state, window, cx));
                MultiWorkspace::new(workspace, window, cx)
            })
        };
        let (other_workspace, other_user_pane, other_ai_panel) = other_window
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace
                    .workspace()
                    .clone()
                    .update(cx, |workspace, cx| {
                        let user_panel = cx.new(|cx| BrowserPanel::new(workspace, window, cx));
                        workspace.add_panel(user_panel.clone(), window, cx);
                        let ai_panel = cx.new(|cx| AiBrowserPanel::new(workspace, window, cx));
                        workspace.add_panel(ai_panel.clone(), window, cx);
                        workspace.right_dock().update(cx, |dock, cx| {
                            let index = dock
                                .panel_index_for_type::<BrowserPanel>()
                                .expect("the user's browser is in the right dock");
                            dock.restore_active_panel(index, window, cx);
                            dock.set_open(true, window, cx);
                        });
                        let user_pane = user_panel.read(cx).tabs.active_pane.clone();
                        (cx.entity(), user_pane, ai_panel)
                    })
            })
            .expect("the second window is open");
        cx.run_until_parked();

        let drop_on = |pane: Entity<Pane>, cx: &mut VisualTestContext| {
            let source_pane = source_pane.clone();
            fixture.workspace.update_in(cx, |workspace, window, cx| {
                workspace.move_item_to_window(
                    source_pane,
                    item_id,
                    other_window,
                    Some(pane),
                    None,
                    window,
                    cx,
                );
            });
            cx.run_until_parked();
        };
        let in_source = |cx: &mut VisualTestContext| {
            source_pane.read_with(cx, |pane, _| {
                pane.items().any(|item| item.item_id() == item_id)
            })
        };

        // The user's browser of the other window refuses it, as it would in this window.
        drop_on(other_user_pane, cx);
        assert!(in_source(cx), "the AI tab stays where it was");
        other_ai_panel.read_with(cx, |panel, cx| assert!(panel.tabs.has_no_tabs(cx)));

        // The other window's editor area takes it.
        let other_center =
            other_workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        drop_on(other_center.clone(), cx);
        assert!(!in_source(cx));
        other_center.read_with(cx, |pane, _| {
            assert!(pane.items().any(|item| item.item_id() == item_id))
        });
    }

    #[gpui::test]
    async fn the_empty_ai_browser_takes_only_dropped_ai_tabs(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        let (center, ai_item) = fixture.workspace.update_in(cx, |workspace, window, cx| {
            let workspace_handle = workspace.weak_handle();
            let fs = workspace.app_state().fs.clone();
            let ai_item = cx.new(|cx| {
                BrowserItem::new_ai(
                    workspace_handle,
                    fs,
                    BrowserHost::EditorTab,
                    Some("http://localhost:3000/".to_string()),
                    window,
                    cx,
                )
            });
            workspace.add_item_to_active_pane(Box::new(ai_item.clone()), None, false, window, cx);
            (workspace.active_pane().clone(), ai_item)
        });
        let user_item = fixture.user_panel.update_in(cx, |panel, window, cx| {
            panel.open_tab(
                Some("https://outlook.office.com/".to_string()),
                NewTabPlacement::ForegroundWithoutFocus,
                window,
                cx,
            );
            panel
                .tabs
                .active_pane
                .read(cx)
                .active_item()
                .expect("the user's tab is open")
        });
        cx.run_until_parked();
        assert!(
            fixture
                .ai_panel
                .read_with(cx, |panel, cx| panel.tabs.has_no_tabs(cx))
        );
        let user_pane = fixture
            .user_panel
            .read_with(cx, |panel, _| panel.tabs.active_pane.clone());
        let dragged = |pane: &Entity<Pane>, item: &dyn workspace::ItemHandle| DraggedTab {
            pane: pane.clone(),
            item: item.boxed_clone(),
            ix: 0,
            detail: 0,
            is_active: true,
        };

        fixture.ai_panel.update_in(cx, |panel, window, cx| {
            panel.drop_tab_on_empty_state(&dragged(&user_pane, user_item.as_ref()), window, cx);
        });
        cx.run_until_parked();
        assert!(
            fixture
                .ai_panel
                .read_with(cx, |panel, cx| panel.tabs.has_no_tabs(cx)),
            "the user's tab is refused"
        );
        user_pane.read_with(cx, |pane, _| {
            assert!(
                pane.items()
                    .any(|item| item.item_id() == user_item.item_id()),
                "and stays where it was"
            );
        });

        fixture.ai_panel.update_in(cx, |panel, window, cx| {
            panel.drop_tab_on_empty_state(&dragged(&center, &ai_item), window, cx);
        });
        cx.run_until_parked();
        assert_eq!(ai_panel_urls(&fixture, cx), vec!["http://localhost:3000/"]);
        center.read_with(cx, |pane, _| {
            assert!(
                !pane
                    .items()
                    .any(|item| item.item_id() == ai_item.entity_id())
            );
        });
    }

    #[gpui::test]
    async fn a_new_tab_asked_for_inside_an_ai_tab_is_an_ai_tab(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        fixture.workspace.update_in(cx, |workspace, window, cx| {
            AiBrowserPanel::open_ai_tab_for_user(workspace, window, cx);
        });
        cx.run_until_parked();

        // Ctrl+T with the AI tab focused: the tab handles it before the panel does.
        cx.dispatch_action(NewPanelTab);
        cx.run_until_parked();

        fixture.ai_panel.read_with(cx, |panel, cx| {
            let tabs = panel
                .tabs
                .panes()
                .iter()
                .flat_map(|pane| {
                    pane.read(cx)
                        .items()
                        .filter_map(|item| item.downcast::<BrowserItem>())
                        .map(|item| item.read(cx).view().read(cx).is_ai_tab())
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            assert_eq!(tabs, vec![true, true]);
        });
        fixture.workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace
                    .active_pane()
                    .read(cx)
                    .items_of_type::<BrowserItem>()
                    .count(),
                0,
                "no tab of the user's profile opens in the editor area"
            );
        });
    }

    #[gpui::test]
    async fn closing_the_last_ai_tab_keeps_the_ai_browser_on_screen(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        arrange_right_dock(&fixture, true, Some("AiBrowserPanel"), cx);
        run(&fixture, open_tab_command("http://localhost:3000/"), cx).expect("opened");
        let tab = fixture
            .workspace
            .update(cx, |workspace, cx| ai_tabs(workspace, cx))
            .pop()
            .expect("the AI tab is listed");

        run(&fixture, BrowserCommand::CloseTab { tab: tab.id }, cx).expect("closed");

        assert!(
            fixture
                .ai_panel
                .read_with(cx, |panel, cx| panel.tabs.has_no_tabs(cx))
        );
        assert_eq!(
            right_dock(&fixture, cx),
            (true, Some("TestPanel"), Some("AiBrowserPanel")),
            "the empty AI browser explains itself instead of closing"
        );
    }

    #[gpui::test]
    async fn a_dock_that_shows_nothing_shows_the_ai_browser(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        // The AI browser moved to the bottom dock, where nothing else lives.
        cx.update(|_, cx| {
            cx.update_global(|store: &mut SettingsStore, cx| {
                store.update_user_settings(cx, |settings| {
                    settings
                        .browser
                        .get_or_insert_default()
                        .ai_panel
                        .get_or_insert_default()
                        .dock = Some(settings::DockPosition::Bottom);
                });
            });
        });
        cx.run_until_parked();
        fixture.workspace.read_with(cx, |workspace, cx| {
            assert!(
                workspace
                    .bottom_dock()
                    .read(cx)
                    .panel_index_for_type::<AiBrowserPanel>()
                    .is_some()
            );
        });
        focus_center_item(&fixture, cx);

        run(&fixture, open_tab_command("http://localhost:3000/"), cx).expect("opened");

        fixture.workspace.read_with(cx, |workspace, cx| {
            let dock = workspace.bottom_dock().read(cx);
            assert!(dock.is_open());
            assert_eq!(
                dock.active_panel().map(|panel| panel.persistent_name()),
                Some("AiBrowserPanel")
            );
        });
        assert!(center_item_has_focus(&fixture, cx));
    }

    #[gpui::test]
    async fn listing_covers_the_ai_panel_and_the_editor_area_only(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        run(&fixture, open_tab_command("http://localhost:3000/"), cx).expect("opened");
        run(&fixture, open_tab_command("http://localhost:5173/"), cx).expect("opened");
        fixture.user_panel.update_in(cx, |panel, window, cx| {
            panel.open_tab(
                Some("https://outlook.office.com/".to_string()),
                NewTabPlacement::Foreground,
                window,
                cx,
            );
        });
        // The user dragged the second AI tab to the editor area.
        fixture.workspace.update_in(cx, |workspace, window, cx| {
            let source = workspace
                .panel::<AiBrowserPanel>(cx)
                .expect("the AI browser is in the workspace")
                .read(cx)
                .tabs
                .active_pane
                .clone();
            let item_id = source
                .read(cx)
                .active_item()
                .expect("the AI tab is in front")
                .item_id();
            let center = workspace.active_pane().clone();
            workspace::move_item(&source, &center, item_id, 0, false, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(ai_panel_urls(&fixture, cx), vec!["http://localhost:3000/"]);

        let listing = run(&fixture, BrowserCommand::ListTabs, cx).expect("listing");
        assert!(
            listing.text.contains("http://localhost:3000/"),
            "{}",
            listing.text
        );
        assert!(
            listing.text.contains("http://localhost:5173/"),
            "an AI tab in the editor area is still the AI's: {}",
            listing.text
        );
        assert!(!listing.text.contains("outlook"), "{}", listing.text);
    }

    #[gpui::test]
    async fn the_user_opens_an_ai_tab_in_the_ai_panel(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        fixture.workspace.update_in(cx, |workspace, window, cx| {
            AiBrowserPanel::open_ai_tab_for_user(workspace, window, cx);
        });
        cx.run_until_parked();

        fixture.ai_panel.read_with(cx, |panel, cx| {
            let view = panel.tabs.active_view(cx).expect("the AI tab is in front");
            assert!(view.read(cx).is_ai_tab());
            assert_eq!(view.read(cx).host(), BrowserHost::AiPanel);
        });
        fixture
            .user_panel
            .read_with(cx, |panel, cx| assert!(panel.tabs.has_no_tabs(cx)));
        let ai_panel = fixture.ai_panel.clone();
        assert!(
            cx.update(|window, cx| ai_panel.read(cx).tabs.contains_focus(window, cx)),
            "the user asked for it, so the focus moves to it"
        );
    }

    #[gpui::test]
    async fn tabs_opened_from_an_ai_tab_stay_in_the_ai_panel(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        run(&fixture, open_tab_command("http://localhost:3000/"), cx).expect("opened");
        let view = fixture.ai_panel.read_with(cx, |panel, cx| {
            panel.tabs.active_view(cx).expect("the AI tab is in front")
        });
        // A page's "open in a new tab" (a `target=_blank` link, a middle click).
        view.update(cx, |_, cx| {
            cx.emit(BrowserViewEvent::OpenInNewTab(
                "http://localhost:3000/next".to_string(),
            ))
        });
        cx.run_until_parked();
        assert_eq!(
            ai_panel_urls(&fixture, cx),
            vec!["http://localhost:3000/", "http://localhost:3000/next"]
        );

        // Duplicating an AI tab into a split keeps it in the AI panel, as an AI tab.
        fixture.ai_panel.update_in(cx, |panel, window, cx| {
            panel.tabs.active_pane.update(cx, |pane, cx| {
                pane.activate_item(0, false, false, window, cx);
            });
            panel.split_active_tab_right(window, cx);
        });
        cx.run_until_parked();
        fixture.ai_panel.read_with(cx, |panel, cx| {
            assert_eq!(panel.tabs.panes().len(), 2);
            for pane in panel.tabs.panes() {
                for item in pane.read(cx).items() {
                    let item = item
                        .downcast::<BrowserItem>()
                        .expect("only browser tabs are in the AI panel");
                    assert!(item.read(cx).view().read(cx).is_ai_tab());
                }
            }
        });
        fixture
            .user_panel
            .read_with(cx, |panel, cx| assert!(panel.tabs.has_no_tabs(cx)));
    }

    #[gpui::test]
    async fn the_layout_round_trips_under_its_own_key(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        fixture.workspace.read_with(cx, |workspace, cx| {
            let dock = workspace.right_dock().read(cx);
            assert!(
                dock.panel_index_for_persistent_name("AiBrowserPanel", cx)
                    .is_some()
            );
        });
        let workspace_id = fixture.workspace.update(cx, |workspace, _| {
            workspace.set_random_database_id();
            workspace
                .database_id()
                .expect("the workspace has an id now")
        });
        run(&fixture, open_tab_command("http://localhost:3000/"), cx).expect("opened");
        run(&fixture, open_tab_command("http://localhost:5173/"), cx).expect("opened");
        fixture.ai_panel.update_in(cx, |panel, window, cx| {
            panel.split_active_tab_right(window, cx)
        });
        cx.run_until_parked();

        // The tabs' own rows, as `BrowserItem::serialize` writes them.
        let rows = fixture.ai_panel.read_with(cx, |panel, cx| {
            panel
                .tabs
                .panes()
                .iter()
                .flat_map(|pane| {
                    pane.read(cx)
                        .items()
                        .filter_map(|item| {
                            let url = item
                                .downcast::<BrowserItem>()?
                                .read(cx)
                                .view()
                                .read(cx)
                                .current_url()
                                .to_string();
                            Some((item.item_id().as_u64(), url))
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        });
        let db = cx.update(|_, cx| BrowserItemDb::global(cx));
        db.write(move |connection| {
            connection.exec(&format!(
                "INSERT OR IGNORE INTO workspaces (workspace_id) VALUES ({})",
                i64::from(workspace_id)
            ))?()
        })
        .await
        .expect("the browser_items rows need a workspace to reference");
        for (item_id, url) in rows {
            db.save_tab(item_id, workspace_id, url, true)
                .await
                .expect("saving an AI tab");
        }
        fixture
            .ai_panel
            .update(cx, |panel, cx| BrowserTabsPanel::serialize(panel, cx));
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();

        let async_cx = cx.update(|window, cx| window.to_async(cx));
        let restored = AiBrowserPanel::load(fixture.workspace.downgrade(), async_cx)
            .await
            .expect("the panel should load");
        cx.run_until_parked();
        restored.read_with(cx, |panel, cx| {
            let urls = panel
                .tabs
                .panes()
                .iter()
                .map(|pane| {
                    pane.read(cx)
                        .items()
                        .filter_map(|item| item.downcast::<BrowserItem>())
                        .map(|item| {
                            let view = item.read(cx).view().read(cx);
                            assert!(view.is_ai_tab());
                            assert_eq!(view.host(), BrowserHost::AiPanel);
                            view.current_url().to_string()
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            assert_eq!(
                urls,
                vec![
                    vec!["http://localhost:3000/".to_string()],
                    vec!["http://localhost:5173/".to_string()]
                ]
            );
        });
        // The user's browser panel has a key of its own, which this did not write.
        let user_key = fixture.workspace.read_with(cx, |workspace, _| {
            crate::serialization_key::<BrowserPanel>(workspace).expect("the workspace has an id")
        });
        let kvp = cx.update(|_, cx| db::kvp::KeyValueStore::global(cx));
        assert_eq!(kvp.read_kvp(&user_key).expect("reading the kvp"), None);

        // Written without a workspace id (the url list) and without the AI flags of a layout
        // from the browser panel, every tab still comes back as an AI tab.
        let root = fixture.ai_panel.read_with(cx, |panel, cx| {
            serialize_pane_group(
                &panel.tabs.center.root,
                &panel.tabs.active_pane,
                PanelLayoutFormat::Urls,
                cx,
            )
        });
        let json = serde_json::to_string(&SerializedBrowserPanel { root })
            .expect("the layout should serialize")
            .replace(",\"ai_tabs\":[0]", "");
        assert!(!json.contains("ai_tabs"), "{json}");
        let without_flags: SerializedBrowserPanel =
            serde_json::from_str(&json).expect("the layout should deserialize");
        let from_urls = fixture.workspace.update_in(cx, |workspace, window, cx| {
            cx.new(|cx| AiBrowserPanel::new(workspace, window, cx))
        });
        let mut async_cx = cx.update(|window, cx| window.to_async(cx));
        assert!(restore(&from_urls, without_flags, None, &mut async_cx).await);
        cx.run_until_parked();
        from_urls.read_with(cx, |panel, cx| {
            assert_eq!(panel.tabs.panes().len(), 2);
            for pane in panel.tabs.panes() {
                for item in pane.read(cx).items() {
                    let item = item.downcast::<BrowserItem>().expect("a browser tab");
                    assert!(item.read(cx).view().read(cx).is_ai_tab());
                }
            }
        });
    }

    #[gpui::test]
    async fn the_panel_s_button_shows_only_while_ai_control_is_on(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        let ai_panel = fixture.ai_panel.clone();
        let icon = move |cx: &mut VisualTestContext| {
            ai_panel.update_in(cx, |panel, window, cx| panel.icon(window, cx))
        };
        assert_eq!(icon(cx), Some(IconName::Sparkle));
        set_ai_control(false, cx);
        assert_eq!(icon(cx), None);

        // The empty panel explains itself either way.
        fixture.workspace.update_in(cx, |workspace, window, cx| {
            workspace.focus_panel::<AiBrowserPanel>(window, cx);
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        set_ai_control(true, cx);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        assert!(
            fixture
                .ai_panel
                .read_with(cx, |panel, cx| panel.tabs.has_no_tabs(cx))
        );
    }
}
