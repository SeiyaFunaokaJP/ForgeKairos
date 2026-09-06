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

use std::{any::Any, path::Path, sync::Arc, time::Duration};

use fs::Fs;
use gpui::{
    App, Context, Entity, EventEmitter, ExternalPaths, FocusHandle, Focusable, SharedString,
    Subscription, Task, WeakEntity, Window,
};
use project::ProjectEntryId;
use settings::Settings as _;
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{
    Pane, Workspace,
    item::{Item, ItemEvent},
};

use crate::{
    BrowserHost, BrowserPanel, BrowserSettings, BrowserView, BrowserViewEvent, ClosePanelTab,
    NewPanelTab, NewTabPlacement,
};

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

/// ブラウザーのタブとして開けるファイルの拡張子。落とされたパスが**すべて**これなら
/// エディターではなく `file://` のページとして開く。
const BROWSABLE_EXTENSIONS: &[&str] = &["html", "htm", "xhtml", "svg", "pdf"];

pub struct BrowserItem {
    workspace: WeakEntity<Workspace>,
    /// 自分自身への弱参照。`Item::handle_drop` は `&self` + `&mut App` しか
    /// 受け取れないので、ドロップの処理を次のフレームへ回すのに要る。
    weak_self: WeakEntity<Self>,
    view: Entity<BrowserView>,
    /// 直近の周期で [`Render::render`] が呼ばれたか。
    painted_since_last_tick: bool,
    /// 子 HWND を見せているつもりかどうか。隠してある間はウォッチドッグが
    /// 再描画を促さないので、裏に回ったタブは何のコストも掛からない。
    surface_shown: bool,
    _visibility_watchdog: Task<()>,
    /// ズーム連動の購読。タブを別ウィンドウへ移すとワークスペースごと変わるので、
    /// [`Item::added_to_workspace`] で張り替える。
    _workspace_subscription: Option<Subscription>,
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
        let workspace_handle = workspace.weak_handle();
        let fs = workspace.app_state().fs.clone();
        let item = cx.new(|cx| {
            BrowserItem::new(
                workspace_handle,
                fs,
                BrowserHost::EditorTab,
                url,
                window,
                cx,
            )
        });
        workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
    }

    /// `workspace` はハンドルで受ける。パネルからタブを開く経路では
    /// ワークスペース自身が更新中のことがあり、実体を読みに行くと二重借用になる。
    pub(crate) fn new(
        workspace: WeakEntity<Workspace>,
        fs: Arc<dyn Fs>,
        host: BrowserHost,
        url: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let view = cx.new(|cx| BrowserView::new(workspace.clone(), fs, host, window, cx));

        let initial_url = url.unwrap_or_else(|| BrowserSettings::get_global(cx).home_url.clone());
        view.update(cx, |view, cx| {
            if initial_url.trim().is_empty() {
                view.open_bookmarks_pane(cx);
            } else {
                view.set_initial_url(initial_url, window, cx);
            }
        });

        let subscriptions = vec![cx.subscribe_in(&view, window, Self::on_view_event)];
        let workspace_subscription = workspace
            .upgrade()
            .map(|workspace| Self::watch_workspace_zoom(&workspace, cx));

        Self {
            workspace,
            weak_self: cx.weak_entity(),
            view,
            painted_since_last_tick: false,
            surface_shown: false,
            _visibility_watchdog: Self::start_visibility_watchdog(cx),
            _workspace_subscription: workspace_subscription,
            _subscriptions: subscriptions,
        }
    }

    fn watch_workspace_zoom(workspace: &Entity<Workspace>, cx: &mut Context<Self>) -> Subscription {
        cx.subscribe(workspace, |this, _, event: &workspace::Event, cx| {
            if matches!(event, workspace::Event::ZoomChanged) {
                // ズームで隠れたかどうかは描画されるまで分からない。まず隠して、
                // まだ見えているなら次の描画で戻す。
                this.hide_surface(HIDDEN_ZOOM_CHANGED, cx);
            }
        })
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

    pub(crate) fn view(&self) -> &Entity<BrowserView> {
        &self.view
    }

    /// このタブがどこに載っているか。ツールバーの「エディター領域で開く」の
    /// 出し分けに使う。ドラッグでパネルと中央を行き来するので固定ではない。
    pub(crate) fn set_host(&mut self, host: BrowserHost, cx: &mut Context<Self>) {
        self.view.update(cx, |view, cx| view.set_host(host, cx));
    }

    pub(crate) fn hide_surface(&mut self, reason: &'static str, cx: &mut App) {
        self.surface_shown = false;
        self.view
            .update(cx, |view, _| view.hide_surface_from_host(reason));
    }

    fn open_sibling_tab(&mut self, _: &NewPanelTab, window: &mut Window, cx: &mut Context<Self>) {
        self.open_tab_beside_self(None, NewTabPlacement::Foreground, window, cx);
    }

    /// 「もう 1 枚」をこのタブの隣に開く。パネルに載っているならそのペイン、
    /// エディター領域ならアクティブなペイン。
    fn open_tab_beside_self(
        &mut self,
        url: Option<String>,
        placement: NewTabPlacement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let host = self.view.read(cx).host();
        let item_id = cx.entity_id();
        // ペインを触るのは次の機会に回す (このアイテムの更新中に、同じアイテムを
        // 抱えるペインを触りに行かないように)。
        cx.defer_in(window, move |this, window, cx| {
            this.workspace
                .update(cx, |workspace, cx| match host {
                    BrowserHost::Panel => {
                        if let Some(panel) = workspace.panel::<BrowserPanel>(cx) {
                            panel.update(cx, |panel, cx| {
                                panel.open_beside_item(item_id, url, placement, window, cx)
                            });
                        }
                    }
                    BrowserHost::EditorTab => BrowserItem::open(workspace, url, window, cx),
                })
                .log_err();
        });
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
            // 「新しいタブで開く」は同じタブ列の中に開く。パネルに載っているなら
            // そのペイン、エディター領域ならアクティブなペイン。
            BrowserViewEvent::OpenInNewTab(url) => {
                // ブラウザーと同じく背景タブ (今のページは見えたまま)。
                self.open_tab_beside_self(
                    Some(url.clone()),
                    NewTabPlacement::Background,
                    window,
                    cx,
                );
            }
            // ページの中のクリックは子 HWND が飲むので GPUI には届かない。分割した
            // パネルで「どちらのペインを触ったか」はこの申告でしか分からないので、
            // ここでそのペインへフォーカスを移す。
            BrowserViewEvent::Focused => {
                if self.view.read(cx).host() != BrowserHost::Panel {
                    return;
                }
                let item_id = cx.entity_id();
                cx.defer_in(window, move |this, window, cx| {
                    this.workspace
                        .update(cx, |workspace, cx| {
                            if let Some(panel) = workspace.panel::<BrowserPanel>(cx) {
                                panel.update(cx, |panel, cx| {
                                    panel.activate_pane_for_item(item_id, window, cx)
                                });
                            }
                        })
                        .log_err();
                });
            }
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 描画された = このタブはペインのアクティブアイテムで、ペインも画面に
        // 出ている。ウォッチドッグはこの 2 つの旗だけを見る。
        self.painted_since_last_tick = true;
        self.surface_shown = true;
        div()
            .size_full()
            // Ctrl+T / Ctrl+W はキーマップ上「BrowserPanel」文脈に割り当ててあり、
            // その文脈はエディター領域のタブでも立つ。パネル側のハンドラーだけに
            // 任せると中央のタブで空振りするので、載っている場所によらず
            // 「今見ているタブ」に効くようここで受ける。
            .on_action(cx.listener(|_, _: &ClosePanelTab, _, cx| {
                cx.emit(ItemEvent::CloseItem);
            }))
            .on_action(cx.listener(Self::open_sibling_tab))
            .child(self.view.clone())
    }
}

/// 落とされたパスをブラウザーで開く `file://` URL に直す。ブラウザーで開けない
/// 種類が 1 つでも混じっていたら `None` を返し、ドロップは既定の経路
/// (このペインにエディターとして開く) に任せる。
fn browsable_file_urls(paths: &[std::path::PathBuf]) -> Option<Vec<String>> {
    if paths.is_empty() {
        return None;
    }
    paths.iter().map(|path| browsable_file_url(path)).collect()
}

fn browsable_file_url(path: &Path) -> Option<String> {
    let extension = path.extension()?.to_str()?.to_lowercase();
    if !BROWSABLE_EXTENSIONS.contains(&extension.as_str()) {
        return None;
    }
    url::Url::from_file_path(path)
        .ok()
        .map(|url| url.to_string())
}

impl Item for BrowserItem {
    type Event = ItemEvent;

    /// ブラウザーのペインに落とされたファイルのうち、ブラウザーで開ける種類
    /// (HTML / SVG / PDF) だけを `file://` のタブとして横取りする。それ以外は
    /// 横取りせず、ペインに普通のエディタータブとして開かせる。
    fn handle_drop(
        &self,
        active_pane: &Pane,
        dropped: &dyn Any,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let urls = if let Some(paths) = dropped.downcast_ref::<ExternalPaths>() {
            browsable_file_urls(paths.paths())
        } else if let Some(entry_id) = dropped.downcast_ref::<ProjectEntryId>() {
            let workspace = self.workspace.upgrade();
            workspace
                .and_then(|workspace| {
                    let project = workspace.read(cx).project().read(cx);
                    let project_path = project.path_for_entry(*entry_id, cx)?;
                    project.absolute_path(&project_path, cx)
                })
                .and_then(|path| browsable_file_url(&path))
                .map(|url| vec![url])
        } else {
            None
        };
        let Some(urls) = urls else {
            return false;
        };

        let host = self.view.read(cx).host();
        let item_id = self.weak_self.entity_id();
        let workspace = self.workspace.clone();
        // 端に落とされていたらパネルを割ってから開く。エディター領域のタブでは
        // 分割先の `PaneGroup` を持っていないので、そのままアクティブなペインに開く。
        let split = active_pane.drag_split_direction();
        // ここはこのペインの更新中。タブを足すのは次のフレームに回す。
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| match host {
                    BrowserHost::Panel => {
                        if let Some(panel) = workspace.panel::<BrowserPanel>(cx) {
                            panel.update(cx, |panel, cx| {
                                panel.open_dropped_urls(item_id, urls, split, window, cx)
                            });
                        }
                    }
                    BrowserHost::EditorTab => {
                        for url in urls {
                            BrowserItem::open(workspace, Some(url), window, cx);
                        }
                    }
                })
                .log_err();
        });
        true
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.view.read(cx).tab_label()
    }

    fn tab_icon(&self, _: &Window, _: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ToolWeb))
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        let url = self.view.read(cx).current_url().trim().to_string();
        (!url.is_empty()).then(|| crate::tab_tooltip(&url))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    /// パンくずリストも検索バーも持たないので、ツールバーごと消す。
    fn show_toolbar(&self) -> bool {
        false
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 既定は中央のタブ。ブラウザーパネルに載ったときだけ、パネルが
        // `pane::Event::AddItem` を受けた直後に `BrowserHost::Panel` へ上書きする。
        self.set_host(BrowserHost::EditorTab, cx);

        // タブを別ウィンドウへ移すと持ち主のワークスペースごと変わる。旧ウィンドウを
        // 参照したままだと、モーダル判定もズーム連動も消えた窓を見に行ってしまう。
        let handle = workspace.weak_handle();
        if self.workspace.entity_id() == handle.entity_id() {
            return;
        }
        self.workspace = handle.clone();
        self.view
            .update(cx, |view, _| view.set_workspace(handle.clone()));
        self._workspace_subscription = handle
            .upgrade()
            .map(|workspace| Self::watch_workspace_zoom(&workspace, cx));
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
