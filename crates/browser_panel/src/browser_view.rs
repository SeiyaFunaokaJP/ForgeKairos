//! 内蔵ブラウザーの中身 (ADR-F19)。
//!
//! WebView2 のサーフェス・アドレスバー・ナビゲーション・ブックマークペインを
//! ひとまとめにしたビュー。右ドックの [`crate::BrowserPanel`] と、エディター領域の
//! タブ [`crate::BrowserItem`] の両方がこれを 1 つずつ持つので、ブラウザーは
//! 「同時に 1 つだけ」ではなくタブの数だけ開ける。
//!
//! 子 HWND は GPUI の合成面より手前に出るので、ポップオーバーやドロップダウンは
//! ページに隠れる。そのためブックマークの管理 UI はオーバーレイではなく、
//! ビュー内の左サイドペイン (開くとページ側が縮む) として作ってある。

use std::{collections::HashSet, rc::Rc, sync::Arc};

use editor::Editor;
use fs::Fs;
use futures::StreamExt as _;
use gpui::{
    AnyElement, App, ClickEvent, Context, DispatchPhase, Entity, EventEmitter, FocusHandle,
    Focusable, MouseButton, MouseDownEvent, SharedString, Subscription, Task, WeakEntity, Window,
    canvas, px, relative,
};
use settings::{Settings as _, SettingsStore};
use ui::{Tooltip, prelude::*};
use workspace::Workspace;

use crate::{
    AddBookmark, BookmarkNode, BookmarkPath, BookmarkRow, BookmarkStore, BookmarksFile,
    BrowserPanel, BrowserSettings, FocusAddressBar, GoHome, NavigateBack, NavigateForward,
    OpenInCenter, OpenInExternalBrowser, Reload, SetAsHomePage, SubmitAddress, ToggleBookmarks,
    normalize_bookmark_url, resolve_address_input,
};

#[cfg(windows)]
use crate::webview_windows::{OverlayRect, Viewport, WebView, WebViewEvent};

#[cfg(not(windows))]
use crate::webview_unsupported::{OverlayRect, Viewport, WebView, WebViewEvent};

const WEBVIEW2_DOWNLOAD_URL: &str = "https://developer.microsoft.com/microsoft-edge/webview2/";

/// ブックマークのサイドペインの幅。ツリーの行に「編集 / 上へ / 下へ / 削除」の
/// アイコンボタンが並ぶので、名前が潰れない程度には要る。
const BOOKMARKS_PANE_WIDTH: f32 = 244.;

/// タブに出すページタイトルの上限。長いタイトルでタブが伸び切らないようにする。
const MAX_TAB_TITLE_CHARS: usize = 40;

/// URL もタイトルも無いときのタブ名。
pub(crate) const UNTITLED_TAB_LABEL: &str = "ブラウザー";

enum SurfaceState {
    /// WebView2 の生成をまだ始めていない。
    Idle,
    /// 生成中。環境とコントローラーの作成は非同期。
    Starting,
    Ready,
    /// 生成できなかった (ランタイム未導入 / 非対応プラットフォーム)。
    Unavailable {
        message: SharedString,
        offer_runtime_download: bool,
    },
}

/// サイドペインに出す 1 つだけのインライン編集フォーム。
/// ポップオーバーはページに隠れるので、ダイアログではなくペインに埋める。
struct BookmarkForm {
    /// 編集中の既存ノードの位置。`None` なら新規追加。
    editing: Option<BookmarkPath>,
    is_folder: bool,
    /// 追加先 / 移動先のフォルダー。空なら最上位。
    parent: BookmarkPath,
    name: Entity<Editor>,
    url: Entity<Editor>,
}

/// ビューを載せている側 (ドックのパネル / エディターのタブ) の違い。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BrowserHost {
    Panel,
    EditorTab,
}

pub enum BrowserViewEvent {
    /// ページのタイトルまたは URL が変わった。タブのラベルを更新する。
    LocationChanged,
    /// この URL を新しいタブで開いてほしい (ブックマークの Ctrl+クリック / 中クリック)。
    /// どこにタブを作るかはホストが決める (パネルならパネルのタブバー、
    /// エディター領域のタブなら中央ペインにもう 1 枚)。
    OpenInNewTab(String),
    /// このビューが操作された。子 HWND の中のクリックは GPUI に届かないので、
    /// 左右に分割したパネルが「どちらのペインを触っているか」を知る唯一の手掛かり。
    Focused,
}

/// ブックマークバーの項目をドラッグしている間の持ち物。
#[derive(Clone)]
struct DraggedBookmark {
    path: BookmarkPath,
    name: SharedString,
}

impl Render for DraggedBookmark {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .px_1p5()
            .py_0p5()
            .rounded_sm()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().element_background)
            .child(Label::new(self.name.clone()).size(LabelSize::Small))
    }
}

/// ブックマークバーで、掴んだ項目を落とせる場所。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BookmarkDropTarget {
    /// 最上位の `usize` 番目の手前へ挿し込む。
    Before(usize),
    /// 最上位の `usize` 番目のフォルダーの中へ入れる。
    IntoFolder(usize),
}

pub struct BrowserView {
    workspace: WeakEntity<Workspace>,
    fs: Arc<dyn Fs>,
    host: BrowserHost,
    focus_handle: FocusHandle,
    address_editor: Entity<Editor>,
    webview: Option<Rc<WebView>>,
    /// サーフェスを作ったときのホストウィンドウ。タブを別ウィンドウへ移すと
    /// 子 HWND は元のウィンドウにぶら下がったままなので、変わったら作り直す。
    host_hwnd: Option<isize>,
    surface: SurfaceState,
    warned_missing_window_handle: bool,
    current_url: String,
    /// ホームページを「ユーザーがブラウザーに向くまで」待たせているタブの
    /// 行き先 ([`Self::defer_home_page`])。空文字は `browser.home_url` が空の
    /// ときで、待機が解ければブックマークのペインを開く (新しいタブと同じ)。
    pending_home_url: Option<String>,
    page_title: SharedString,
    status_message: Option<SharedString>,
    can_go_back: bool,
    can_go_forward: bool,
    loading: bool,
    page_has_focus: bool,
    applied_dark_mode: Option<bool>,
    /// The `(red, green, blue)` last handed to WebView2 as its default
    /// background; see [`Self::sync_appearance`].
    applied_background_color: Option<(u8, u8, u8)>,
    /// Whether gpui composites in front of the child window on this host, so
    /// that overlays and drag previews are visible without hiding the surface
    /// (ADR-F32). Read once per surface, from the host window's styles.
    host_is_composited: bool,
    bookmarks: Entity<BookmarkStore>,
    bookmarks_pane_open: bool,
    /// 畳んであるフォルダー。既定は開いた状態なので、木を作り直しても
    /// 「勝手に閉じている」状態にはならない。
    collapsed_folders: HashSet<BookmarkPath>,
    bookmark_form: Option<BookmarkForm>,
    /// 削除の確認待ち。行の中で確認するのでポップオーバーが要らない。
    pending_delete: Option<BookmarkPath>,
    event_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<BrowserViewEvent> for BrowserView {}

impl BrowserView {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        fs: Arc<dyn Fs>,
        host: BrowserHost,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let address_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("URL または検索語を入力", window, cx);
            editor
        });

        let bookmarks = BookmarkStore::global(fs.clone(), cx);

        let mut subscriptions = Vec::new();
        subscriptions.push(cx.observe_global::<SettingsStore>(|this, cx| {
            this.sync_appearance(cx);
            cx.notify();
        }));
        subscriptions.push(cx.observe(&bookmarks, |_, _, cx| cx.notify()));

        Self {
            workspace,
            fs,
            host,
            focus_handle: cx.focus_handle(),
            address_editor,
            webview: None,
            host_hwnd: None,
            surface: SurfaceState::Idle,
            warned_missing_window_handle: false,
            current_url: String::new(),
            pending_home_url: None,
            page_title: SharedString::default(),
            status_message: None,
            can_go_back: false,
            can_go_forward: false,
            loading: false,
            page_has_focus: false,
            applied_dark_mode: None,
            applied_background_color: None,
            host_is_composited: false,
            bookmarks,
            bookmarks_pane_open: false,
            collapsed_folders: HashSet::default(),
            bookmark_form: None,
            pending_delete: None,
            event_task: None,
            _subscriptions: subscriptions,
        }
    }

    fn settings(cx: &App) -> &BrowserSettings {
        BrowserSettings::get_global(cx)
    }

    // -- ホストから使う API --------------------------------------------------

    pub fn host(&self) -> BrowserHost {
        self.host
    }

    /// ドラッグでパネルと中央のペインを行き来するので、載っている場所は
    /// 生成時に決め打てない。持ち主 ([`crate::BrowserItem`]) が知らせてくる。
    pub(crate) fn set_host(&mut self, host: BrowserHost, cx: &mut Context<Self>) {
        if self.host == host {
            return;
        }
        self.host = host;
        cx.notify();
    }

    /// タブを別ウィンドウへ移したときに持ち主が張り替える。モーダル判定
    /// ([`Self::surface_presentation`]) が消えたウィンドウを見に行かないように。
    pub(crate) fn set_workspace(&mut self, workspace: WeakEntity<Workspace>) {
        self.workspace = workspace;
    }

    pub fn current_url(&self) -> &str {
        &self.current_url
    }

    pub fn page_title(&self) -> &SharedString {
        &self.page_title
    }

    /// タブに出す名前。ページタイトル → URL → 既定名の順に落とす。
    pub fn tab_label(&self) -> SharedString {
        tab_label(&self.page_title, &self.current_url)
    }

    pub fn set_status_message(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.status_message = Some(message.into());
        cx.notify();
    }

    /// ホスト側の都合で子 HWND を隠す。
    ///
    /// 子 HWND は GPUI の描画に従わないので、**隠すのはホストからの push、
    /// 見せるのは描画からの pull** と向きを分けてある。次に [`Render::render`]
    /// が呼ばれると canvas が矩形を流し直して自動で表示に戻るので、ここで
    /// 「見せる」側の API は要らない。
    ///
    /// [`Context::notify`] が要る: ドックやペインは `.cached()` なので、
    /// 通知しないと次のフレームは**前のフレームの描画をそのまま流し直す** =
    /// 透過穴だけが描かれてサーフェスは隠れたまま、という組み合わせになり、
    /// 穴の下にデスクトップが覗く。通知しておけば次のフレームで必ず
    /// `render` が走り、まだ画面に出ているなら同じフレームで表示に戻る。
    pub fn hide_surface_from_host(&mut self, reason: &'static str, cx: &mut Context<Self>) {
        if let Some(webview) = &self.webview {
            webview.hide(reason);
        }
        cx.notify();
    }

    pub fn open_bookmarks_pane(&mut self, cx: &mut Context<Self>) {
        self.bookmarks_pane_open = true;
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn bookmarks_pane_is_open(&self) -> bool {
        self.bookmarks_pane_open
    }

    /// ホストが決めた最初の URL。サーフェスはまだ無いので [`Self::navigate_to`] は
    /// 使えない (WebView2 が無いときは外部ブラウザーに落ちる仕様のため)。
    /// [`Self::ensure_surface`] がコントローラーを作るときにこの URL へ飛ばす。
    pub fn set_initial_url(&mut self, url: String, window: &mut Window, cx: &mut Context<Self>) {
        if url.trim().is_empty() {
            return;
        }
        self.pending_home_url = None;
        self.current_url = url;
        self.loading = true;
        self.sync_address_editor(window, cx);
        cx.emit(BrowserViewEvent::LocationChanged);
        cx.notify();
    }

    /// ホームページを持ったまま、まだ開かない空のタブにする。
    ///
    /// ブラウザーパネルが自分のために持っておくタブは、ユーザーが「ブラウザーを
    /// 出せ」と言わなくても画面に出る: 右ドックはエージェントパネルとブラウザー
    /// パネルの 2 枚並べが既定なので (ADR-F30)、エージェントを開くだけで相方の
    /// ブラウザーも表示される。そこで既定のホーム (Microsoft 365 Copilot) へ
    /// 出ていくのは「AI とブラウザーは既定で起動時に開かない」(ADR-F48) の趣旨に
    /// 反するので、行き先は持っておくだけにして、[`Self::load_deferred_home_page`]
    /// = ユーザーがブラウザーを名指しで出したときに初めて読み込む。
    pub(crate) fn defer_home_page(&mut self, cx: &mut Context<Self>) {
        // 空の `browser.home_url` も待機させる: 「新しいタブ」はそのとき
        // ブックマークのペインを開くので、待機を解いたときに同じものを出す
        // (先に開いてしまうと、頼まれていないのにブックマークが出る)。
        self.pending_home_url = Some(Self::settings(cx).home_url.trim().to_string());
    }

    /// 待たせておいたホームページを開く。ユーザーがこのビューの載っている
    /// ブラウザーパネルを名指しで出したときに、パネルから呼ばれる。
    pub(crate) fn load_deferred_home_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(url) = self.pending_home_url.take() else {
            return;
        };
        if url.is_empty() {
            self.open_bookmarks_pane(cx);
        } else if self.webview.is_some() {
            self.navigate_to(&url, cx);
        } else {
            // サーフェスがまだ無いうちに `navigate_to` を呼ぶと OS の既定の
            // ブラウザーへ落ちてしまうので、生成時に飛ばす方へ渡す。
            self.set_initial_url(url, window, cx);
        }
    }

    // -- WebView2 のライフサイクル ------------------------------------------

    fn ensure_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let host_hwnd = host_window_handle(window);
        if let (Some(current), Some(previous)) = (host_hwnd, self.host_hwnd)
            && current != previous
        {
            // タブが別ウィンドウへ移った。子 HWND は元のウィンドウの下にいるので
            // 作り直す。`event_task` をここで落とせるのは、この関数が描画から
            // しか呼ばれない (= タスクの中ではない) ため。
            log::info!("browser view: the host window changed; recreating the browsing surface");
            self.webview = None;
            self.event_task = None;
            self.surface = SurfaceState::Idle;
            self.host_hwnd = None;
            self.applied_dark_mode = None;
            self.applied_background_color = None;
        }

        if self.webview.is_some() || !matches!(self.surface, SurfaceState::Idle) {
            return;
        }

        let Some(host_hwnd) = host_hwnd else {
            // ウィンドウハンドルはウィンドウが出来上がっていれば必ず取れる。
            // 取れないうちは `Idle` のままにして次の描画で作り直すが、
            // `render` から毎フレーム来るのでログは 1 回だけ。
            if !self.warned_missing_window_handle {
                self.warned_missing_window_handle = true;
                log::warn!(
                    "browser view: the host window handle is not available yet; \
                     retrying on the next frame"
                );
            }
            return;
        };
        self.host_is_composited = host_is_composited(host_hwnd);

        let settings = Self::settings(cx);
        let proxy_endpoint = settings
            .use_egress_proxy
            .then(http_client::child_process_proxy)
            .flatten()
            .map(|endpoint| endpoint.as_str().trim_end_matches('/').to_string());
        if settings.use_egress_proxy && proxy_endpoint.is_none() {
            log::warn!(
                "browser.use_egress_proxy is on but no egress proxy is running; \
                 the built-in browser will connect directly"
            );
        }

        let user_data_folder = paths::data_dir().join("webview2");
        let (events_tx, mut events_rx) = futures::channel::mpsc::unbounded();
        match WebView::new(
            host_hwnd,
            &user_data_folder,
            proxy_endpoint.as_deref(),
            events_tx,
        ) {
            Ok(webview) => {
                let webview = Rc::new(webview);
                // 作り直し (別ウィンドウへの移動) では既に開いていたページを
                // 引き継ぐ。コントローラーが出来るまでキューに積まれる。
                if !self.current_url.is_empty() {
                    webview.navigate(&self.current_url);
                }
                self.webview = Some(webview);
                self.host_hwnd = Some(host_hwnd);
                self.surface = SurfaceState::Starting;
                self.event_task = Some(cx.spawn_in(window, async move |this, cx| {
                    while let Some(event) = events_rx.next().await {
                        if this
                            .update_in(cx, |this, window, cx| {
                                this.handle_webview_event(event, window, cx)
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }));
            }
            Err(error) => {
                log::error!("failed to start WebView2: {error:#}");
                self.surface = SurfaceState::Unavailable {
                    message: runtime_missing_message(),
                    offer_runtime_download: cfg!(windows),
                };
            }
        }
        cx.notify();
    }

    fn handle_webview_event(
        &mut self,
        event: WebViewEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            WebViewEvent::Ready => {
                log::info!("browser view: the browsing surface is ready");
                self.surface = SurfaceState::Ready;
                self.applied_dark_mode = None;
                self.applied_background_color = None;
                self.sync_appearance(cx);
                // 待たせてあるホームがあるタブは空のまま置く。ここで開いてしまうと
                // サーフェスが出来た瞬間に、頼まれていない行き先へ出ていく。
                if self.current_url.is_empty() && self.pending_home_url.is_none() {
                    let home = Self::settings(cx).home_url.clone();
                    self.navigate_to(&home, cx);
                }
            }
            WebViewEvent::Failed(detail) => {
                log::error!("WebView2 unavailable: {detail}");
                // Dropping the surface drops the event sender, which ends the
                // stream this handler is running inside; the task must not be
                // dropped from within itself.
                self.webview = None;
                self.surface = SurfaceState::Unavailable {
                    message: runtime_missing_message(),
                    offer_runtime_download: true,
                };
            }
            WebViewEvent::UrlChanged(url) => {
                if url != self.current_url {
                    self.current_url = url;
                    self.sync_address_editor(window, cx);
                    cx.emit(BrowserViewEvent::LocationChanged);
                }
            }
            WebViewEvent::TitleChanged(title) => {
                self.page_title = title.into();
                cx.emit(BrowserViewEvent::LocationChanged);
            }
            WebViewEvent::HistoryChanged {
                can_go_back,
                can_go_forward,
            } => {
                self.can_go_back = can_go_back;
                self.can_go_forward = can_go_forward;
            }
            WebViewEvent::LoadFinished { succeeded } => {
                self.loading = false;
                self.status_message =
                    (!succeeded).then(|| SharedString::from("ページを読み込めませんでした。"));
            }
            WebViewEvent::FocusReturned => {
                self.page_has_focus = false;
                window.focus(&self.focus_handle, cx);
            }
            WebViewEvent::PageFocusChanged(focused) => {
                self.page_has_focus = focused;
                if focused {
                    cx.emit(BrowserViewEvent::Focused);
                }
            }
            WebViewEvent::OpenInPlace(url) => self.navigate_to(&url, cx),
        }
        // 生成完了は非同期に届くので、ここで再描画しないと canvas が走らず、
        // コントローラーは 0x0 のまま (= 何も表示されない) になる。
        cx.notify();
    }

    fn sync_appearance(&mut self, cx: &mut Context<Self>) {
        let Some(webview) = self.webview.clone() else {
            return;
        };
        let dark = !cx.theme().appearance().is_light();
        if self.applied_dark_mode != Some(dark) {
            webview.set_dark_mode(dark);
            self.applied_dark_mode = Some(dark);
        }
        // WebView2 が「ページがまだ描いていない部分」に塗る色。既定は白なので、
        // 暗いテーマでは最初の表示や再表示のたびに白が一瞬出る。エディターの
        // 背景に合わせておけば、出ても地の色と見分けが付かない (ADR-F32)。
        let background = surface_background_color(cx);
        if self.applied_background_color != Some(background) {
            apply_surface_background(&webview, background);
            self.applied_background_color = Some(background);
        }
    }

    fn sync_address_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.address_editor.focus_handle(cx).is_focused(window) {
            return;
        }
        let url = self.current_url.clone();
        self.address_editor
            .update(cx, |editor, cx| editor.set_text(url, window, cx));
    }

    // -- ナビゲーション ------------------------------------------------------

    pub fn navigate_to(&mut self, url: &str, cx: &mut Context<Self>) {
        if url.trim().is_empty() {
            return;
        }
        // ユーザーが行き先を選んだ = 待たせてあるホームは用済み (ブックマークを
        // 押した直後にホームが追い掛けてこないように)。
        self.pending_home_url = None;
        self.current_url = url.to_string();
        self.loading = true;
        self.status_message = None;
        match &self.webview {
            Some(webview) => webview.navigate(url),
            None => cx.open_url(url),
        }
        cx.emit(BrowserViewEvent::LocationChanged);
        cx.notify();
    }

    fn submit_address(&mut self, _: &SubmitAddress, window: &mut Window, cx: &mut Context<Self>) {
        // ブックマークのフォームもこのビュー直下の単一行エディターなので、同じ
        // Enter がここに届く。フォームに入力中ならそちらを確定する。
        if self.bookmark_form.as_ref().is_some_and(|form| {
            form.name.focus_handle(cx).is_focused(window)
                || form.url.focus_handle(cx).is_focused(window)
        }) {
            self.submit_bookmark_form(window, cx);
            return;
        }

        let input = self.address_editor.read(cx).text(cx);
        let search_url = Self::settings(cx).search_url.clone();
        let Some(url) = resolve_address_input(&input, &search_url) else {
            return;
        };
        self.navigate_to(&url, cx);
        window.focus(&self.focus_handle, cx);
        if let Some(webview) = &self.webview {
            webview.focus_page();
        }
    }

    fn navigate_back(&mut self, _: &NavigateBack, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(webview) = &self.webview {
            webview.go_back();
        }
        cx.notify();
    }

    fn navigate_forward(&mut self, _: &NavigateForward, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(webview) = &self.webview {
            webview.go_forward();
        }
        cx.notify();
    }

    fn reload(&mut self, _: &Reload, _: &mut Window, cx: &mut Context<Self>) {
        match &self.webview {
            Some(webview) => {
                self.loading = true;
                webview.reload();
            }
            None => {
                let url = self.current_url.clone();
                self.navigate_to(&url, cx);
            }
        }
        cx.notify();
    }

    fn go_home(&mut self, _: &GoHome, _: &mut Window, cx: &mut Context<Self>) {
        let home = Self::settings(cx).home_url.clone();
        self.navigate_to(&home, cx);
    }

    fn focus_address_bar(
        &mut self,
        _: &FocusAddressBar,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(webview) = &self.webview {
            webview.release_keyboard_focus();
        }
        self.address_editor.update(cx, |editor, cx| {
            editor.select_all(&editor::actions::SelectAll, window, cx);
        });
        window.focus(&self.address_editor.focus_handle(cx), cx);
    }

    fn open_in_external_browser(
        &mut self,
        _: &OpenInExternalBrowser,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let url = if self.current_url.trim().is_empty() {
            Self::settings(cx).home_url.clone()
        } else {
            self.current_url.clone()
        };
        cx.open_url(&url);
    }

    /// 今開いているページを `browser.home_url` に書き込む。設定 GUI を開かなくても
    /// 「起動時に開くページ」を差し替えられるようにするための導線。
    fn set_as_home_page(&mut self, _: &SetAsHomePage, _: &mut Window, cx: &mut Context<Self>) {
        let url = self.current_url.trim().to_string();
        if url.is_empty() {
            self.set_status_message("先にページを開いてください。", cx);
            return;
        }
        settings::update_settings_file(self.fs.clone(), cx, {
            let url = url.clone();
            move |settings, _| {
                settings.browser.get_or_insert_default().home_url = Some(url);
            }
        });
        self.set_status_message(format!("起動時に開くページを {url} にしました。"), cx);
    }

    /// ホストに「新しいタブでこの URL を開いてほしい」と伝える。
    fn open_in_new_tab(&mut self, url: &str, cx: &mut Context<Self>) {
        if url.trim().is_empty() {
            return;
        }
        cx.emit(BrowserViewEvent::OpenInNewTab(url.to_string()));
    }

    /// ブックマークのクリック。Ctrl (macOS では Cmd) 併用と中クリックは新しいタブ。
    fn activate_bookmark_url(&mut self, url: &str, event: &ClickEvent, cx: &mut Context<Self>) {
        if event.modifiers().secondary() || event.is_middle_click() {
            self.open_in_new_tab(url, cx);
        } else {
            self.navigate_to(url, cx);
        }
    }

    // -- ブックマーク --------------------------------------------------------

    fn bookmarks_file(&self, cx: &App) -> BookmarksFile {
        self.bookmarks.read(cx).file().clone()
    }

    /// 木を書き換えてファイルに書き戻す。`edit` が false を返したら何もしない。
    fn update_bookmarks(
        &mut self,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut BookmarksFile) -> bool,
    ) {
        if !self.bookmarks.read(cx).is_loaded() {
            self.status_message = Some(
                "ブックマークを読み込んでいます。少し待ってからもう一度お試しください。".into(),
            );
            cx.notify();
            return;
        }
        let mut file = self.bookmarks_file(cx);
        if !edit(&mut file) {
            return;
        }
        // 位置がずれた畳み状態は捨てる (フォルダーで無くなった位置は必ず捨てる)。
        self.collapsed_folders
            .retain(|path| file.node(path).is_some_and(BookmarkNode::is_folder));
        self.pending_delete = None;

        let write = self
            .bookmarks
            .update(cx, |bookmarks, cx| bookmarks.write(file, cx));
        cx.spawn(async move |this, cx| {
            if let Err(error) = write.await {
                log::error!("failed to write browser_bookmarks.json: {error:#}");
                this.update(cx, |this, cx| {
                    this.status_message = Some("ブックマークを保存できませんでした。".into());
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
        cx.notify();
    }

    fn toggle_bookmarks(&mut self, _: &ToggleBookmarks, _: &mut Window, cx: &mut Context<Self>) {
        self.bookmarks_pane_open = !self.bookmarks_pane_open;
        if !self.bookmarks_pane_open {
            self.bookmark_form = None;
            self.pending_delete = None;
        }
        cx.notify();
    }

    fn add_bookmark(&mut self, _: &AddBookmark, window: &mut Window, cx: &mut Context<Self>) {
        let name = if self.page_title.is_empty() {
            self.current_url.clone()
        } else {
            self.page_title.to_string()
        };
        let url = self.current_url.clone();
        self.open_bookmark_form(None, false, Vec::new(), name, url, window, cx);
    }

    fn open_bookmark_form(
        &mut self,
        editing: Option<BookmarkPath>,
        is_folder: bool,
        parent: BookmarkPath,
        name: String,
        url: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("名前", window, cx);
            editor.set_text(name, window, cx);
            editor
        });
        let url_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("https://example.com/", window, cx);
            editor.set_text(url, window, cx);
            editor
        });

        if let Some(webview) = &self.webview {
            // ページがキーボードを握ったままだと入力欄に文字が入らない。
            webview.release_keyboard_focus();
        }
        window.focus(&name_editor.focus_handle(cx), cx);

        self.bookmarks_pane_open = true;
        self.pending_delete = None;
        self.status_message = None;
        self.bookmark_form = Some(BookmarkForm {
            editing,
            is_folder,
            parent,
            name: name_editor,
            url: url_editor,
        });
        cx.notify();
    }

    fn submit_bookmark_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.bookmark_form.take() else {
            return;
        };
        let name = form.name.read(cx).text(cx).trim().to_string();
        let url = normalize_bookmark_url(&form.url.read(cx).text(cx));
        let is_folder = form.is_folder;

        if name.is_empty() || (!is_folder && url.is_empty()) {
            self.status_message = Some(if name.is_empty() {
                "名前を入力してください。".into()
            } else {
                "URL を入力してください。".into()
            });
            self.bookmark_form = Some(form);
            cx.notify();
            return;
        }

        let parent = form.parent.clone();
        match form.editing {
            Some(path) => self.update_bookmarks(cx, move |file| {
                if !file.rename(&path, name, (!is_folder).then_some(url.as_str())) {
                    return false;
                }
                // 移動先が自分自身の中などで拒否されたら、名前だけ変えて据え置く。
                if file.move_to_folder(&path, &parent).is_none() {
                    log::info!("browser view: the bookmark could not be moved into {parent:?}");
                }
                true
            }),
            None => self.update_bookmarks(cx, move |file| {
                let node = if is_folder {
                    BookmarkNode::folder(name)
                } else {
                    BookmarkNode::bookmark(name, url)
                };
                file.insert(&parent, node).is_some()
            }),
        }
        window.focus(&self.focus_handle, cx);
    }

    fn edit_bookmark(&mut self, path: BookmarkPath, window: &mut Window, cx: &mut Context<Self>) {
        let file = self.bookmarks_file(cx);
        let Some(node) = file.node(&path) else {
            return;
        };
        let is_folder = node.is_folder();
        let name = node.name().to_string();
        let url = node.url().unwrap_or_default().to_string();
        let parent = path
            .split_last()
            .map(|(_, parent)| parent.to_vec())
            .unwrap_or_default();
        self.open_bookmark_form(Some(path), is_folder, parent, name, url, window, cx);
    }

    fn set_folder_expanded(&mut self, path: &[usize], expanded: bool, cx: &mut Context<Self>) {
        if expanded {
            self.collapsed_folders.remove(path);
        } else {
            self.collapsed_folders.insert(path.to_vec());
        }
        cx.notify();
    }

    // -- 表示 ----------------------------------------------------------------

    /// GPUI がこのホストで子 HWND の**手前**に合成しているか (= 透過穴で見せている)。
    /// 穴が無いホストでは子 HWND が常に手前なので、隠す判断を遅らせる意味が無い。
    pub(crate) fn host_is_composited(&self) -> bool {
        self.host_is_composited
    }

    /// テストの窓には HWND が無く、実機なら面を作るときに読む旗が立たない。
    #[cfg(test)]
    pub(crate) fn set_host_is_composited_for_test(&mut self, composited: bool) {
        self.host_is_composited = composited;
    }

    /// 子 HWND は自分の矩形でマウス入力を受け取り続けるので、GPUI 側が
    /// その上に何かを出している間は隠すか、入力を止める (ADR-F19 / ADR-F48)。
    /// 判定そのものは [`surface_presentation_for`]。
    ///
    /// **このメソッドが呼ばれている = ビューが描画されている** ので、ホストは
    /// このビューを表示している。はっきり検出できた条件でだけ隠す。
    fn surface_presentation(&self, window: &mut Window, cx: &mut App) -> SurfacePresentation {
        let modal_is_open = self.workspace.upgrade().is_some_and(|workspace| {
            workspace.update(cx, |workspace, cx| workspace.has_active_modal(window, cx))
        });
        surface_presentation_for(cx.has_active_drag(), self.host_is_composited, modal_is_open)
    }

    fn render_toolbar(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let focus_handle = self.focus_handle.clone();
        let address_focused = self
            .address_editor
            .focus_handle(cx)
            .contains_focused(window, cx);
        h_flex()
            .w_full()
            // 下ドックの低いパネルでは縦の余りが負になる。既定の flex-shrink の
            // ままだと、伸縮しない内容領域 (flex-basis 0) の代わりにこの行が潰れ、
            // アドレス欄がボタンの下敷きになる。削るのは内容領域だけに限る
            // (それでも足りなければドック側の overflow で切られる)。
            .flex_shrink_0()
            .gap_1()
            .p_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                IconButton::new("browser-back", IconName::ArrowLeft)
                    .icon_size(IconSize::Small)
                    .disabled(!self.can_go_back)
                    .tooltip({
                        let focus_handle = focus_handle.clone();
                        move |_, cx| {
                            Tooltip::for_action_in("戻る", &NavigateBack, &focus_handle, cx)
                        }
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.navigate_back(&NavigateBack, window, cx)
                    })),
            )
            .child(
                IconButton::new("browser-forward", IconName::ArrowRight)
                    .icon_size(IconSize::Small)
                    .disabled(!self.can_go_forward)
                    .tooltip({
                        let focus_handle = focus_handle.clone();
                        move |_, cx| {
                            Tooltip::for_action_in("進む", &NavigateForward, &focus_handle, cx)
                        }
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.navigate_forward(&NavigateForward, window, cx)
                    })),
            )
            .child(
                IconButton::new("browser-reload", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .tooltip({
                        let focus_handle = focus_handle.clone();
                        move |_, cx| {
                            Tooltip::for_action_in("再読み込み", &Reload, &focus_handle, cx)
                        }
                    })
                    .on_click(cx.listener(|this, _, window, cx| this.reload(&Reload, window, cx))),
            )
            .child(
                ui::input_frame_sized(ui::InputFrameSize::Compact, address_focused, cx)
                    .flex_1()
                    .child(self.address_editor.clone()),
            )
            .child(
                IconButton::new("browser-add-bookmark", IconName::Star)
                    .icon_size(IconSize::Small)
                    .tooltip({
                        let focus_handle = focus_handle.clone();
                        move |_, cx| {
                            Tooltip::for_action_in(
                                "このページをブックマークに追加",
                                &AddBookmark,
                                &focus_handle,
                                cx,
                            )
                        }
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.add_bookmark(&AddBookmark, window, cx)
                    })),
            )
            .child(
                IconButton::new("browser-set-home", IconName::Pin)
                    .icon_size(IconSize::Small)
                    .tooltip({
                        let focus_handle = focus_handle.clone();
                        move |_, cx| {
                            Tooltip::for_action_in(
                                "このページを起動時に開く",
                                &SetAsHomePage,
                                &focus_handle,
                                cx,
                            )
                        }
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.set_as_home_page(&SetAsHomePage, window, cx)
                    })),
            )
            .child(
                IconButton::new("browser-bookmarks", IconName::Bookmark)
                    .icon_size(IconSize::Small)
                    .toggle_state(self.bookmarks_pane_open)
                    .tooltip({
                        let focus_handle = focus_handle.clone();
                        move |_, cx| {
                            Tooltip::for_action_in(
                                "ブックマーク",
                                &ToggleBookmarks,
                                &focus_handle,
                                cx,
                            )
                        }
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_bookmarks(&ToggleBookmarks, window, cx)
                    })),
            )
            // パネル版だけの導線。エディター領域のタブから更にタブを開く導線は
            // 「新しいブラウザータブ」があるので要らない。
            .when(self.host == BrowserHost::Panel, |this| {
                let focus_handle = focus_handle.clone();
                this.child(
                    IconButton::new("browser-open-in-center", IconName::Maximize)
                        .icon_size(IconSize::Small)
                        .tooltip(move |_, cx| {
                            Tooltip::for_action_in(
                                "エディター領域へ移す",
                                &OpenInCenter,
                                &focus_handle,
                                cx,
                            )
                        })
                        // ワークスペース側のハンドラーに任せる (パネルの URL を
                        // 読んで中央ペインにタブを開くのはワークスペースの仕事)。
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(OpenInCenter), cx)
                        }),
                )
            })
            .child(
                IconButton::new("browser-external", IconName::ArrowUpRight)
                    .icon_size(IconSize::Small)
                    .tooltip(move |_, cx| {
                        Tooltip::for_action_in(
                            "外部ブラウザーで開く",
                            &OpenInExternalBrowser,
                            &focus_handle,
                            cx,
                        )
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_in_external_browser(&OpenInExternalBrowser, window, cx)
                    })),
            )
            .into_any_element()
    }

    /// このビューがブックマークバーを出すか (ADR-F33 追記 2)。
    ///
    /// バーは Chrome と同じ並び (タブバー → アドレス欄 → ブックマークバー → ページ)
    /// になるようにビューの中に置く。「パネルに 1 本だけ」は**アクティブなペインの
    /// アクティブなビューだけが出す**という規則で保つので、パネルからの push は
    /// 要らない (押し忘れた状態が残らない)。エディター領域やターミナルパネルに
    /// 置いたタブは、それぞれが自分のバーを出す。
    pub(crate) fn shows_bookmark_bar(&self, cx: &Context<Self>) -> bool {
        match self.host {
            BrowserHost::EditorTab => true,
            BrowserHost::Panel => self
                .workspace
                .upgrade()
                .and_then(|workspace| workspace.read(cx).panel::<BrowserPanel>(cx))
                .is_some_and(|panel| panel.read(cx).is_active_view(cx.entity_id(), cx)),
        }
    }

    /// ブックマークバー。最上位の項目だけを並べ、フォルダーはサイドペインを
    /// そのフォルダーで開くボタンにする (メニューはページに隠れるため)。
    /// 並べ替えはドラッグ＆ドロップ (落とし先はドラッグ中だけ上に敷く)。
    fn render_bookmarks_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let items = self.bookmarks_file(cx).items;
        if items.is_empty() {
            return None;
        }
        let dragging = cx.has_active_drag();
        Some(
            h_flex()
                .w_full()
                // ツールバーと同じ理由 (低いパネルでも潰さない)。
                .flex_shrink_0()
                .flex_wrap()
                .gap_1()
                .px_1()
                .pb_1()
                .children(items.into_iter().enumerate().map(|(index, node)| {
                    let name = SharedString::from(node.name().to_string());
                    let is_folder = node.is_folder();
                    let bookmark_url = node.url().map(str::to_string);
                    let chip = match node {
                        BookmarkNode::Bookmark { url, .. } => {
                            Button::new(("browser-bookmark", index), name.clone())
                                .label_size(LabelSize::Small)
                                .start_icon(
                                    Icon::new(IconName::Bookmark)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .tooltip(Tooltip::text(bookmark_tooltip(&url)))
                                .on_click(cx.listener(move |this, event, _, cx| {
                                    this.activate_bookmark_url(&url, event, cx);
                                }))
                                .into_any_element()
                        }
                        BookmarkNode::Folder { .. } => {
                            Button::new(("browser-bookmark", index), name.clone())
                                .label_size(LabelSize::Small)
                                .start_icon(
                                    Icon::new(IconName::Folder)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .tooltip(Tooltip::text("フォルダーの中身を開く"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.bookmarks_pane_open = true;
                                    this.set_folder_expanded(&[index], true, cx);
                                }))
                                .into_any_element()
                        }
                    };
                    div()
                        .id(("browser-bookmark-bar", index))
                        .relative()
                        // 中クリックは `on_click` に届かない (左ボタン専用) ので、
                        // 「新しいタブで開く」は補助クリック側でも受ける。
                        .on_aux_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                            if let (Some(url), true) = (&bookmark_url, event.is_middle_click()) {
                                this.open_in_new_tab(url, cx);
                            }
                        }))
                        .on_drag(
                            DraggedBookmark {
                                path: vec![index],
                                name,
                            },
                            |bookmark, _, _, cx| cx.new(|_| bookmark.clone()),
                        )
                        .child(chip)
                        .when(dragging, |this| {
                            this.child(self.render_bookmark_drop_zones(index, is_folder, cx))
                        })
                        .into_any_element()
                }))
                .into_any_element(),
        )
    }

    /// ドラッグ中だけチップの上に敷く落とし先。左右の端は「その手前 / 後ろへ」、
    /// フォルダーの真ん中は「フォルダーの中へ」。ドラッグ中しか出さないので
    /// 普段のクリックもツールチップも邪魔しない。
    fn render_bookmark_drop_zones(
        &self,
        index: usize,
        is_folder: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let edge = if is_folder { 0.25 } else { 0.5 };
        h_flex()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .child(
                self.render_bookmark_drop_zone(
                    ("browser-bookmark-drop-before", index),
                    BookmarkDropTarget::Before(index),
                    cx,
                )
                .w(relative(edge))
                .drag_over::<DraggedBookmark>(|style, _, _, cx| {
                    style
                        .border_l_2()
                        .border_color(cx.theme().colors().drop_target_border)
                }),
            )
            .when(is_folder, |this| {
                this.child(
                    self.render_bookmark_drop_zone(
                        ("browser-bookmark-drop-into", index),
                        BookmarkDropTarget::IntoFolder(index),
                        cx,
                    )
                    .flex_1()
                    .drag_over::<DraggedBookmark>(|style, _, _, cx| {
                        style.bg(cx.theme().colors().drop_target_background)
                    }),
                )
            })
            .child(
                self.render_bookmark_drop_zone(
                    ("browser-bookmark-drop-after", index),
                    BookmarkDropTarget::Before(index + 1),
                    cx,
                )
                .w(relative(edge))
                .drag_over::<DraggedBookmark>(|style, _, _, cx| {
                    style
                        .border_r_2()
                        .border_color(cx.theme().colors().drop_target_border)
                }),
            )
            .into_any_element()
    }

    fn render_bookmark_drop_zone(
        &self,
        id: (&'static str, usize),
        target: BookmarkDropTarget,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .h_full()
            .on_drop(cx.listener(move |this, dragged: &DraggedBookmark, _, cx| {
                this.drop_bookmark(&dragged.path, target, cx)
            }))
    }

    /// バーの上で落とされたブックマークを動かす。バーに出ているのは最上位の項目
    /// だけなので、掴めるのも最上位だけ。
    fn drop_bookmark(
        &mut self,
        path: &BookmarkPath,
        target: BookmarkDropTarget,
        cx: &mut Context<Self>,
    ) {
        if path.len() != 1 {
            return;
        }
        let path = path.clone();
        match target {
            BookmarkDropTarget::Before(to_index) => self.update_bookmarks(cx, move |file| {
                file.reorder_within_parent(&path, to_index).is_some()
            }),
            BookmarkDropTarget::IntoFolder(folder) => self.update_bookmarks(cx, move |file| {
                file.move_to_folder(&path, &[folder]).is_some()
            }),
        }
    }

    fn render_bookmarks_pane(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let file = self.bookmarks_file(cx);
        let loaded = self.bookmarks.read(cx).is_loaded();
        let rows = file.visible_rows(&|path| !self.collapsed_folders.contains(path));

        v_flex()
            .w(px(BOOKMARKS_PANE_WIDTH))
            .flex_none()
            .h_full()
            .border_r_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .w_full()
                    .px_1()
                    .py_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        Label::new("ブックマーク")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(
                        IconButton::new("browser-bookmark-add-folder", IconName::FolderAdd)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("フォルダーを追加"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_bookmark_form(
                                    None,
                                    true,
                                    Vec::new(),
                                    String::new(),
                                    String::new(),
                                    window,
                                    cx,
                                );
                            })),
                    )
                    .child(
                        IconButton::new("browser-bookmark-close-pane", IconName::Close)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("ブックマークを閉じる"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_bookmarks(&ToggleBookmarks, window, cx)
                            })),
                    ),
            )
            .children(self.render_bookmark_form(&file, window, cx))
            .child(
                v_flex()
                    .id("browser-bookmark-list")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .p_0p5()
                    .gap_px()
                    .overflow_y_scroll()
                    .when(rows.is_empty(), |this| {
                        this.child(
                            div().p_2().child(
                                Label::new(if loaded {
                                    "ブックマークはまだありません。ツールバーの星ボタンで今のページを追加できます。"
                                } else {
                                    "読み込み中…"
                                })
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            ),
                        )
                    })
                    .children(
                        rows.iter()
                            .enumerate()
                            .map(|(index, row)| self.render_bookmark_row(index, row, cx)),
                    ),
            )
            .into_any_element()
    }

    fn render_bookmark_row(
        &self,
        index: usize,
        row: &BookmarkRow,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.pending_delete.as_deref() == Some(row.path.as_slice()) {
            let path = row.path.clone();
            return h_flex()
                .w_full()
                .gap_1()
                .px_1()
                .py_0p5()
                .rounded_sm()
                .bg(cx.theme().status().error_background)
                .child(
                    Label::new(if row.is_folder {
                        "フォルダーごと削除しますか?"
                    } else {
                        "削除しますか?"
                    })
                    .size(LabelSize::Small),
                )
                .child(div().flex_1())
                .child(
                    Button::new(("browser-bookmark-confirm-delete", index), "削除")
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let path = path.clone();
                            this.update_bookmarks(cx, move |file| file.remove(&path).is_some());
                        })),
                )
                .child(
                    Button::new(("browser-bookmark-cancel-delete", index), "取消")
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.pending_delete = None;
                            cx.notify();
                        })),
                )
                .into_any_element();
        }

        let icon = if row.is_folder {
            if row.expanded {
                IconName::FolderOpen
            } else {
                IconName::Folder
            }
        } else {
            IconName::Bookmark
        };
        let name = SharedString::from(row.name.clone());
        let tooltip = match &row.url {
            Some(url) => bookmark_tooltip(url),
            None => row.name.clone().into(),
        };

        let activate = {
            let path = row.path.clone();
            let url = row.url.clone();
            let expanded = row.expanded;
            cx.listener(move |this, event: &ClickEvent, _, cx| match &url {
                Some(url) => this.activate_bookmark_url(url, event, cx),
                None => this.set_folder_expanded(&path, !expanded, cx),
            })
        };
        let open_in_new_tab = {
            let url = row.url.clone();
            cx.listener(move |this, event: &ClickEvent, _, cx| {
                if let (Some(url), true) = (&url, event.is_middle_click()) {
                    this.open_in_new_tab(url, cx);
                }
            })
        };

        h_flex()
            .w_full()
            .gap_0p5()
            .pr_0p5()
            .pl(px(2. + row.depth as f32 * 10.))
            .rounded_sm()
            .hover(|style| style.bg(cx.theme().colors().element_hover))
            .child(
                h_flex()
                    .id(("browser-bookmark-open", index))
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .py_0p5()
                    .cursor_pointer()
                    .tooltip(Tooltip::text(tooltip))
                    .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
                    .child(Label::new(name).size(LabelSize::Small).truncate())
                    .on_click(activate)
                    .on_aux_click(open_in_new_tab),
            )
            .child(
                IconButton::new(("browser-bookmark-edit", index), IconName::Pencil)
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text("編集"))
                    .on_click({
                        let path = row.path.clone();
                        cx.listener(move |this, _, window, cx| {
                            this.edit_bookmark(path.clone(), window, cx)
                        })
                    }),
            )
            .child(
                IconButton::new(("browser-bookmark-up", index), IconName::ArrowUp)
                    .icon_size(IconSize::XSmall)
                    .disabled(row.index == 0)
                    .tooltip(Tooltip::text("上へ"))
                    .on_click({
                        let path = row.path.clone();
                        cx.listener(move |this, _, _, cx| {
                            let path = path.clone();
                            this.update_bookmarks(cx, move |file| {
                                file.move_within_parent(&path, -1).is_some()
                            });
                        })
                    }),
            )
            .child(
                IconButton::new(("browser-bookmark-down", index), IconName::ArrowDown)
                    .icon_size(IconSize::XSmall)
                    .disabled(row.index + 1 >= row.sibling_count)
                    .tooltip(Tooltip::text("下へ"))
                    .on_click({
                        let path = row.path.clone();
                        cx.listener(move |this, _, _, cx| {
                            let path = path.clone();
                            this.update_bookmarks(cx, move |file| {
                                file.move_within_parent(&path, 1).is_some()
                            });
                        })
                    }),
            )
            .child(
                IconButton::new(("browser-bookmark-delete", index), IconName::Trash)
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text("削除"))
                    .on_click({
                        let path = row.path.clone();
                        cx.listener(move |this, _, _, cx| {
                            this.pending_delete = Some(path.clone());
                            cx.notify();
                        })
                    }),
            )
            .into_any_element()
    }

    fn render_bookmark_form(
        &self,
        file: &BookmarksFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let form = self.bookmark_form.as_ref()?;
        let title = match (&form.editing, form.is_folder) {
            (Some(_), true) => "フォルダーを編集",
            (Some(_), false) => "ブックマークを編集",
            (None, true) => "フォルダーを追加",
            (None, false) => "ブックマークを追加",
        };
        let name_focused = form.name.focus_handle(cx).contains_focused(window, cx);
        let url_focused = form.url.focus_handle(cx).contains_focused(window, cx);
        let field = |editor: &Entity<Editor>, focused: bool| {
            ui::input_frame_sized(ui::InputFrameSize::Compact, focused, cx)
                .w_full()
                .child(editor.clone())
        };

        // フォルダーの移動先はドロップダウンにできない (ページに隠れる) ので、
        // ラジオボタンのように押せる平らな一覧にする。
        let mut folder_buttons = vec![("最上位".to_string(), 0usize, Vec::new())];
        folder_buttons.extend(
            file.folders()
                .into_iter()
                .map(|(path, depth, name)| (name, depth + 1, path)),
        );

        Some(
            v_flex()
                .w_full()
                .p_1()
                .gap_1()
                .border_b_1()
                .border_color(cx.theme().colors().border)
                .bg(cx.theme().colors().element_background)
                .child(Label::new(title).size(LabelSize::Small))
                .child(field(&form.name, name_focused))
                .when(!form.is_folder, |this| {
                    this.child(field(&form.url, url_focused))
                })
                .child(
                    Label::new("場所")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    v_flex()
                        .id("browser-bookmark-folder-picker")
                        .max_h(px(120.))
                        .w_full()
                        .gap_px()
                        .overflow_y_scroll()
                        .children(folder_buttons.into_iter().enumerate().map(
                            |(index, (name, depth, path))| {
                                let selected = form.parent == path;
                                // 自分自身の中へは移せないので選ばせない。
                                let forbidden = form
                                    .editing
                                    .as_ref()
                                    .is_some_and(|editing| path.starts_with(editing));
                                div().pl(px(depth as f32 * 8.)).child(
                                    Button::new(("browser-bookmark-folder", index), name)
                                        .label_size(LabelSize::Small)
                                        .full_width()
                                        .disabled(forbidden)
                                        .toggle_state(selected)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if let Some(form) = this.bookmark_form.as_mut() {
                                                form.parent = path.clone();
                                            }
                                            cx.notify();
                                        })),
                                )
                            },
                        )),
                )
                .child(
                    h_flex()
                        .w_full()
                        .gap_1()
                        .child(
                            Button::new("browser-bookmark-save", "保存")
                                .label_size(LabelSize::Small)
                                .full_width()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.submit_bookmark_form(window, cx)
                                })),
                        )
                        .child(
                            Button::new("browser-bookmark-cancel", "取消")
                                .label_size(LabelSize::Small)
                                .full_width()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.bookmark_form = None;
                                    window.focus(&this.focus_handle, cx);
                                    cx.notify();
                                })),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_surface(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match &self.surface {
            SurfaceState::Unavailable {
                message,
                offer_runtime_download,
            } => self.render_unavailable(message.clone(), *offer_runtime_download, cx),
            SurfaceState::Idle | SurfaceState::Starting | SurfaceState::Ready => {
                let presentation = self.surface_presentation(window, cx);
                // 穴を開けてよいのは実際にページが載っているときだけ。ページの
                // 無い矩形を透明にすると、ウィンドウの背後 (デスクトップ) が
                // そのまま見えてしまう。
                let surface_is_up = matches!(self.surface, SurfaceState::Ready);
                // ドラッグプレビューは毎フレーム動くので、切り抜くと 1 フレームに
                // 1 回 `SetWindowRgn` を撃つことになり、プレビューの周りに継ぎ目が
                // 出る。ドラッグ中はウィンドウがマウスをキャプチャしていて子 HWND
                // にクリックが渡らないので、そもそも切り抜く必要がない。
                let cut_overlays = !cx.has_active_drag();
                let webview = self.webview.clone();
                let focus_handle = self.focus_handle.clone();
                div()
                    .relative()
                    .size_full()
                    .bg(cx.theme().colors().editor_background)
                    // 起動中の案内は子 HWND が載るのと同じ場所に敷いておく。
                    // ページが出る段になると下の canvas が同じ矩形を透明に
                    // 抜くので、案内はそのフレームから消える。
                    .child(
                        v_flex()
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full()
                            .items_center()
                            .justify_center()
                            .gap_2()
                            .p_4()
                            .when(matches!(self.surface, SurfaceState::Starting), |this| {
                                this.child(
                                    Label::new("内蔵ブラウザーを起動しています…")
                                        .color(Color::Muted),
                                )
                            }),
                    )
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, _, window, cx| {
                                let Some(webview) = webview else {
                                    return;
                                };
                                let view_focused = focus_handle.contains_focused(window, cx);
                                sync_surface_viewport(
                                    &webview,
                                    Viewport {
                                        origin_x: f32::from(bounds.origin.x),
                                        origin_y: f32::from(bounds.origin.y),
                                        width: f32::from(bounds.size.width),
                                        height: f32::from(bounds.size.height),
                                        scale_factor: window.scale_factor(),
                                        hidden_reason: presentation.hidden_reason,
                                        input_blocked: presentation.input_blocked,
                                        overlays: if cut_overlays {
                                            overlay_rects(window)
                                        } else {
                                            Vec::new()
                                        },
                                    },
                                    view_focused,
                                    window,
                                );
                                // 子 HWND は gpui の合成面の「後ろ」に居るので、
                                // ページの矩形だけ透明に抜いて見せる。この後に
                                // 描かれるツールチップ / ポップオーバー / モーダルは
                                // 普通に手前へ出る (ADR-F19)。
                                if surface_is_up && presentation.hidden_reason.is_none() {
                                    window.paint_punch_through(bounds);
                                }
                                // Windows はクリックが親のクライアント領域に落ちても
                                // 子 HWND からフォーカスを外さないので、GPUI 側に
                                // 届いたマウス押下で明示的に取り戻す。
                                window.on_mouse_event(move |_: &MouseDownEvent, phase, _, _| {
                                    if phase == DispatchPhase::Capture {
                                        webview.release_keyboard_focus();
                                    }
                                });
                            },
                        )
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full(),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_unavailable(
        &self,
        message: SharedString,
        offer_runtime_download: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .p_4()
            .child(Label::new(message).color(Color::Muted))
            .when(offer_runtime_download, |this| {
                this.child(
                    Button::new("browser-install-runtime", "ダウンロードページを開く")
                        .on_click(|_, _, cx| cx.open_url(WEBVIEW2_DOWNLOAD_URL)),
                )
            })
            .child(
                Button::new("browser-open-external", "外部ブラウザーで開く").on_click(cx.listener(
                    |this, _, window, cx| {
                        this.open_in_external_browser(&OpenInExternalBrowser, window, cx)
                    },
                )),
            )
            .into_any_element()
    }
}

impl Render for BrowserView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // `Panel::set_active(true)` を取りこぼしても表示されるように、描画のたびに
        // 生成を試みる (2 回目以降は即座に戻る)。
        self.ensure_surface(window, cx);
        // テーマの追従。`SettingsStore` の監視だけでは OS の外観変更 (自動テーマ) を
        // 拾えないので、描画のたびに差分がある時だけ WebView2 へ流す。
        self.sync_appearance(cx);

        // 1 行を常に確保しておく。出し入れすると内容領域の高さが変わり、
        // 子 HWND の矩形が毎回動いてちらつくため。ページタイトルはタブに出るので
        // ここには出さない (行そのものは状態表示のために残す)。
        let status = match (&self.status_message, self.loading) {
            (Some(message), _) => message.clone(),
            (None, true) => "読み込み中…".into(),
            (None, false) => SharedString::default(),
        };
        let bookmarks_pane_open = self.bookmarks_pane_open;
        let bookmarks_bar = self
            .shows_bookmark_bar(cx)
            .then(|| self.render_bookmarks_bar(cx))
            .flatten();
        v_flex()
            .key_context("BrowserPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .on_action(cx.listener(Self::navigate_back))
            .on_action(cx.listener(Self::navigate_forward))
            .on_action(cx.listener(Self::reload))
            .on_action(cx.listener(Self::go_home))
            .on_action(cx.listener(Self::focus_address_bar))
            .on_action(cx.listener(Self::submit_address))
            .on_action(cx.listener(Self::open_in_external_browser))
            .on_action(cx.listener(Self::toggle_bookmarks))
            .on_action(cx.listener(Self::add_bookmark))
            .on_action(cx.listener(Self::set_as_home_page))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if let Some(webview) = &this.webview {
                        webview.release_keyboard_focus();
                    }
                    cx.emit(BrowserViewEvent::Focused);
                }),
            )
            .child(self.render_toolbar(window, cx))
            .children(bookmarks_bar)
            .child(
                h_flex()
                    .w_full()
                    .min_h(window.line_height())
                    .px_2()
                    .pb_1()
                    .child(
                        Label::new(status)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    ),
            )
            .child(
                // 内容領域は「残り全部」。子 HWND の矩形はこの canvas の bounds から
                // 取るので、ここで高さも幅も潰れないようにしておくのが要。
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .when(bookmarks_pane_open, |this| {
                        this.child(self.render_bookmarks_pane(window, cx))
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(self.render_surface(window, cx)),
                    ),
            )
    }
}

impl Focusable for BrowserView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// ブックマークのツールチップ。「Ctrl+クリックで新しいタブ」は見た目からは
/// 分からないので、URL と一緒に添える。
fn bookmark_tooltip(url: &str) -> SharedString {
    format!("{url} — Ctrl+クリック / 中クリックで新しいタブ").into()
}

/// タブに出す名前。ページタイトルが無ければ URL、それも無ければ既定名。
/// 長いページタイトルはタブを押し広げるので、ここで切り詰める。
pub(crate) fn tab_label(page_title: &str, url: &str) -> SharedString {
    let source = match (page_title.trim(), url.trim()) {
        ("", "") => return UNTITLED_TAB_LABEL.into(),
        ("", url) => url,
        (title, _) => title,
    };
    if source.chars().count() <= MAX_TAB_TITLE_CHARS {
        return source.to_string().into();
    }
    let truncated: String = source.chars().take(MAX_TAB_TITLE_CHARS).collect();
    format!("{truncated}…").into()
}

/// gpui がこのフレームで要素ツリーの上に描くもの (ツールチップ / コンテキスト
/// メニュー / ポップオーバー) の矩形。子 HWND は gpui の描画より手前に合成される
/// ので、重なった分は WebView の側で切り抜かないと見えない (ADR-F19)。
fn overlay_rects(window: &Window) -> Vec<OverlayRect> {
    window
        .overlay_bounds()
        .into_iter()
        .map(|bounds| OverlayRect {
            left: f32::from(bounds.origin.x),
            top: f32::from(bounds.origin.y),
            right: f32::from(bounds.right()),
            bottom: f32::from(bounds.bottom()),
        })
        .collect()
}

/// 描画面の位置とオーバーレイの切り抜きを WebView に渡す。
///
/// 穴を開けるのはこのフレームを Present した後にしたい: gpui のペイント中に
/// 子 HWND の領域を削ると、ツールチップがまだ画面に出ていないうちにホスト
/// ウィンドウ (背景ブラシは黒) が覗いて一瞬ちらつく。切り抜きが必要になった
/// フレームだけ `on_next_frame` に回す — 保留中のコールバックがあると gpui は
/// フレームレート制限を外すので、変化が無いフレームでは登録しない。
///
/// `view_focused` はビューが GPUI のフォーカスを持っているか。ページの入力を
/// 止めている間に来たフォーカス要求は、これが立っている間しか後から流さない
/// (よそへ移ったフォーカスをページが奪わないように)。
#[cfg(windows)]
fn sync_surface_viewport(
    webview: &Rc<WebView>,
    viewport: Viewport,
    view_focused: bool,
    window: &mut Window,
) {
    if webview.sync_viewport(viewport, view_focused) {
        let webview = webview.clone();
        window.on_next_frame(move |_, _| webview.apply_deferred_clip());
    }
}

#[cfg(not(windows))]
fn sync_surface_viewport(webview: &Rc<WebView>, viewport: Viewport, _: bool, _: &mut Window) {
    webview.sync_viewport(viewport);
}

/// このフレームで子 HWND をどう扱うか。[`surface_presentation_for`] が決める。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SurfacePresentation {
    /// `None` なら見せる。`Some` なら隠す理由 (ログにだけ使う)。
    hidden_reason: Option<&'static str>,
    /// 見せたまま子 HWND を無効にして、ページへのマウス入力を止める。隠すときは
    /// 立てない (隠れた子 HWND はそもそも入力を受けない)。
    input_blocked: bool,
}

/// 子 HWND を隠すか、入力を止めるか。純粋関数にして判定を表で確かめられるように
/// してある。
///
/// - **モーダル**は deferred draw ではないので [`Window::overlay_bounds`] に載らず、
///   穴が開かない = そのままでは子 HWND が、モーダル自身と「外側クリックで閉じる」
///   背景宛てのクリックを飲む。DirectComposition が効いていればモーダルは穴の上に
///   描かれるので、ページは見せたまま (穴も開けたまま) 入力だけ止める。無効な子
///   ウィンドウは Windows のヒットテストで飛ばされ、クリックは GPUI に届く
///   (ADR-F48)。DComp 無効時は子 HWND が手前に出てモーダルを覆うので隠す (ADR-F32)。
/// - **ドラッグ**は DirectComposition が効いているなら隠さない。プレビューは穴の
///   あとに描かれるので手前に出るし、ドラッグ中はウィンドウがマウスをキャプチャ
///   しているので子 HWND がドロップを横取りすることもない。DComp 無効時だけは
///   子 HWND が手前に出てプレビューを覆うので従来どおり隠す。
///   タブは 2px 動かしただけでもドラッグ扱いになるので、ここで隠すと
///   「タブをクリックしただけでページが一瞬暗くなる」ことになる。
fn surface_presentation_for(
    dragging: bool,
    host_is_composited: bool,
    modal_is_open: bool,
) -> SurfacePresentation {
    if host_is_composited {
        return SurfacePresentation {
            hidden_reason: None,
            input_blocked: modal_is_open,
        };
    }
    let hidden_reason = if dragging {
        Some("a drag is in progress")
    } else if modal_is_open {
        Some("a modal is open")
    } else {
        None
    };
    SurfacePresentation {
        hidden_reason,
        input_blocked: false,
    }
}

/// テーマのエディター背景を 8 ビット 3 原色に落とす。WebView2 に渡せるのは
/// 8 ビットの色だけなので、比較もこの形で持っておく。
fn surface_background_color(cx: &App) -> (u8, u8, u8) {
    let color = gpui::Rgba::from(cx.theme().colors().editor_background);
    let channel = |value: f32| (value.clamp(0., 1.) * 255.).round() as u8;
    (channel(color.r), channel(color.g), channel(color.b))
}

#[cfg(windows)]
fn apply_surface_background(webview: &Rc<WebView>, (red, green, blue): (u8, u8, u8)) {
    webview.set_background_color(red, green, blue);
}

#[cfg(not(windows))]
fn apply_surface_background(_: &Rc<WebView>, _: (u8, u8, u8)) {}

/// GPUI がこのウィンドウを DirectComposition で合成しているか。合成していれば
/// 子 HWND は GPUI の描画の**後ろ**なので、ツールチップもドラッグプレビューも
/// サーフェスを隠さずに手前へ出せる (ADR-F32)。
#[cfg(windows)]
fn host_is_composited(host_hwnd: isize) -> bool {
    crate::webview_windows::host_uses_direct_composition(host_hwnd)
}

#[cfg(not(windows))]
fn host_is_composited(_: isize) -> bool {
    false
}

fn runtime_missing_message() -> SharedString {
    if cfg!(windows) {
        "WebView2 ランタイムが見つかりません。Microsoft Edge WebView2 ランタイムをインストールしてください。".into()
    } else {
        "このプラットフォームでは内蔵ブラウザーは未対応です。".into()
    }
}

#[cfg(windows)]
fn host_window_handle(window: &Window) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    // `gpui::Window` has its own inherent `window_handle`, so the trait method
    // has to be named explicitly.
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
        _ => None,
    }
}

#[cfg(not(windows))]
fn host_window_handle(_: &Window) -> Option<isize> {
    Some(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_labels_fall_back_from_title_to_url() {
        assert_eq!(tab_label("Zed", "https://zed.dev/"), "Zed");
        assert_eq!(
            tab_label("  ", "https://zed.dev/"),
            SharedString::from("https://zed.dev/")
        );
        assert_eq!(tab_label("", ""), UNTITLED_TAB_LABEL);
        assert_eq!(tab_label("   ", "  "), UNTITLED_TAB_LABEL);
    }

    #[test]
    fn long_tab_labels_are_truncated_by_characters() {
        let title = "あ".repeat(MAX_TAB_TITLE_CHARS + 10);
        let label = tab_label(&title, "");
        assert_eq!(label.chars().count(), MAX_TAB_TITLE_CHARS + 1);
        assert!(label.ends_with('…'));

        let exact = "い".repeat(MAX_TAB_TITLE_CHARS);
        assert_eq!(tab_label(&exact, ""), SharedString::from(exact.clone()));
    }

    const SHOWN: SurfacePresentation = SurfacePresentation {
        hidden_reason: None,
        input_blocked: false,
    };

    const SHOWN_WITHOUT_INPUT: SurfacePresentation = SurfacePresentation {
        hidden_reason: None,
        input_blocked: true,
    };

    fn hidden(reason: &'static str) -> SurfacePresentation {
        SurfacePresentation {
            hidden_reason: Some(reason),
            input_blocked: false,
        }
    }

    #[test]
    fn a_modal_keeps_the_page_visible_but_blocks_its_input_with_direct_composition() {
        // モーダルは穴の上に描かれるので、ページは見せたまま入力だけ止める。
        assert_eq!(
            surface_presentation_for(false, true, true),
            SHOWN_WITHOUT_INPUT
        );
        // モーダルの中でドラッグしていても同じ。
        assert_eq!(
            surface_presentation_for(true, true, true),
            SHOWN_WITHOUT_INPUT
        );
    }

    #[test]
    fn a_modal_hides_the_surface_without_direct_composition() {
        // 子 HWND が手前に出てモーダルを覆うので、隠すしかない。
        assert_eq!(
            surface_presentation_for(false, false, true),
            hidden("a modal is open")
        );
    }

    #[test]
    fn a_drag_only_hides_the_surface_without_direct_composition() {
        // 合成しているなら、ドラッグプレビューは穴の上に出るので隠さない。
        assert_eq!(surface_presentation_for(true, true, false), SHOWN);
        assert_eq!(
            surface_presentation_for(true, false, false),
            hidden("a drag is in progress")
        );
        assert_eq!(
            surface_presentation_for(true, false, true),
            hidden("a drag is in progress")
        );
    }

    #[test]
    fn nothing_hides_a_plain_visible_surface() {
        assert_eq!(surface_presentation_for(false, true, false), SHOWN);
        assert_eq!(surface_presentation_for(false, false, false), SHOWN);
    }
}
