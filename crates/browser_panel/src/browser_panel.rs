//! 内蔵ブラウザーパネル (ADR-F19)。
//!
//! Windows では Microsoft Edge WebView2 を GPUI のトップレベル HWND の子として
//! 載せる。人間が URL を打つ操作は AI / ツールのデータ持ち出しとは性質が違うため、
//! 通信は既定で egress 許可リストの対象外 (ADR-F18 の「人間のターミナルは対象外」と
//! 同じ判断)。`browser.use_egress_proxy` を有効にしたときだけ ADR-F14 の loopback
//! プロキシを経由させ、許可リストをブラウジングにも効かせる。
//!
//! ブックマークは設定ではなく `browser_bookmarks.json` に置く ([`browser_bookmarks`])。
//! 子 HWND は GPUI の合成面より手前に出るので、ポップオーバーやドロップダウンは
//! ページに隠れる。そのためブックマークの管理 UI はオーバーレイではなく、
//! パネル内の左サイドペイン (開くとページ側が縮む) として作ってある。

use std::{collections::HashSet, rc::Rc, sync::Arc};

use anyhow::Result;
use editor::Editor;
use futures::StreamExt as _;
use gpui::{
    Action, AnyElement, App, AsyncWindowContext, ClipboardItem, Context, DispatchPhase, Entity,
    EventEmitter, FocusHandle, Focusable, MouseButton, MouseDownEvent, Pixels, SharedString,
    Subscription, Task, WeakEntity, Window, actions, canvas, px,
};
use settings::{IntoGpui, RegisterSetting, Settings, SettingsStore};
use ui::{Tooltip, prelude::*};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    searchable::SearchableItem as _,
};

mod browser_bookmarks;

pub use browser_bookmarks::{
    BookmarkNode, BookmarkPath, BookmarkRow, BookmarkStore, BookmarksFile, parse_bookmarks_file,
    serialize_bookmarks_file,
};

#[cfg(windows)]
mod webview_windows;
#[cfg(windows)]
use webview_windows::{Viewport, WebView, WebViewEvent, host_uses_direct_composition};

#[cfg(not(windows))]
use webview_unsupported::{Viewport, WebView, WebViewEvent, host_uses_direct_composition};

const BROWSER_PANEL_KEY: &str = "BrowserPanel";
const WEBVIEW2_DOWNLOAD_URL: &str = "https://developer.microsoft.com/microsoft-edge/webview2/";
const QUERY_PLACEHOLDER: &str = "{query}";

/// 検索・質問に渡すテキストの上限。長すぎる選択範囲で URL が壊れるのを防ぐ。
const MAX_QUERY_CHARS: usize = 400;

/// ブックマークのサイドペインの幅。ツリーの行に「編集 / 上へ / 下へ / 削除」の
/// アイコンボタンが並ぶので、名前が潰れない程度には要る。
const BOOKMARKS_PANE_WIDTH: f32 = 244.;

/// 子 HWND が表示されない環境で案内するコマンド (ADR-F19 の既知の制約)。
const DISABLE_DIRECT_COMPOSITION_COMMAND: &str = "setx GPUI_DISABLE_DIRECT_COMPOSITION 1";

actions!(
    browser_panel,
    [
        /// 内蔵ブラウザーパネルの表示を切り替えます。
        Toggle,
        /// 内蔵ブラウザーパネルのフォーカスを切り替えます。
        ToggleFocus,
        /// 前のページに戻ります。
        NavigateBack,
        /// 次のページに進みます。
        NavigateForward,
        /// 現在のページを再読み込みします。
        Reload,
        /// アドレスバーにフォーカスを移します。
        FocusAddressBar,
        /// アドレスバーに入力した URL または検索語を開きます。
        SubmitAddress,
        /// ホームに設定した URL を開きます。
        GoHome,
        /// 現在のページを OS の既定のブラウザーで開きます。
        OpenInExternalBrowser,
        /// ブックマークの一覧を開閉します。
        ToggleBookmarks,
        /// 現在のページをブックマークに追加します。
        AddBookmark,
    ]
);

// エディターの右クリックメニューから参照できるよう、選択テキストを渡す 2 つだけは
// `zed_actions` 側に置いてある (`editor` は `browser_panel` に依存できないため)。
pub use zed_actions::browser::{AskCopilot, SearchSelection};

// ---------------------------------------------------------------------------
// 設定
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct BrowserSettings {
    pub button: bool,
    pub dock: DockPosition,
    pub default_width: Pixels,
    pub home_url: String,
    pub search_url: String,
    pub copilot_url: String,
    pub use_egress_proxy: bool,
}

pub const DEFAULT_HOME_URL: &str = "https://github.com/copilot";
pub const DEFAULT_SEARCH_URL: &str = "https://www.google.com/search?q={query}";
pub const DEFAULT_COPILOT_URL: &str = "https://github.com/copilot";

impl Settings for BrowserSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let browser = content.browser.clone().unwrap_or_default();
        Self {
            button: browser.button.unwrap_or(true),
            dock: browser.dock.map(Into::into).unwrap_or(DockPosition::Right),
            default_width: browser
                .default_width
                .map(|width| width.into_gpui())
                .unwrap_or(px(480.)),
            home_url: browser
                .home_url
                .unwrap_or_else(|| DEFAULT_HOME_URL.to_string()),
            search_url: browser
                .search_url
                .unwrap_or_else(|| DEFAULT_SEARCH_URL.to_string()),
            copilot_url: browser
                .copilot_url
                .unwrap_or_else(|| DEFAULT_COPILOT_URL.to_string()),
            use_egress_proxy: browser.use_egress_proxy.unwrap_or(false),
        }
    }
}

// ---------------------------------------------------------------------------
// URL の組み立て
// ---------------------------------------------------------------------------

/// `Url::parse` がスキームとして受け付けてしまう入力 (`localhost:8080` など) を
/// 誤ってナビゲーションに使わないよう、実際に開いてよいスキームだけを列挙する。
const NAVIGABLE_SCHEMES: &[&str] = &[
    "http",
    "https",
    "file",
    "about",
    "data",
    "edge",
    "view-source",
];

/// アドレスバーの入力を「そのまま開く URL」か「検索 URL」に解決する。
pub fn resolve_address_input(input: &str, search_template: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(url) = explicit_url(trimmed) {
        return Some(url);
    }
    if let Some(url) = implicit_https_url(trimmed) {
        return Some(url);
    }
    Some(build_query_url(search_template, trimmed))
}

/// ブックマークに保存する URL。アドレスバーと違って**検索には落とさない**:
/// ユーザーが打ち間違えた文字列を検索 URL に化けさせると、後から直しようがない。
pub fn normalize_bookmark_url(input: &str) -> String {
    let trimmed = input.trim();
    explicit_url(trimmed)
        .or_else(|| implicit_https_url(trimmed))
        .unwrap_or_else(|| trimmed.to_string())
}

fn explicit_url(input: &str) -> Option<String> {
    let url = url::Url::parse(input).ok()?;
    NAVIGABLE_SCHEMES
        .contains(&url.scheme())
        .then(|| url.to_string())
}

/// スキームの無いホスト名らしき入力 (`example.com/path`) を `https://` に昇格する。
fn implicit_https_url(input: &str) -> Option<String> {
    if input.chars().any(char::is_whitespace) {
        return None;
    }
    let authority = input
        .split(['/', '?', '#'])
        .next()
        .filter(|authority| !authority.is_empty())?;
    let host = authority
        .rsplit_once(':')
        .filter(|(_, port)| !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()))
        .map_or(authority, |(host, _)| host);

    let looks_like_host = host == "localhost"
        || host.parse::<std::net::IpAddr>().is_ok()
        || host.rsplit_once('.').is_some_and(|(name, tld)| {
            !name.is_empty() && tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic())
        });
    if !looks_like_host {
        return None;
    }

    let url = url::Url::parse(&format!("https://{input}")).ok()?;
    url.host().is_some().then(|| url.to_string())
}

/// テンプレートの `{query}` をパーセントエンコードした問い合わせ文字列で置換する。
/// プレースホルダーが無いテンプレートはそのまま開く (Copilot のように、クエリ
/// パラメーターが公開されていないサービスを既定にできるようにするため)。
pub fn build_query_url(template: &str, query: &str) -> String {
    let query: String = query.chars().take(MAX_QUERY_CHARS).collect();
    let encoded = urlencoding::encode(query.trim());
    template.replace(QUERY_PLACEHOLDER, encoded.as_ref())
}

pub fn template_accepts_query(template: &str) -> bool {
    template.contains(QUERY_PLACEHOLDER)
}

// ---------------------------------------------------------------------------
// パネル
// ---------------------------------------------------------------------------

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

pub struct BrowserPanel {
    workspace: WeakEntity<Workspace>,
    fs: Arc<dyn fs::Fs>,
    focus_handle: FocusHandle,
    address_editor: Entity<Editor>,
    webview: Option<Rc<WebView>>,
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

pub fn init(cx: &mut App) {
    cx.observe_new(
        |workspace: &mut Workspace, _window, _: &mut Context<Workspace>| {
            workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
                workspace.toggle_panel_focus::<BrowserPanel>(window, cx);
            });
            workspace.register_action(|workspace, _: &Toggle, window, cx| {
                if !workspace.toggle_panel_focus::<BrowserPanel>(window, cx) {
                    workspace.close_panel::<BrowserPanel>(window, cx);
                }
            });
            workspace.register_action(|workspace, _: &SearchSelection, window, cx| {
                BrowserPanel::open_selection_query(workspace, QueryKind::Search, window, cx);
            });
            workspace.register_action(|workspace, _: &AskCopilot, window, cx| {
                BrowserPanel::open_selection_query(workspace, QueryKind::Copilot, window, cx);
            });
        },
    )
    .detach();
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum QueryKind {
    Search,
    Copilot,
}

impl BrowserPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            cx.new(|cx| BrowserPanel::new(workspace, window, cx))
        })
    }

    fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let address_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("URL または検索語を入力", window, cx);
            editor
        });

        let fs = workspace.app_state().fs.clone();
        let bookmarks = BookmarkStore::global(fs.clone(), cx);

        let mut subscriptions = Vec::new();
        subscriptions.push(cx.observe_global::<SettingsStore>(|this, cx| {
            this.sync_appearance(cx);
            cx.notify();
        }));
        subscriptions.push(cx.observe(&bookmarks, |_, _, cx| cx.notify()));

        Self {
            workspace: workspace.weak_handle(),
            fs,
            focus_handle: cx.focus_handle(),
            address_editor,
            webview: None,
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

    // -- WebView2 のライフサイクル ------------------------------------------

    fn ensure_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.webview.is_some() || !matches!(self.surface, SurfaceState::Idle) {
            return;
        }

        let Some(host_hwnd) = host_window_handle(window) else {
            // ウィンドウハンドルはウィンドウが出来上がっていれば必ず取れる。
            // 取れないうちは `Idle` のままにして次の描画で作り直すが、
            // `render` から毎フレーム来るのでログは 1 回だけ。
            if !self.warned_missing_window_handle {
                self.warned_missing_window_handle = true;
                log::warn!(
                    "browser panel: the host window handle is not available yet; \
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
                "browser panel: the gpui window has WS_EX_NOREDIRECTIONBITMAP (DirectComposition), \
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
                self.webview = Some(Rc::new(webview));
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
                log::info!("browser panel: the browsing surface is ready");
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
                }
            }
            WebViewEvent::TitleChanged(title) => self.page_title = title.into(),
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
        cx.notify();
    }

    fn submit_address(&mut self, _: &SubmitAddress, window: &mut Window, cx: &mut Context<Self>) {
        // ブックマークのフォームもパネル直下の単一行エディターなので、同じ Enter が
        // ここに届く。フォームに入力中ならそちらを確定する。
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
                    log::info!("browser panel: the bookmark could not be moved into {parent:?}");
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

    // -- エディターからの導線 ------------------------------------------------

    fn open_selection_query(
        workspace: &mut Workspace,
        kind: QueryKind,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(editor) = workspace
            .active_item(cx)
            .and_then(|item| item.act_as::<Editor>(cx))
        else {
            return;
        };
        let query = editor.update(cx, |editor, cx| {
            editor.query_suggestion(Some(settings::SeedQuerySetting::Always), window, cx)
        });
        let query = query.trim().to_string();
        if query.is_empty() {
            return;
        }

        let settings = BrowserSettings::get_global(cx);
        let template = match kind {
            QueryKind::Search => settings.search_url.clone(),
            QueryKind::Copilot => settings.copilot_url.clone(),
        };
        let carries_query = template_accepts_query(&template);
        let url = build_query_url(&template, &query);

        let Some(panel) = workspace.panel::<BrowserPanel>(cx) else {
            cx.open_url(&url);
            return;
        };
        workspace.focus_panel::<BrowserPanel>(window, cx);
        panel.update(cx, |panel, cx| {
            panel.navigate_to(&url, cx);
            if !carries_query {
                // Copilot のようにクエリパラメーターが公開されていない宛先では、
                // 貼り付けられる形で渡すのが最も確実。
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(query.clone()));
                panel.status_message = Some(
                    "選択テキストをクリップボードにコピーしました。貼り付けて質問してください。"
                        .into(),
                );
            }
        });
    }

    // -- 表示 ----------------------------------------------------------------

    /// 子 HWND は GPUI の合成面より手前に出るので、覆ってはいけない状況では
    /// 隠す (ADR-F19 の z-order 緩和策)。
    ///
    /// 既定は「見せる」。**このメソッドが呼ばれている = パネルが描画されている**
    /// ので、ドックはこのパネルを表示している。はっきり検出できた条件でだけ隠す。
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

impl Render for BrowserPanel {
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

impl Focusable for BrowserPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for BrowserPanel {}

impl Panel for BrowserPanel {
    fn persistent_name() -> &'static str {
        "BrowserPanel"
    }

    fn panel_key() -> &'static str {
        BROWSER_PANEL_KEY
    }

    fn position(&self, _: &Window, cx: &App) -> DockPosition {
        Self::settings(cx).dock
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        settings::update_settings_file(self.fs.clone(), cx, move |settings, _| {
            settings.browser.get_or_insert_default().dock = Some(position.into())
        });
    }

    fn default_size(&self, _: &Window, cx: &App) -> Pixels {
        Self::settings(cx).default_width
    }

    fn icon(&self, _: &Window, cx: &App) -> Option<IconName> {
        Self::settings(cx).button.then_some(IconName::ToolWeb)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("ブラウザーパネル")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        log::info!("browser panel: set_active({active})");
        if active {
            self.ensure_surface(window, cx);
        } else if let Some(webview) = &self.webview {
            webview.hide("the panel is no longer the active dock panel");
        }
        cx.notify();
    }

    fn activation_priority(&self) -> u32 {
        // 0..=7 are taken by the upstream panels (outline = 6, debugger = 7); duplicates panic in `Dock::add_panel`.
        8
    }

    fn hide_button_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        Some(workspace::HideStatusItem::new(|settings| {
            settings.browser.get_or_insert_default().button = Some(false);
        }))
    }
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

/// Windows 以外でパネル本体を素通しでコンパイルするためのスタブ。
/// `new` は必ず失敗し、パネルは「未対応」表示 + 外部ブラウザーへの導線になる。
#[cfg(not(windows))]
#[allow(dead_code)]
mod webview_unsupported {
    use std::path::Path;

    use anyhow::{Result, anyhow};
    use futures::channel::mpsc::UnboundedSender;

    pub(crate) enum WebViewEvent {
        Ready,
        Failed(String),
        UrlChanged(String),
        TitleChanged(String),
        HistoryChanged {
            can_go_back: bool,
            can_go_forward: bool,
        },
        LoadFinished {
            succeeded: bool,
        },
        FocusReturned,
        PageFocusChanged(bool),
        OpenInPlace(String),
    }

    pub(crate) struct Viewport {
        pub(crate) origin_x: f32,
        pub(crate) origin_y: f32,
        pub(crate) width: f32,
        pub(crate) height: f32,
        pub(crate) scale_factor: f32,
        pub(crate) hidden_reason: Option<&'static str>,
    }

    pub(crate) fn host_uses_direct_composition(_host_hwnd: isize) -> bool {
        false
    }

    pub(crate) struct WebView;

    impl WebView {
        pub(crate) fn new(
            _host_hwnd: isize,
            _user_data_folder: &Path,
            _proxy_endpoint: Option<&str>,
            _events: UnboundedSender<WebViewEvent>,
        ) -> Result<Self> {
            Err(anyhow!("the embedded browser requires Windows WebView2"))
        }

        pub(crate) fn sync_viewport(&self, _: Viewport) {}
        pub(crate) fn hide(&self, _: &'static str) {}
        pub(crate) fn navigate(&self, _: &str) {}
        pub(crate) fn go_back(&self) {}
        pub(crate) fn go_forward(&self) {}
        pub(crate) fn reload(&self) {}
        pub(crate) fn set_dark_mode(&self, _: bool) {}
        pub(crate) fn release_keyboard_focus(&self) {}
        pub(crate) fn focus_page(&self) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_urls_are_opened_verbatim() {
        assert_eq!(
            resolve_address_input("https://github.com/copilot", DEFAULT_SEARCH_URL).as_deref(),
            Some("https://github.com/copilot")
        );
        assert_eq!(
            resolve_address_input("  http://127.0.0.1:11434/  ", DEFAULT_SEARCH_URL).as_deref(),
            Some("http://127.0.0.1:11434/")
        );
        assert_eq!(
            resolve_address_input("about:blank", DEFAULT_SEARCH_URL).as_deref(),
            Some("about:blank")
        );
    }

    #[test]
    fn bare_hosts_are_promoted_to_https() {
        assert_eq!(
            resolve_address_input("example.com", DEFAULT_SEARCH_URL).as_deref(),
            Some("https://example.com/")
        );
        assert_eq!(
            resolve_address_input("example.co.jp/path?a=b", DEFAULT_SEARCH_URL).as_deref(),
            Some("https://example.co.jp/path?a=b")
        );
        assert_eq!(
            resolve_address_input("localhost:3000", DEFAULT_SEARCH_URL).as_deref(),
            Some("https://localhost:3000/")
        );
        assert_eq!(
            resolve_address_input("192.168.0.1", DEFAULT_SEARCH_URL).as_deref(),
            Some("https://192.168.0.1/")
        );
    }

    #[test]
    fn everything_else_becomes_a_search() {
        assert_eq!(
            resolve_address_input("rust lifetime elision", DEFAULT_SEARCH_URL).as_deref(),
            Some("https://www.google.com/search?q=rust%20lifetime%20elision")
        );
        // ドットを含んでいても TLD の形をしていなければ検索に落とす。
        assert_eq!(
            resolve_address_input("3.14", DEFAULT_SEARCH_URL).as_deref(),
            Some("https://www.google.com/search?q=3.14")
        );
        assert_eq!(resolve_address_input("   ", DEFAULT_SEARCH_URL), None);
    }

    #[test]
    fn bookmark_urls_are_never_turned_into_searches() {
        assert_eq!(
            normalize_bookmark_url("  example.com/path  "),
            "https://example.com/path"
        );
        assert_eq!(
            normalize_bookmark_url("https://github.com/copilot"),
            "https://github.com/copilot"
        );
        // URL に解決できない入力はそのまま残す (検索 URL に化けさせない)。
        assert_eq!(normalize_bookmark_url("  あとで直す  "), "あとで直す");
        assert_eq!(normalize_bookmark_url("   "), "");
    }

    #[test]
    fn queries_are_percent_encoded() {
        assert_eq!(
            build_query_url(DEFAULT_SEARCH_URL, "a&b=c d/e"),
            "https://www.google.com/search?q=a%26b%3Dc%20d%2Fe"
        );
        assert_eq!(
            build_query_url(DEFAULT_SEARCH_URL, "日本語 検索"),
            "https://www.google.com/search?q=%E6%97%A5%E6%9C%AC%E8%AA%9E%20%E6%A4%9C%E7%B4%A2"
        );
    }

    #[test]
    fn templates_without_a_placeholder_are_left_alone() {
        assert!(!template_accepts_query(DEFAULT_COPILOT_URL));
        assert_eq!(
            build_query_url(DEFAULT_COPILOT_URL, "何か"),
            DEFAULT_COPILOT_URL
        );
        assert!(template_accepts_query(DEFAULT_SEARCH_URL));
    }

    #[test]
    fn long_queries_are_truncated() {
        let query = "あ".repeat(MAX_QUERY_CHARS + 50);
        let url = build_query_url("https://example.com/?q={query}", &query);
        // パーセントエンコード後の長さではなく、元の文字数で切り詰める。
        assert_eq!(url.matches("%E3%81%82").count(), MAX_QUERY_CHARS);
    }

    #[test]
    fn settings_fall_back_to_the_fork_defaults() {
        let settings = BrowserSettings::from_settings(&settings::SettingsContent::default());
        assert_eq!(settings.home_url, DEFAULT_HOME_URL);
        assert_eq!(settings.search_url, DEFAULT_SEARCH_URL);
        assert_eq!(settings.copilot_url, DEFAULT_COPILOT_URL);
        assert_eq!(settings.dock, DockPosition::Right);
        assert!(settings.button);
        assert!(!settings.use_egress_proxy);
    }
}
