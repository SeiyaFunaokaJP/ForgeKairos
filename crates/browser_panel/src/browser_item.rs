//! エディター領域 (中央ペイン) のタブとして開く内蔵ブラウザー (ADR-F19)。
//!
//! ドックのパネル版と違い、こちらは**いくつでも開ける**。1 タブ = 1 つの
//! [`BrowserView`] = 1 つの WebView2 コントローラーで、WebView2 環境だけが
//! プロセス内で共有される。
//!
//! ## 可視性がこのファイルの肝
//!
//! WebView2 は GPUI のトップレベル HWND の**子 HWND** なので、GPUI の描画には
//! 従わない。「タブが裏に回った」「ペインがズームで隠れた」「別のワークスペースに
//! 切り替わった」を GPUI 側から通知してくれる汎用のイベントは無いので、
//!
//! - **隠すのは push**: [`Item::deactivated`]・ズーム変化・そして下のウォッチドッグ
//! - **見せるのは pull**: [`Render::render`] が呼ばれた = 画面に出ている
//!
//! と向きを分けてある。ウォッチドッグは「1 周期のあいだ一度も `render` が
//! 呼ばれなかった」を検出する。周期ごとに [`Context::notify`] を撃つのがミソで、
//! gpui の `App::notify` は**そのビューがウィンドウに実際に載っているときだけ**
//! 再描画を起こす (`tracked_entities` で絞っている) ため、載っていなければ
//! 次の周期まで `render` が呼ばれず、そこで確実に隠せる。

use std::time::Duration;

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Subscription, Task,
    WeakEntity, Window,
};
use settings::Settings as _;
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

use crate::{BrowserHost, BrowserSettings, BrowserView, BrowserViewEvent};

/// 「1 周期のあいだ一度も描画されなかった」を判定する間隔。短くするほど隠れるのが
/// 速くなるが、周期ごとにウィンドウを 1 回描き直すので短くしすぎない。よくある
/// 経路 (タブ切り替え・タブを閉じる・ズーム) は即座に隠す方でも押さえてある。
const VISIBILITY_POLL_INTERVAL: Duration = Duration::from_millis(500);

const HIDDEN_DEACTIVATED: &str = "the browser tab is no longer the active tab in its pane";
const HIDDEN_OFF_SCREEN: &str = "the browser tab was not painted for a full poll interval";
const HIDDEN_ZOOM_CHANGED: &str = "the workspace zoom state changed";
const HIDDEN_REMOVED: &str = "the browser tab was removed from its pane";

/// ウォッチドッグ 1 周期分の判定。純粋関数にしてテストできるようにしてある。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VisibilityTick {
    /// 直前の周期で描画された。次の周期でも判定できるよう再描画を促す。
    KeepAlive,
    /// 1 周期まるごと描画されなかった。子ウィンドウを隠す。
    Hide,
    /// すでに隠してある。何もしない (再描画も起こさない)。
    Idle,
}

pub(crate) fn visibility_tick(
    painted_since_last_tick: bool,
    surface_shown: bool,
) -> VisibilityTick {
    match (painted_since_last_tick, surface_shown) {
        (true, _) => VisibilityTick::KeepAlive,
        (false, true) => VisibilityTick::Hide,
        (false, false) => VisibilityTick::Idle,
    }
}

pub struct BrowserItem {
    workspace: WeakEntity<Workspace>,
    view: Entity<BrowserView>,
    /// 直近の周期で [`Render::render`] が呼ばれたか。
    painted_since_last_tick: bool,
    /// 子 HWND を見せているつもりかどうか。隠してある間はウォッチドッグが
    /// 再描画を促さないので、裏に回ったタブは何のコストも掛からない。
    surface_shown: bool,
    _visibility_watchdog: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl BrowserItem {
    /// アクティブなペインに新しいブラウザータブを開く。`url` が `None` なら
    /// `browser.home_url` を開き、それも空ならブックマークを開いた空のタブになる。
    pub fn open(
        workspace: &mut Workspace,
        url: Option<String>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let item = cx.new(|cx| BrowserItem::new(workspace, url, window, cx));
        workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
    }

    fn new(
        workspace: &Workspace,
        url: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let view = cx.new(|cx| {
            BrowserView::new(
                workspace.weak_handle(),
                workspace.app_state().fs.clone(),
                BrowserHost::EditorTab,
                window,
                cx,
            )
        });

        let initial_url = url.unwrap_or_else(|| BrowserSettings::get_global(cx).home_url.clone());
        view.update(cx, |view, cx| {
            if initial_url.trim().is_empty() {
                view.open_bookmarks_pane(cx);
            } else {
                view.set_initial_url(initial_url, window, cx);
            }
        });

        let mut subscriptions = vec![cx.subscribe_in(&view, window, Self::on_view_event)];
        if let Some(workspace_entity) = workspace.weak_handle().upgrade() {
            subscriptions.push(cx.subscribe(
                &workspace_entity,
                |this, _, event: &workspace::Event, cx| {
                    if matches!(event, workspace::Event::ZoomChanged) {
                        // ズームで隠れたかどうかは描画されるまで分からない。まず
                        // 隠して、まだ見えているなら次の描画で戻す。
                        this.hide_surface(HIDDEN_ZOOM_CHANGED, cx);
                    }
                },
            ));
        }

        Self {
            workspace: workspace.weak_handle(),
            view,
            painted_since_last_tick: false,
            surface_shown: false,
            _visibility_watchdog: Self::start_visibility_watchdog(cx),
            _subscriptions: subscriptions,
        }
    }

    fn start_visibility_watchdog(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(VISIBILITY_POLL_INTERVAL)
                    .await;
                // 更新に失敗する = タブが閉じられてエンティティが消えた。
                if this
                    .update(cx, |this, cx| this.tick_visibility(cx))
                    .is_err()
                {
                    break;
                }
            }
        })
    }

    fn tick_visibility(&mut self, cx: &mut Context<Self>) {
        let painted = std::mem::take(&mut self.painted_since_last_tick);
        match visibility_tick(painted, self.surface_shown) {
            VisibilityTick::KeepAlive => cx.notify(),
            VisibilityTick::Hide => self.hide_surface(HIDDEN_OFF_SCREEN, cx),
            VisibilityTick::Idle => {}
        }
    }

    fn hide_surface(&mut self, reason: &'static str, cx: &mut App) {
        self.surface_shown = false;
        self.view
            .update(cx, |view, _| view.hide_surface_from_host(reason));
    }

    fn on_view_event(
        &mut self,
        _: &Entity<BrowserView>,
        event: &BrowserViewEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            BrowserViewEvent::LocationChanged => {
                cx.emit(ItemEvent::UpdateTab);
                cx.notify();
            }
            // エディター領域のタブからの「新しいタブで開く」は、エディター領域に
            // もう 1 枚。ペインは `add_item_to_active_pane` が決める。
            BrowserViewEvent::OpenInNewTab(url) => {
                let url = url.clone();
                self.workspace
                    .update(cx, |workspace, cx| {
                        BrowserItem::open(workspace, Some(url), window, cx)
                    })
                    .log_err();
            }
            // エディター領域のタブは 1 枚 = 1 ビューなので、どのビューを触ったかを
            // 追う必要が無い (パネル側の左右分割だけが使う)。
            BrowserViewEvent::Focused => {}
        }
    }
}

impl EventEmitter<ItemEvent> for BrowserItem {}

impl Focusable for BrowserItem {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.read(cx).focus_handle(cx)
    }
}

impl Render for BrowserItem {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        // 描画された = このタブはペインのアクティブアイテムで、ペインも画面に
        // 出ている。ウォッチドッグはこの 2 つの旗だけを見る。
        self.painted_since_last_tick = true;
        self.surface_shown = true;
        div().size_full().child(self.view.clone())
    }
}

impl Item for BrowserItem {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.view.read(cx).tab_label()
    }

    fn tab_icon(&self, _: &Window, _: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ToolWeb))
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        let url = self.view.read(cx).current_url().trim().to_string();
        (!url.is_empty()).then(|| url.into())
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    /// パンくずリストも検索バーも持たないので、ツールバーごと消す。
    fn show_toolbar(&self) -> bool {
        false
    }

    fn deactivated(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.hide_surface(HIDDEN_DEACTIVATED, cx);
    }

    fn on_removed(&self, cx: &mut Context<Self>) {
        // `&self` なので旗は倒せないが、隠すのは冪等。コントローラー自体は
        // `BrowserView` (と `WebView`) が drop されるときに Close される。
        self.view
            .update(cx, |view, _| view.hide_surface_from_host(HIDDEN_REMOVED));
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_painted_tab_keeps_asking_for_frames() {
        assert_eq!(visibility_tick(true, true), VisibilityTick::KeepAlive);
        // 隠してあっても描画されたのなら、また見えている。
        assert_eq!(visibility_tick(true, false), VisibilityTick::KeepAlive);
    }

    #[test]
    fn a_tab_that_missed_a_whole_interval_is_hidden_once() {
        assert_eq!(visibility_tick(false, true), VisibilityTick::Hide);
        // 隠したあとは再描画を促さない (裏のタブが電力を食わないように)。
        assert_eq!(visibility_tick(false, false), VisibilityTick::Idle);
    }
}
