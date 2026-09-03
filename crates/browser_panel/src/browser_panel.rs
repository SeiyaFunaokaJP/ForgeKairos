//! 内蔵ブラウザーパネル (ADR-F19)。
//!
//! Windows では Microsoft Edge WebView2 を GPUI のトップレベル HWND の子として
//! 載せる。人間が URL を打つ操作は AI / ツールのデータ持ち出しとは性質が違うため、
//! 通信は既定で egress 許可リストの対象外 (ADR-F18 の「人間のターミナルは対象外」と
//! 同じ判断)。`browser.use_egress_proxy` を有効にしたときだけ ADR-F14 の loopback
//! プロキシを経由させ、許可リストをブラウジングにも効かせる。

use std::{rc::Rc, sync::Arc};

use anyhow::Result;
use editor::Editor;
use futures::StreamExt as _;
use gpui::{
    Action, AnyElement, App, AsyncWindowContext, Context, DispatchPhase, Entity, EventEmitter,
    FocusHandle, Focusable, MouseButton, MouseDownEvent, Pixels, SharedString, Subscription, Task,
    WeakEntity, Window, actions, canvas, px,
};
use settings::{IntoGpui, RegisterSetting, Settings, SettingsStore};
use ui::{Tooltip, prelude::*};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    searchable::SearchableItem as _,
};

#[cfg(windows)]
mod webview_windows;
#[cfg(windows)]
use webview_windows::{WebView, WebViewEvent};

#[cfg(not(windows))]
use webview_unsupported::{WebView, WebViewEvent};

const BROWSER_PANEL_KEY: &str = "BrowserPanel";
const WEBVIEW2_DOWNLOAD_URL: &str = "https://developer.microsoft.com/microsoft-edge/webview2/";
const QUERY_PLACEHOLDER: &str = "{query}";

/// 検索・質問に渡すテキストの上限。長すぎる選択範囲で URL が壊れるのを防ぐ。
const MAX_QUERY_CHARS: usize = 400;

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
    ]
);

// エディターの右クリックメニューから参照できるよう、選択テキストを渡す 2 つだけは
// `zed_actions` 側に置いてある (`editor` は `browser_panel` に依存できないため)。
pub use zed_actions::browser::{AskCopilot, SearchSelection};

// ---------------------------------------------------------------------------
// 設定
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserBookmark {
    pub name: SharedString,
    pub url: SharedString,
}

#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct BrowserSettings {
    pub button: bool,
    pub dock: DockPosition,
    pub default_width: Pixels,
    pub home_url: String,
    pub search_url: String,
    pub copilot_url: String,
    pub bookmarks: Vec<BrowserBookmark>,
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
            bookmarks: browser
                .bookmarks
                .map(|bookmarks| {
                    bookmarks
                        .into_iter()
                        .filter(|bookmark| !bookmark.url.trim().is_empty())
                        .map(|bookmark| BrowserBookmark {
                            name: bookmark.name.into(),
                            url: bookmark.url.into(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
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

pub struct BrowserPanel {
    workspace: WeakEntity<Workspace>,
    fs: Arc<dyn fs::Fs>,
    focus_handle: FocusHandle,
    address_editor: Entity<Editor>,
    webview: Option<Rc<WebView>>,
    surface: SurfaceState,
    current_url: String,
    page_title: SharedString,
    status_message: Option<SharedString>,
    can_go_back: bool,
    can_go_forward: bool,
    loading: bool,
    page_has_focus: bool,
    active: bool,
    applied_dark_mode: Option<bool>,
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

        let mut subscriptions = Vec::new();
        subscriptions.push(cx.observe_global::<SettingsStore>(|this, cx| {
            this.sync_appearance(cx);
            cx.notify();
        }));

        Self {
            workspace: workspace.weak_handle(),
            fs: workspace.app_state().fs.clone(),
            focus_handle: cx.focus_handle(),
            address_editor,
            webview: None,
            surface: SurfaceState::Idle,
            current_url: String::new(),
            page_title: SharedString::default(),
            status_message: None,
            can_go_back: false,
            can_go_forward: false,
            loading: false,
            page_has_focus: false,
            active: false,
            applied_dark_mode: None,
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
            self.surface = SurfaceState::Unavailable {
                message: "ウィンドウハンドルを取得できなかったため、内蔵ブラウザーを開始できませんでした。".into(),
                offer_runtime_download: false,
            };
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
    fn surface_should_be_visible(&self, window: &mut Window, cx: &mut App) -> bool {
        if !self.active || !window.is_window_active() || cx.has_active_drag() {
            return false;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return false;
        };
        !workspace.update(cx, |workspace, cx| workspace.has_active_modal(window, cx))
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

    fn render_bookmarks(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let bookmarks = Self::settings(cx).bookmarks.clone();
        if bookmarks.is_empty() {
            return None;
        }
        Some(
            h_flex()
                .w_full()
                .flex_wrap()
                .gap_1()
                .px_1()
                .pb_1()
                .children(bookmarks.into_iter().enumerate().map(|(index, bookmark)| {
                    let url = bookmark.url.clone();
                    Button::new(("browser-bookmark", index), bookmark.name.clone())
                        .label_size(LabelSize::Small)
                        .start_icon(
                            Icon::new(IconName::Bookmark)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .tooltip(Tooltip::text(bookmark.url))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.navigate_to(&url, cx);
                        }))
                }))
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
                let visible = self.surface_should_be_visible(window, cx);
                let webview = self.webview.clone();
                let starting = matches!(self.surface, SurfaceState::Starting);
                div()
                    .relative()
                    .size_full()
                    .bg(cx.theme().colors().editor_background)
                    // 生成中の案内は子 HWND が載る領域と同じ場所に重ねる。
                    .when(starting, |this| {
                        this.child(
                            v_flex()
                                .absolute()
                                .size_full()
                                .items_center()
                                .justify_center()
                                .child(
                                    Label::new("内蔵ブラウザーを起動しています…")
                                        .color(Color::Muted),
                                ),
                        )
                    })
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, _, window, _| {
                                let Some(webview) = webview else {
                                    return;
                                };
                                webview.sync_viewport(
                                    f32::from(bounds.origin.x),
                                    f32::from(bounds.origin.y),
                                    f32::from(bounds.size.width),
                                    f32::from(bounds.size.height),
                                    window.scale_factor(),
                                    visible,
                                );
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

impl Render for BrowserPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    if let Some(webview) = &this.webview {
                        webview.release_keyboard_focus();
                    }
                }),
            )
            .child(self.render_toolbar(cx))
            .children(self.render_bookmarks(cx))
            .child(
                h_flex().w_full().px_2().pb_1().child(
                    Label::new(status)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_surface(window, cx)),
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
        self.active = active;
        if active {
            self.ensure_surface(window, cx);
        } else if let Some(webview) = &self.webview {
            webview.hide();
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

        pub(crate) fn sync_viewport(&self, _: f32, _: f32, _: f32, _: f32, _: f32, _: bool) {}
        pub(crate) fn hide(&self) {}
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
        assert!(settings.bookmarks.is_empty());
    }

    #[test]
    fn bookmarks_without_a_url_are_dropped() {
        let mut content = settings::SettingsContent::default();
        content.browser = Some(settings::BrowserSettingsContent {
            bookmarks: Some(vec![
                settings::BrowserBookmarkContent {
                    name: "GitHub Copilot".into(),
                    url: "https://github.com/copilot".into(),
                },
                settings::BrowserBookmarkContent {
                    name: "壊れた項目".into(),
                    url: "  ".into(),
                },
            ]),
            ..Default::default()
        });

        let settings = BrowserSettings::from_settings(&content);
        assert_eq!(settings.bookmarks.len(), 1);
        assert_eq!(settings.bookmarks[0].name.as_ref(), "GitHub Copilot");
    }
}
