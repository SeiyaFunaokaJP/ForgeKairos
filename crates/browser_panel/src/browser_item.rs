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
//!
//! ただし push で**即座に**隠してよいのは「確実に画面から消えた」経路だけ
//! ([`Item::deactivated`]・タブを閉じた・裏に回した)。ズーム変化のように
//! 「隠れたかもしれない」だけの合図は 1 フレーム待って判定する ([`PendingHide`])。
//! GPUI 側の透過穴は前のフレームのまま流し直されることがあり (ドックもペインも
//! `.cached()`)、穴だけが残るとその矩形に机上が透けて一瞬暗くなる (ADR-F32 追記)。

use std::{any::Any, path::Path, sync::Arc, time::Duration};

use anyhow::anyhow;
use fs::Fs;
use gpui::{
    App, Context, Entity, EntityId, EventEmitter, ExternalPaths, FocusHandle, Focusable,
    SharedString, Subscription, Task, WeakEntity, Window,
};
use project::{Project, ProjectEntryId};
use settings::Settings as _;
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{
    ItemId, Pane, SerializableItem, Workspace, WorkspaceId,
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

/// ワークスペースのズーム状態のうち、このタブに影響する部分。
/// [`workspace::Event::ZoomChanged`] は「何がどう変わったか」を運ばないので、
/// 受け取るたびに取り直して前回と比べる。`None` = 何もズームしていない
/// (ズームされていたビューが既に無い場合も画面には出ないので同じ扱い)。
type ZoomState = Option<EntityId>;

fn workspace_zoom_state(workspace: &Entity<Workspace>, cx: &App) -> ZoomState {
    workspace
        .read(cx)
        .zoomed_item()
        .and_then(|view| view.upgrade())
        .map(|view| view.entity_id())
}

/// 「隠れたかもしれない」と言われてから、実際に隠すかを決めるまでの判定。
///
/// 隠すのを 1 フレーム遅らせるのが肝: 子 HWND を隠しても、GPUI 側の透過穴は
/// 直前のフレームのまま描かれ続けることがある (ドックもペインも `.cached()` なので
/// 前のフレームをそのまま流し直す)。穴だけが残るとその矩形はウィンドウの背後 =
/// 机上が透けて「一瞬暗くなる」。**穴が描かれているフレームでは隠さない**という
/// 不変条件をここで守る。
///
/// [`Context::on_next_frame`] のコールバックは**そのフレームを描く前**に走るので、
/// 「次のフレームで描かれたか」は 1 つ後のフレームでしか分からない。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PendingHide {
    /// まだ描画の機会が来ていない。もう 1 フレーム待つ。
    Wait,
    /// 描画された = まだ画面に出ている。隠さない。
    Cancel,
    /// 1 フレームまるごと描かれなかった = 画面から消えた。隠す。
    Hide,
}

pub(crate) fn pending_hide(painted_since_request: bool, frames_waited: u32) -> PendingHide {
    if painted_since_request {
        PendingHide::Cancel
    } else if frames_waited == 0 {
        PendingHide::Wait
    } else {
        PendingHide::Hide
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
    /// 直近に観測したズーム状態。`None` = まだ一度も観測していない
    /// (最初の 1 回は「変わったかもしれない」として扱う)。
    observed_zoom: Option<ZoomState>,
    /// 判定待ちの「隠す」要求とその理由。[`PendingHide`] を参照。
    hide_request: Option<&'static str>,
    /// その要求を出してから [`Render::render`] が呼ばれたか。
    painted_since_hide_request: bool,
    _visibility_watchdog: Task<()>,
    /// ズーム連動の購読。タブを別ウィンドウへ移すとワークスペースごと変わるので、
    /// [`Item::added_to_workspace`] で張り替える。
    _workspace_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

/// 新しいタブが最初に出すもの。
enum InitialPage {
    /// ユーザーが開いたタブ。`None` なら `browser.home_url`、それも空なら
    /// ブックマークを開いた空のタブ。
    Open(Option<String>),
    /// パネルが自分のために持っておくタブ。`browser.home_url` は持つだけで
    /// 開かない (ADR-F48。[`BrowserView::defer_home_page`])。
    DeferredHome,
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
        Self::with_initial_page(workspace, fs, host, InitialPage::Open(url), window, cx)
    }

    /// ブラウザーパネルが自分のために持っておくタブ。ホームページは読み込まず、
    /// ユーザーがブラウザーを名指しで出すまで空のまま待つ
    /// ([`BrowserView::defer_home_page`])。
    pub(crate) fn with_deferred_home_page(
        workspace: WeakEntity<Workspace>,
        fs: Arc<dyn Fs>,
        host: BrowserHost,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::with_initial_page(workspace, fs, host, InitialPage::DeferredHome, window, cx)
    }

    fn with_initial_page(
        workspace: WeakEntity<Workspace>,
        fs: Arc<dyn Fs>,
        host: BrowserHost,
        page: InitialPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let view = cx.new(|cx| BrowserView::new(workspace.clone(), fs, host, window, cx));

        view.update(cx, |view, cx| match page {
            InitialPage::DeferredHome => view.defer_home_page(cx),
            InitialPage::Open(url) => {
                let initial_url =
                    url.unwrap_or_else(|| BrowserSettings::get_global(cx).home_url.clone());
                if initial_url.trim().is_empty() {
                    view.open_bookmarks_pane(cx);
                } else {
                    view.set_initial_url(initial_url, window, cx);
                }
            }
        });

        let subscriptions = vec![cx.subscribe_in(&view, window, Self::on_view_event)];
        // ここで実体を読みに行かない (このワークスペースは更新中のことがある) ので、
        // ズーム状態の初期値は最初のイベントで埋める。
        let workspace_subscription = workspace
            .upgrade()
            .map(|workspace| Self::watch_workspace_zoom(&workspace, window, cx));

        Self {
            workspace,
            weak_self: cx.weak_entity(),
            view,
            painted_since_last_tick: false,
            surface_shown: false,
            observed_zoom: None,
            hide_request: None,
            painted_since_hide_request: false,
            _visibility_watchdog: Self::start_visibility_watchdog(cx),
            _workspace_subscription: workspace_subscription,
            _subscriptions: subscriptions,
        }
    }

    fn watch_workspace_zoom(
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(
            workspace,
            window,
            |this, workspace, event: &workspace::Event, window, cx| {
                if !matches!(event, workspace::Event::ZoomChanged) {
                    return;
                }
                // ペインにフォーカスが入るだけでもこのイベントは飛んでくる
                // (`Workspace::handle_pane_focused`)。ズームが動いていない
                // 巻き添えで隠すと、タブをクリックするたびにページが暗くなる。
                let zoom = workspace_zoom_state(workspace, cx);
                if this.observed_zoom == Some(zoom) {
                    return;
                }
                this.observed_zoom = Some(zoom);
                this.request_hide_if_not_painted(HIDDEN_ZOOM_CHANGED, window, cx);
            },
        )
    }

    /// ズームで隠れたかどうかは描画されるまで分からない。すぐ隠すと、GPUI 側の
    /// 透過穴だけが残ったフレームが出て一瞬暗くなるので、判定を次のフレームに
    /// 預ける ([`PendingHide`])。
    fn request_hide_if_not_painted(
        &mut self,
        reason: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.surface_shown || self.hide_request.is_some() {
            return;
        }
        // 遅らせる理由は「穴だけが残るフレームを作らない」こと。DirectComposition が
        // 無効なホストでは GPUI 側に穴が無く (子 HWND が常に手前)、遅らせるとズームで
        // 畳んだはずのペインの上にページが数フレーム残るだけなので、すぐ隠す。
        if !self.view.read(cx).host_is_composited() {
            self.hide_surface(reason, cx);
            return;
        }
        self.hide_request = Some(reason);
        self.painted_since_hide_request = false;
        // 判定に使うフレームを 1 枚起こす。描かれれば取り消し、描かれなければ隠す。
        cx.notify();
        self.schedule_hide_decision(0, window, cx);
    }

    fn schedule_hide_decision(
        &mut self,
        frames_waited: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // `Context::on_next_frame` はビューを**強参照**で捕まえるので、判定待ちの間
        // アイテム (と WebView2 のコントローラー) が生き延びてしまう。畳まれていく
        // ウィンドウでは次のフレームが来ないので、そのまま漏れる。
        let this = cx.weak_entity();
        window.on_next_frame(move |window, cx| {
            // 失敗する = タブが閉じられた。隠す相手ごと消えているので何もしない。
            this.update(cx, |this, cx| {
                this.decide_pending_hide(frames_waited, window, cx);
            })
            .ok();
        });
    }

    /// フレームの境目で 1 回だけ走る判定。`on_next_frame` から呼ぶ (テストは
    /// プラットフォームのフレーム要求を作れないので、ここを直接叩く)。
    fn decide_pending_hide(
        &mut self,
        frames_waited: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(reason) = self.hide_request else {
            return;
        };
        match pending_hide(self.painted_since_hide_request, frames_waited) {
            PendingHide::Wait => self.schedule_hide_decision(frames_waited + 1, window, cx),
            PendingHide::Cancel => self.hide_request = None,
            PendingHide::Hide => {
                self.hide_request = None;
                self.hide_surface(reason, cx);
            }
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
        // 判定待ちがあっても、もう隠したので用済み。
        self.hide_request = None;
        self.view
            .update(cx, |view, cx| view.hide_surface_from_host(reason, cx));
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
            // ここでそのペインを宛先にする (フォーカスがまだ外にあるなら移す)。
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
        self.painted_since_hide_request = true;
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
        window: &mut Window,
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
        self.observed_zoom = None;
        self.view
            .update(cx, |view, _| view.set_workspace(handle.clone()));
        self._workspace_subscription = handle
            .upgrade()
            .map(|workspace| Self::watch_workspace_zoom(&workspace, window, cx));
    }

    /// `Pane` がこれを呼ぶのは**自分のペインの**アクティブなアイテムが替わったときと、
    /// このアイテムを取り除くときだけ (`Pane::activate_item` / `Pane::_remove_item`)。
    /// 別のペインにフォーカスが移っただけでは呼ばれないので、ここは即座に隠してよい。
    fn deactivated(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.hide_surface(HIDDEN_DEACTIVATED, cx);
    }

    fn on_removed(&self, cx: &mut Context<Self>) {
        // `&self` なので旗は倒せないが、隠すのは冪等。コントローラー自体は
        // `BrowserView` (と `WebView`) が drop されるときに Close される。
        self.view.update(cx, |view, cx| {
            view.hide_surface_from_host(HIDDEN_REMOVED, cx)
        });
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

/// ブラウザーのタブも中央のタブと同じ流儀で復元できるようにする (ADR-F37)。
/// これが無いと、パネルの外 (エディター領域 / ターミナルパネル) に置いたページは
/// 再起動で消える。保存するのは URL 1 本だけで、履歴もセッションも持ち越さない。
impl SerializableItem for BrowserItem {
    fn serialized_item_kind() -> &'static str {
        "Browser"
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<()>> {
        let db = BrowserItemDb::global(cx);
        workspace::delete_unloaded_items(alive_items, workspace_id, "browser_items", &db, cx)
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        _closing: bool,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Task<anyhow::Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let url = self.view.read(cx).current_url().trim().to_string();
        if url.is_empty() {
            return None;
        }
        let db = BrowserItemDb::global(cx);
        Some(cx.background_spawn(async move { db.save_url(item_id, workspace_id, url).await }))
    }

    /// URL が変わるたびに `ItemEvent::UpdateTab` が飛ぶ (`BrowserView` の
    /// `LocationChanged`)。タブのラベル更新と同じ合図をそのまま使う。
    fn should_serialize(&self, event: &Self::Event) -> bool {
        matches!(event, ItemEvent::UpdateTab)
    }

    fn deserialize(
        _: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        let db = BrowserItemDb::global(cx);
        let url = db
            .get_url(item_id, workspace_id)
            .log_err()
            .flatten()
            .filter(|url| !url.trim().is_empty());
        let fs = workspace
            .upgrade()
            .map(|workspace| workspace.read(cx).app_state().fs.clone());
        let Some(fs) = fs else {
            return Task::ready(Err(anyhow!(
                "the workspace is gone, so the browser tab cannot be restored"
            )));
        };
        // ホストは `Item::added_to_workspace` が既定の「エディター領域のタブ」に
        // 直し、パネルに載ったならパネルが `AddItem` の直後に上書きする。
        Task::ready(Ok(cx.new(|cx| match url {
            Some(url) => {
                BrowserItem::new(workspace, fs, BrowserHost::EditorTab, Some(url), window, cx)
            }
            // 空のまま保存されたタブ (パネルが自分のために持っていた 1 枚) を
            // 復元しているので、ここでホームを開くと再起動のたびに、頼まれて
            // いない行き先へ出ていくことになる (ADR-F48)。
            None => BrowserItem::with_deferred_home_page(
                workspace,
                fs,
                BrowserHost::EditorTab,
                window,
                cx,
            ),
        })))
    }
}

pub(crate) use persistence::BrowserItemDb;

mod persistence {
    use db::{
        query,
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use workspace::{ItemId, WorkspaceDb, WorkspaceId};

    pub(crate) struct BrowserItemDb(ThreadSafeConnection);

    impl Domain for BrowserItemDb {
        const NAME: &str = stringify!(BrowserItemDb);

        const MIGRATIONS: &[&str] = &[sql!(
            CREATE TABLE browser_items (
                workspace_id INTEGER,
                item_id INTEGER,

                url TEXT,

                PRIMARY KEY(workspace_id, item_id),
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                ON DELETE CASCADE
            ) STRICT;
        )];
    }

    db::static_connection!(BrowserItemDb, [WorkspaceDb]);

    impl BrowserItemDb {
        query! {
            pub async fn save_url(item_id: ItemId, workspace_id: WorkspaceId, url: String) -> Result<()> {
                INSERT OR REPLACE INTO browser_items(item_id, workspace_id, url)
                VALUES (?, ?, ?)
            }
        }

        query! {
            pub fn get_url(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<String>> {
                SELECT url
                FROM browser_items
                WHERE item_id = ? AND workspace_id = ?
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use project::FakeFs;
    use settings::SettingsStore;
    use workspace::MultiWorkspace;

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

    #[test]
    fn a_hide_request_waits_one_frame_before_it_decides() {
        // `on_next_frame` はそのフレームを描く前に走るので、最初の 1 回では
        // まだ「描かれなかった」と言い切れない。
        assert_eq!(pending_hide(false, 0), PendingHide::Wait);
        assert_eq!(pending_hide(false, 1), PendingHide::Hide);
    }

    #[test]
    fn a_paint_cancels_a_hide_request_at_any_point() {
        assert_eq!(pending_hide(true, 0), PendingHide::Cancel);
        assert_eq!(pending_hide(true, 1), PendingHide::Cancel);
        assert_eq!(pending_hide(true, 7), PendingHide::Cancel);
    }

    // -- ウィンドウの要る経路 --------------------------------------------------

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
        });
    }

    async fn init_workspace(
        cx: &mut TestAppContext,
    ) -> (gpui::WindowHandle<MultiWorkspace>, Entity<Workspace>) {
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window_handle
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("failed to read the workspace out of its window");
        (window_handle, workspace)
    }

    /// 透過穴 (DirectComposition) のあるホストに載っているものとして扱う。判定を
    /// 次のフレームに預けるのはこの場合だけなので、その経路を通すテストで立てる。
    fn pretend_the_host_is_composited(item: &Entity<BrowserItem>, cx: &mut VisualTestContext) {
        item.update(cx, |item, cx| {
            item.view
                .update(cx, |view, _| view.set_host_is_composited_for_test(true));
        });
    }

    fn open_browser_tab(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Entity<BrowserItem> {
        workspace.update_in(cx, |workspace, window, cx| {
            BrowserItem::open(
                workspace,
                Some("https://example.com/".to_string()),
                window,
                cx,
            );
        });
        cx.run_until_parked();
        workspace
            .read_with(cx, |workspace, cx| {
                workspace
                    .active_pane()
                    .read(cx)
                    .items()
                    .find_map(|item| item.downcast::<BrowserItem>())
            })
            .expect("the browser tab was not added to the active pane")
    }

    #[gpui::test]
    async fn a_zoom_event_that_changes_nothing_is_ignored(cx: &mut TestAppContext) {
        init_test(cx);
        let (window_handle, workspace) = init_workspace(cx).await;
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);
        let item = open_browser_tab(&workspace, cx);
        pretend_the_host_is_composited(&item, cx);

        item.read_with(cx, |item, _| {
            assert!(item.surface_shown, "the new tab should have been painted");
        });

        // 初回は「まだ観測していない」ので、変わったかもしれない側に倒す。
        workspace.update(cx, |_, cx| cx.emit(workspace::Event::ZoomChanged));
        cx.run_until_parked();
        item.update(cx, |item, _| {
            assert_eq!(item.observed_zoom, Some(None));
            assert!(item.hide_request.is_some());
            item.hide_request = None;
        });

        // 2 回目はズームが動いていない。タブをクリックするたびに飛んでくるのが
        // これで、ここで隠すとページが一瞬暗くなる。
        workspace.update(cx, |_, cx| cx.emit(workspace::Event::ZoomChanged));
        cx.run_until_parked();
        item.read_with(cx, |item, _| {
            assert!(
                item.hide_request.is_none(),
                "a zoom event that changed nothing must not touch the surface"
            );
            assert!(item.surface_shown);
        });
    }

    #[gpui::test]
    async fn a_tab_that_is_still_painted_keeps_its_surface(cx: &mut TestAppContext) {
        init_test(cx);
        let (window_handle, workspace) = init_workspace(cx).await;
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);
        let item = open_browser_tab(&workspace, cx);
        pretend_the_host_is_composited(&item, cx);

        item.update_in(cx, |item, window, cx| {
            item.request_hide_if_not_painted("a test asked to hide", window, cx);
        });
        // 保留を出した側が再描画を促すので、判定のフレームまでに描かれている。
        cx.run_until_parked();
        item.update_in(cx, |item, window, cx| {
            assert!(
                item.painted_since_hide_request,
                "requesting a hide must ask for the frame that decides it"
            );
            item.decide_pending_hide(0, window, cx);
            assert!(item.hide_request.is_none());
            assert!(item.surface_shown, "the tab is still on screen");
        });
    }

    #[gpui::test]
    async fn a_tab_that_is_no_longer_painted_is_hidden_one_frame_later(cx: &mut TestAppContext) {
        init_test(cx);
        let (window_handle, _) = init_workspace(cx).await;
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        // どのペインにも入れないタブ = 描かれない。「見えているつもり」から
        // 始めて、判定用のフレームで描かれなければ隠れることを確かめる。
        let item = window_handle
            .update(cx, |multi_workspace, window, cx| {
                let workspace = multi_workspace.workspace().clone();
                let fs = workspace.read(cx).app_state().fs.clone();
                cx.new(|cx| {
                    BrowserItem::new(
                        workspace.downgrade(),
                        fs,
                        BrowserHost::EditorTab,
                        Some("https://example.com/".to_string()),
                        window,
                        cx,
                    )
                })
            })
            .expect("failed to build a detached browser tab");
        cx.run_until_parked();
        pretend_the_host_is_composited(&item, cx);

        item.update_in(cx, |item, window, cx| {
            item.surface_shown = true;
            item.request_hide_if_not_painted("a test asked to hide", window, cx);
        });
        cx.run_until_parked();
        item.update_in(cx, |item, window, cx| {
            assert!(!item.painted_since_hide_request);
            // 1 フレーム目はまだ判定できない (`on_next_frame` は描画の前)。
            item.decide_pending_hide(0, window, cx);
            assert!(item.hide_request.is_some());
            assert!(item.surface_shown);
            item.decide_pending_hide(1, window, cx);
            assert!(item.hide_request.is_none());
            assert!(!item.surface_shown, "the tab is off screen and must hide");
        });
    }

    #[gpui::test]
    async fn without_direct_composition_a_hide_takes_effect_at_once(cx: &mut TestAppContext) {
        init_test(cx);
        let (window_handle, workspace) = init_workspace(cx).await;
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);
        let item = open_browser_tab(&workspace, cx);

        // 穴が無いホストでは子 HWND が常に手前なので、遅らせると畳んだペインの
        // 上にページが残る。判定を待たずに隠す。
        item.update_in(cx, |item, window, cx| {
            assert!(!item.view.read(cx).host_is_composited());
            item.request_hide_if_not_painted("a test asked to hide", window, cx);
            assert!(item.hide_request.is_none(), "nothing is left pending");
            assert!(!item.surface_shown, "the surface is hidden right away");
        });
    }
}
