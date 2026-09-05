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

use std::{collections::HashSet, rc::Rc};

use editor::Editor;
use futures::StreamExt as _;
use gpui::{
    AnyElement, App, ClipboardItem, Context, DispatchPhase, Entity, EventEmitter, FocusHandle,
    Focusable, MouseButton, MouseDownEvent, SharedString, Subscription, Task, WeakEntity, Window,
    canvas, px,
};
use settings::{Settings as _, SettingsStore};
use ui::{Tooltip, prelude::*};
use workspace::Workspace;

use crate::{
    AddBookmark, BookmarkNode, BookmarkPath, BookmarkRow, BookmarkStore, BookmarksFile,
    BrowserSettings, FocusAddressBar, GoHome, NavigateBack, NavigateForward, OpenInCenter,
    OpenInExternalBrowser, Reload, SubmitAddress, ToggleBookmarks, normalize_bookmark_url,
    resolve_address_input,
};

#[cfg(windows)]
use crate::webview_windows::{Viewport, WebView, WebViewEvent, host_uses_direct_composition};

#[cfg(not(windows))]
use crate::webview_unsupported::{Viewport, WebView, WebViewEvent, host_uses_direct_composition};

const WEBVIEW2_DOWNLOAD_URL: &str = "https://developer.microsoft.com/microsoft-edge/webview2/";

/// ブックマークのサイドペインの幅。ツリーの行に「編集 / 上へ / 下へ / 削除」の
/// アイコンボタンが並ぶので、名前が潰れない程度には要る。
const BOOKMARKS_PANE_WIDTH: f32 = 244.;

/// 子 HWND が表示されない環境で案内するコマンド (ADR-F19 の既知の制約)。
const DISABLE_DIRECT_COMPOSITION_COMMAND: &str = "setx GPUI_DISABLE_DIRECT_COMPOSITION 1";

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
}

pub struct BrowserView {
    workspace: WeakEntity<Workspace>,
    host: BrowserHost,
    focus_handle: FocusHandle,
    address_editor: Entity<Editor>,
    webview: Option<Rc<WebView>>,
    /// サーフェスを作ったときのホストウィンドウ。タブを別ウィンドウへ移すと
    /// 子 HWND は元のウィンドウにぶら下がったままなので、変わったら作り直す。
    host_hwnd: Option<isize>,
    surface: SurfaceState,
    /// ホストウィンドウが DirectComposition で描画されていて、子 HWND を出せない。
    /// 表示できない理由をページの下に敷いて案内する。
    direct_composition_host: bool,
    warned_missing_window_handle: bool,
    current_url: String,
    page_title: SharedString,
    status_message: Option<SharedString>,
    can_go_back: bool,
    can_go_forward: bool,
    loading: bool,
    page_has_focus: bool,
    applied_dark_mode: Option<bool>,
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
        workspace: &Workspace,
        host: BrowserHost,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let address_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("URL または検索語を入力", window, cx);
            editor
        });

        let fs = workspace.app_state().fs.clone();
        let bookmarks = BookmarkStore::global(fs, cx);

        let mut subscriptions = Vec::new();
        subscriptions.push(cx.observe_global::<SettingsStore>(|this, cx| {
            this.sync_appearance(cx);
            cx.notify();
        }));
        subscriptions.push(cx.observe(&bookmarks, |_, _, cx| cx.notify()));

        Self {
            workspace: workspace.weak_handle(),
            host,
            focus_handle: cx.focus_handle(),
            address_editor,
            webview: None,
            host_hwnd: None,
            surface: SurfaceState::Idle,
            direct_composition_host: false,
            warned_missing_window_handle: false,
            current_url: String::new(),
            page_title: SharedString::default(),
            status_message: None,
            can_go_back: false,
            can_go_forward: false,
            loading: false,
            page_has_focus: false,
            applied_dark_mode: None,
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
    pub fn hide_surface_from_host(&mut self, reason: &'static str) {
        if let Some(webview) = &self.webview {
            webview.hide(reason);
        }
    }

    pub fn open_bookmarks_pane(&mut self, cx: &mut Context<Self>) {
        self.bookmarks_pane_open = true;
        cx.notify();
    }

    /// ホストが決めた最初の URL。サーフェスはまだ無いので [`Self::navigate_to`] は
    /// 使えない (WebView2 が無いときは外部ブラウザーに落ちる仕様のため)。
    /// [`Self::ensure_surface`] がコントローラーを作るときにこの URL へ飛ばす。
    pub fn set_initial_url(&mut self, url: String, window: &mut Window, cx: &mut Context<Self>) {
        if url.trim().is_empty() {
            return;
        }
        self.current_url = url;
        self.loading = true;
        self.sync_address_editor(window, cx);
        cx.emit(BrowserViewEvent::LocationChanged);
        cx.notify();
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

        self.direct_composition_host = host_uses_direct_composition(host_hwnd);
        if self.direct_composition_host {
            log::warn!(
                "browser view: the gpui window has WS_EX_NOREDIRECTIONBITMAP (DirectComposition), \
                 so a windowed WebView2 child cannot be composited; \
                 set GPUI_DISABLE_DIRECT_COMPOSITION=1 and restart to use the built-in browser"
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
                self.sync_appearance(cx);
                if self.current_url.is_empty() {
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
            WebViewEvent::PageFocusChanged(focused) => self.page_has_focus = focused,
            WebViewEvent::OpenInPlace(url) => self.navigate_to(&url, cx),
        }
        // 生成完了は非同期に届くので、ここで再描画しないと canvas が走らず、
        // コントローラーは 0x0 のまま (= 何も表示されない) になる。
        cx.notify();
    }

    fn sync_appearance(&mut self, cx: &mut Context<Self>) {
        let dark = !cx.theme().appearance().is_light();
        if self.applied_dark_mode == Some(dark) {
            return;
        }
        if let Some(webview) = &self.webview {
            webview.set_dark_mode(dark);
            self.applied_dark_mode = Some(dark);
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

    /// 子 HWND は GPUI の合成面より手前に出るので、覆ってはいけない状況では
    /// 隠す (ADR-F19 の z-order 緩和策)。
    ///
    /// **このメソッドが呼ばれている = ビューが描画されている** ので、ホストは
    /// このビューを表示している。はっきり検出できた条件でだけ隠す。
    fn surface_hidden_reason(&self, window: &mut Window, cx: &mut App) -> Option<&'static str> {
        if self.direct_composition_host {
            // 合成されないだけで子 HWND は `WS_VISIBLE` のまま残るため、内容領域の
            // クリックを全部飲んでしまい、案内のボタンさえ押せなくなる。隠す。
            return Some("the host window is composited with DirectComposition");
        }
        if cx.has_active_drag() {
            return Some("a drag is in progress");
        }
        let workspace = self.workspace.upgrade()?;
        workspace
            .update(cx, |workspace, cx| workspace.has_active_modal(window, cx))
            .then_some("a modal is open")
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let focus_handle = self.focus_handle.clone();
        h_flex()
            .w_full()
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
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .px_1p5()
                    .py_0p5()
                    .rounded_sm()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().editor_background)
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
                                "エディター領域で開く",
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

    /// ツールバー直下のブックマークバー。最上位の項目だけを並べ、フォルダーは
    /// サイドペインをそのフォルダーで開くボタンにする (メニューはページに隠れるため)。
    fn render_bookmarks_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let items = self.bookmarks_file(cx).items;
        if items.is_empty() {
            return None;
        }
        Some(
            h_flex()
                .w_full()
                .flex_wrap()
                .gap_1()
                .px_1()
                .pb_1()
                .children(items.into_iter().enumerate().map(|(index, node)| {
                    let name = SharedString::from(node.name().to_string());
                    match node {
                        BookmarkNode::Bookmark { url, .. } => {
                            Button::new(("browser-bookmark", index), name)
                                .label_size(LabelSize::Small)
                                .start_icon(
                                    Icon::new(IconName::Bookmark)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .tooltip(Tooltip::text(url.clone()))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.navigate_to(&url, cx);
                                }))
                        }
                        BookmarkNode::Folder { .. } => {
                            Button::new(("browser-bookmark", index), name)
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
                        }
                    }
                }))
                .into_any_element(),
        )
    }

    fn render_bookmarks_pane(&self, cx: &mut Context<Self>) -> AnyElement {
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
            .children(self.render_bookmark_form(&file, cx))
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
        let tooltip = row.url.clone().unwrap_or_else(|| row.name.clone());

        let activate = {
            let path = row.path.clone();
            let url = row.url.clone();
            let expanded = row.expanded;
            cx.listener(move |this, _, _, cx| match &url {
                Some(url) => this.navigate_to(url, cx),
                None => this.set_folder_expanded(&path, !expanded, cx),
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
                    .on_click(activate),
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
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let form = self.bookmark_form.as_ref()?;
        let title = match (&form.editing, form.is_folder) {
            (Some(_), true) => "フォルダーを編集",
            (Some(_), false) => "ブックマークを編集",
            (None, true) => "フォルダーを追加",
            (None, false) => "ブックマークを追加",
        };
        let field = |editor: &Entity<Editor>| {
            h_flex()
                .w_full()
                .px_1()
                .py_0p5()
                .rounded_sm()
                .border_1()
                .border_color(cx.theme().colors().border)
                .bg(cx.theme().colors().editor_background)
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
                .child(field(&form.name))
                .when(!form.is_folder, |this| this.child(field(&form.url)))
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
                let hidden_reason = self.surface_hidden_reason(window, cx);
                let webview = self.webview.clone();
                div()
                    .relative()
                    .size_full()
                    .bg(cx.theme().colors().editor_background)
                    // 案内は子 HWND が載るのと同じ場所に敷いておく。ページが描けて
                    // いれば覆われて見えないので、見えている = 描けていない。
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
                            })
                            .when(self.direct_composition_host, |this| {
                                this.child(self.render_direct_composition_notice(cx))
                            }),
                    )
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, _, window, _| {
                                let Some(webview) = webview else {
                                    return;
                                };
                                webview.sync_viewport(Viewport {
                                    origin_x: f32::from(bounds.origin.x),
                                    origin_y: f32::from(bounds.origin.y),
                                    width: f32::from(bounds.size.width),
                                    height: f32::from(bounds.size.height),
                                    scale_factor: window.scale_factor(),
                                    hidden_reason,
                                });
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

    /// gpui のウィンドウが DirectComposition で描かれていると、子 HWND は
    /// リダイレクション面を持てず (さらに gpui の合成ツリーが topmost なので)
    /// WebView2 は一切描画されない。直せるのはユーザーだけなので手順を出す。
    fn render_direct_composition_notice(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .max_w(px(360.))
            .gap_1()
            .items_center()
            .child(Label::new("内蔵ブラウザーの画面を表示できません。").color(Color::Error))
            .child(
                Label::new(
                    "Zed のウィンドウが DirectComposition で描画されているため、\
                     WebView2 の子ウィンドウが合成されません。\
                     環境変数 GPUI_DISABLE_DIRECT_COMPOSITION に 1 を設定して \
                     Zed を再起動すると表示されます。",
                )
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("browser-copy-dc-command", "コマンドをコピー")
                            .label_size(LabelSize::Small)
                            .tooltip(Tooltip::text(DISABLE_DIRECT_COMPOSITION_COMMAND))
                            .on_click(|_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    DISABLE_DIRECT_COMPOSITION_COMMAND.to_string(),
                                ));
                            }),
                    )
                    .child(
                        Button::new("browser-open-external-dc", "外部ブラウザーで開く")
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_in_external_browser(&OpenInExternalBrowser, window, cx)
                            })),
                    ),
            )
            .into_any_element()
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
        // 子 HWND の矩形が毎回動いてちらつくため。
        let status = match (&self.status_message, self.loading) {
            (Some(message), _) => message.clone(),
            (None, true) => "読み込み中…".into(),
            (None, false) => self.page_title.clone(),
        };
        let bookmarks_pane_open = self.bookmarks_pane_open;
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
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    if let Some(webview) = &this.webview {
                        webview.release_keyboard_focus();
                    }
                }),
            )
            .child(self.render_toolbar(cx))
            .children(self.render_bookmarks_bar(cx))
            .child(
                h_flex().w_full().px_2().pb_1().child(
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
                        this.child(self.render_bookmarks_pane(cx))
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
}
