//! 内蔵ブラウザー (ADR-F19)。
//!
//! Windows では Microsoft Edge WebView2 を GPUI のトップレベル HWND の子として
//! 載せる。人間が URL を打つ操作は AI / ツールのデータ持ち出しとは性質が違うため、
//! 通信は既定で egress 許可リストの対象外 (ADR-F18 の「人間のターミナルは対象外」と
//! 同じ判断)。`browser.use_egress_proxy` を有効にしたときだけ ADR-F14 の loopback
//! プロキシを経由させ、許可リストをブラウジングにも効かせる。
//!
//! 開き方は 2 つある:
//!
//! - 右ドックの [`BrowserPanel`] — ワークスペースに 1 つ。エージェントパネルと
//!   同じドックなので、同時に出せるのはどちらか一方。パネルの中は**タブバー**で、
//!   ここでもページは何枚でも開ける
//! - エディター領域のタブ [`BrowserItem`] — **いくつでも開ける** (`browser_panel::NewBrowserTab`)
//!
//! どちらも中身は [`BrowserView`] で、WebView2 の環境 (`ICoreWebView2Environment`) は
//! プロセス内で 1 つを共有する (user data フォルダーは 1 つの環境しか開けない)。
//! **1 タブ = 1 コントローラー**で、履歴もフォーカスもページごとに独立する。
//! 裏に回ったタブの子 HWND は明示的に隠し (見せるのは描画からの pull)、
//! タブを閉じたら [`BrowserView`] ごと drop してコントローラーを Close する。
//!
//! ブックマークは設定ではなく `browser_bookmarks.json` に置く ([`browser_bookmarks`])。

use std::sync::Arc;

use anyhow::Result;
use editor::Editor;
use gpui::{
    Action, AnyElement, App, AsyncWindowContext, ClickEvent, Context, Entity, EventEmitter,
    FocusHandle, Focusable, Pixels, Subscription, WeakEntity, Window, actions, px,
};
use settings::{IntoGpui, RegisterSetting, Settings};
use ui::{Tooltip, prelude::*};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    searchable::SearchableItem as _,
};

mod browser_bookmarks;
mod browser_item;
mod browser_view;

pub use browser_bookmarks::{
    BookmarkNode, BookmarkPath, BookmarkRow, BookmarkStore, BookmarksFile, default_bookmarks,
    parse_bookmarks_file, serialize_bookmarks_file, should_seed_default_bookmarks,
};
pub use browser_item::BrowserItem;
pub use browser_view::{BrowserHost, BrowserView, BrowserViewEvent};

#[cfg(windows)]
mod webview_windows;

const BROWSER_PANEL_KEY: &str = "BrowserPanel";
const QUERY_PLACEHOLDER: &str = "{query}";

const HIDDEN_TAB_INACTIVE: &str = "the browser tab is not the active tab in the panel";
const HIDDEN_TAB_CLOSED: &str = "the browser tab was closed";
const HIDDEN_PANEL_INACTIVE: &str = "the panel is no longer the active dock panel";

/// パネルのタブ 1 枚分の幅。狭いドックでも 2, 3 枚は並ぶ程度に抑える。
const PANEL_TAB_MIN_WIDTH: f32 = 84.;
const PANEL_TAB_MAX_WIDTH: f32 = 176.;
// OAuth のリダイレクト URL は数千文字になり、ツールチップが WebView の上に
// 画面いっぱいに広がる (2026-09-06 の実機)。タブのツールチップは先頭だけで足りる。
const MAX_TAB_TOOLTIP_CHARS: usize = 160;

/// 検索・質問に渡すテキストの上限。長すぎる選択範囲で URL が壊れるのを防ぐ。
const MAX_QUERY_CHARS: usize = 400;

actions!(
    browser_panel,
    [
        /// 内蔵ブラウザーパネルの表示を切り替えます。
        Toggle,
        /// 内蔵ブラウザーパネルのフォーカスを切り替えます。
        ToggleFocus,
        /// エディター領域に新しいブラウザータブを開きます。
        NewBrowserTab,
        /// ブラウザーパネルに新しいタブを開きます。
        NewPanelTab,
        /// ブラウザーパネルの今のタブを閉じます。
        ClosePanelTab,
        /// ブラウザーパネルで開いているページをエディター領域のタブへ移します。
        OpenInCenter,
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
        /// 今開いているページを「起動時に開くページ」(browser.home_url) にします。
        SetAsHomePage,
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

pub const DEFAULT_HOME_URL: &str = "https://m365.cloud.microsoft/chat";
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

/// タブのツールチップ。長い URL は先頭 [`MAX_TAB_TOOLTIP_CHARS`] 文字で切る。
fn tab_tooltip(url: &str) -> SharedString {
    if url.chars().count() <= MAX_TAB_TOOLTIP_CHARS {
        return url.to_string().into();
    }
    let truncated: String = url.chars().take(MAX_TAB_TOOLTIP_CHARS).collect();
    format!("{truncated}…").into()
}

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

/// タブ 1 枚 = ページ 1 つ = WebView2 コントローラー 1 つ。
struct PanelTab {
    view: Entity<BrowserView>,
    _subscription: Subscription,
}

/// 新しいタブをどう開くか。
#[derive(Clone, Copy, PartialEq, Eq)]
enum NewTabPlacement {
    /// 表に出してフォーカスも移す (「+」ボタンなど、利用者が明示的に開いたとき)。
    Foreground,
    /// 表には出すがフォーカスは動かさない。ドックの復元でパネルが作られたときに
    /// エディターからフォーカスを奪わないため。
    ForegroundWithoutFocus,
    /// 裏で開く (ブックマークの Ctrl+クリック)。WebView2 のコントローラーは
    /// 表に出るまで作られない。
    Background,
}

pub struct BrowserPanel {
    fs: Arc<dyn fs::Fs>,
    workspace: WeakEntity<Workspace>,
    /// タブが 1 枚も無いときに使うフォーカス先。タブがあるときはそのタブの
    /// [`BrowserView`] にフォーカスを渡す。
    focus_handle: FocusHandle,
    tabs: Vec<PanelTab>,
    /// アクティブなタブの位置。`tabs` が空のときは意味を持たない。
    active_tab: usize,
}

/// タブを 1 枚閉じた後にアクティブにするタブ。`tab_count` は閉じる**前**の枚数。
/// アクティブなタブを閉じたときは右隣 (無ければ左隣) を選ぶ。最後の 1 枚を
/// 閉じたら `None` = タブ無しの空状態。
fn active_tab_after_close(tab_count: usize, active: usize, closed: usize) -> Option<usize> {
    if closed >= tab_count {
        // 閉じるタブが無い = 何も動かない。
        return tab_count.checked_sub(1).map(|last| active.min(last));
    }
    let remaining = tab_count - 1;
    if remaining == 0 {
        return None;
    }
    let active = if closed < active { active - 1 } else { active };
    Some(active.min(remaining - 1))
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
            workspace.register_action(|workspace, _: &NewBrowserTab, window, cx| {
                BrowserItem::open(workspace, None, window, cx);
            });
            workspace.register_action(|workspace, _: &OpenInCenter, window, cx| {
                // 「開く」ではなく「移す」。パネル側のタブは畳んで中央ペインへ渡す。
                let url = workspace.panel::<BrowserPanel>(cx).and_then(|panel| {
                    panel.update(cx, |panel, cx| panel.take_active_tab_url(window, cx))
                });
                BrowserItem::open(workspace, url, window, cx);
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
        let mut panel = Self {
            fs: workspace.app_state().fs.clone(),
            workspace: workspace.weak_handle(),
            focus_handle: cx.focus_handle(),
            tabs: Vec::new(),
            active_tab: 0,
        };
        panel.open_tab(None, NewTabPlacement::ForegroundWithoutFocus, window, cx);
        panel
    }

    pub fn active_view(&self) -> Option<&Entity<BrowserView>> {
        self.tabs.get(self.active_tab).map(|tab| &tab.view)
    }

    pub fn current_url(&self, cx: &App) -> String {
        self.active_view()
            .map(|view| view.read(cx).current_url().to_string())
            .unwrap_or_default()
    }

    pub fn navigate_to(&mut self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        match self.active_view() {
            Some(view) => view.update(cx, |view, cx| view.navigate_to(url, cx)),
            None => self.open_tab(
                Some(url.to_string()),
                NewTabPlacement::Foreground,
                window,
                cx,
            ),
        }
    }

    // -- タブ ----------------------------------------------------------------

    /// タブを 1 枚足す。`url` が `None` なら `browser.home_url`、それも空なら
    /// ブックマークを開いた空のタブになる ([`BrowserItem`] と同じ規則)。
    ///
    /// [`NewTabPlacement::Background`] のタブは WebView2 のコントローラーを
    /// **まだ作らない**。生成は [`BrowserView`] の描画から起きるので、そのタブが
    /// 表に出るまで msedgewebview2.exe は 1 つも増えない。
    fn open_tab(
        &mut self,
        url: Option<String>,
        placement: NewTabPlacement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = cx.new(|cx| {
            BrowserView::new(
                self.workspace.clone(),
                self.fs.clone(),
                BrowserHost::Panel,
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
        let subscription = cx.subscribe_in(&view, window, Self::on_view_event);
        self.tabs.push(PanelTab {
            view,
            _subscription: subscription,
        });
        if placement != NewTabPlacement::Background || self.tabs.len() == 1 {
            let index = self.tabs.len() - 1;
            let focus = placement == NewTabPlacement::Foreground;
            self.activate_tab(index, focus, window, cx);
        }
        cx.notify();
    }

    /// `focus` が false でも、既にパネルの中にフォーカスがあるならフォーカスは
    /// 新しいタブへ移る (タブを閉じたときにフォーカスが宙に浮かないように)。
    fn activate_tab(
        &mut self,
        index: usize,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let focus_handle = tab.view.read(cx).focus_handle(cx);
        self.hide_other_surfaces(index, cx);
        self.active_tab = index;
        if focus || self.focus_handle.contains_focused(window, cx) {
            window.focus(&focus_handle, cx);
        }
        cx.notify();
    }

    fn close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        let next_active = active_tab_after_close(self.tabs.len(), self.active_tab, index);
        let tab = self.tabs.remove(index);
        // コントローラーは `WebView` の drop (= `ICoreWebView2Controller::Close`) で
        // 閉じるが、drop が次の描画まで遅れても透明な子 HWND がクリックを飲まない
        // よう、外した時点で隠しておく。
        tab.view
            .update(cx, |view, _| view.hide_surface_from_host(HIDDEN_TAB_CLOSED));
        drop(tab);
        match next_active {
            Some(next_active) => self.activate_tab(next_active, false, window, cx),
            None => self.active_tab = 0,
        }
        cx.notify();
    }

    /// アクティブなタブをパネルから外して URL を返す。「エディター領域で開く」は
    /// コピーではなく**移動**なので、開いた後にパネル側のタブは残さない。
    fn take_active_tab_url(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let index = self.active_tab;
        let url = self
            .tabs
            .get(index)?
            .view
            .read(cx)
            .current_url()
            .trim()
            .to_string();
        if url.is_empty() {
            return None;
        }
        self.close_tab(index, window, cx);
        Some(url)
    }

    /// 表に出ていないタブの子 HWND を隠す。子 HWND は GPUI の描画に従わないので、
    /// 「隠すのは push、見せるのは pull」(次の描画で canvas が矩形を流し直す)。
    fn hide_other_surfaces(&self, active: usize, cx: &mut App) {
        for (index, tab) in self.tabs.iter().enumerate() {
            if index != active {
                tab.view.update(cx, |view, _| {
                    view.hide_surface_from_host(HIDDEN_TAB_INACTIVE)
                });
            }
        }
    }

    fn on_view_event(
        &mut self,
        _: &Entity<BrowserView>,
        event: &BrowserViewEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            BrowserViewEvent::LocationChanged => cx.notify(),
            // ブラウザーと同じく背景タブとして開く (今のページは見えたまま)。
            BrowserViewEvent::OpenInNewTab(url) => {
                self.open_tab(Some(url.clone()), NewTabPlacement::Background, window, cx)
            }
        }
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
            panel.navigate_to(&url, window, cx);
            if !carries_query {
                // Copilot のようにクエリパラメーターが公開されていない宛先では、
                // 貼り付けられる形で渡すのが最も確実。
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(query.clone()));
                if let Some(view) = panel.active_view().cloned() {
                    view.update(cx, |view, cx| {
                        view.set_status_message(
                            "選択テキストをクリップボードにコピーしました。貼り付けて質問してください。",
                            cx,
                        );
                    });
                }
            }
        });
    }
}

impl BrowserPanel {
    /// パネル内のタブバー。子 HWND はこの下の内容領域にしか出ないので、ここは
    /// 普通に GPUI で描いてよい (ページに覆われない)。
    fn render_tab_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let active_tab = self.active_tab;
        let focus_handle = self.focus_handle.clone();
        h_flex()
            .id("browser-panel-tab-bar")
            .w_full()
            .flex_none()
            .gap_px()
            .px_1()
            .pt_1()
            .overflow_x_scroll()
            .bg(cx.theme().colors().tab_bar_background)
            .children(
                self.tabs
                    .iter()
                    .enumerate()
                    .map(|(index, tab)| self.render_tab(index, tab, index == active_tab, cx)),
            )
            .child(
                IconButton::new("browser-panel-new-tab", IconName::Plus)
                    .icon_size(IconSize::XSmall)
                    .tooltip(move |_, cx| {
                        Tooltip::for_action_in("新しいタブ", &NewPanelTab, &focus_handle, cx)
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_tab(None, NewTabPlacement::Foreground, window, cx)
                    })),
            )
            .into_any_element()
    }

    fn render_tab(
        &self,
        index: usize,
        tab: &PanelTab,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let view = tab.view.read(cx);
        let label = view.tab_label();
        let url = view.current_url().trim().to_string();
        let background = if selected {
            cx.theme().colors().tab_active_background
        } else {
            cx.theme().colors().tab_inactive_background
        };

        h_flex()
            .id(("browser-panel-tab", index))
            .flex_none()
            .min_w(px(PANEL_TAB_MIN_WIDTH))
            .max_w(px(PANEL_TAB_MAX_WIDTH))
            .gap_1()
            .pl_1p5()
            .pr_0p5()
            .py_0p5()
            .rounded_t_sm()
            .overflow_hidden()
            .bg(background)
            .cursor_pointer()
            .when(!url.is_empty(), |this| {
                this.tooltip(Tooltip::text(tab_tooltip(&url)))
            })
            .child(
                Icon::new(IconName::ToolWeb)
                    .size(IconSize::XSmall)
                    .color(if selected {
                        Color::Default
                    } else {
                        Color::Muted
                    }),
            )
            // 名前は残り幅を全部使ったうえで切り詰める。`min_w_0` が無いと
            // flex アイテムは最小内容幅より縮まず、長いタイトルでタブが伸びる。
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(label)
                        .size(LabelSize::Small)
                        .color(if selected {
                            Color::Default
                        } else {
                            Color::Muted
                        })
                        .truncate(),
                ),
            )
            .child(
                IconButton::new(("browser-panel-tab-close", index), IconName::Close)
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text("タブを閉じる"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        // タブ本体の on_click まで届くと、閉じた位置に繰り上がった
                        // 別のタブをアクティブにしてしまう。
                        cx.stop_propagation();
                        this.close_tab(index, window, cx);
                    })),
            )
            .on_click(
                cx.listener(move |this, _, window, cx| this.activate_tab(index, true, window, cx)),
            )
            .on_aux_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if event.is_middle_click() {
                    this.close_tab(index, window, cx);
                }
            }))
            .into_any_element()
    }

    fn render_empty_state(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .child(Label::new("開いているページはありません。").color(Color::Muted))
            .child(
                Button::new("browser-panel-empty-new-tab", "新しいタブ").on_click(cx.listener(
                    |this, _, window, cx| {
                        this.open_tab(None, NewTabPlacement::Foreground, window, cx)
                    },
                )),
            )
            .into_any_element()
    }
}

impl Render for BrowserPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active_view = self.active_view().cloned();
        v_flex()
            .key_context(BROWSER_PANEL_KEY)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .on_action(cx.listener(|this, _: &NewPanelTab, window, cx| {
                this.open_tab(None, NewTabPlacement::Foreground, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ClosePanelTab, window, cx| {
                let active_tab = this.active_tab;
                this.close_tab(active_tab, window, cx)
            }))
            .child(self.render_tab_bar(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .map(|this| match active_view {
                        Some(view) => this.child(view),
                        None => this.child(self.render_empty_state(cx)),
                    }),
            )
    }
}

impl Focusable for BrowserPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self.active_view() {
            Some(view) => view.read(cx).focus_handle(cx),
            None => self.focus_handle.clone(),
        }
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
        BrowserSettings::get_global(cx).dock
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
        BrowserSettings::get_global(cx).default_width
    }

    fn icon(&self, _: &Window, cx: &App) -> Option<IconName> {
        BrowserSettings::get_global(cx)
            .button
            .then_some(IconName::ToolWeb)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("ブラウザーパネル")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn set_active(&mut self, active: bool, _: &mut Window, cx: &mut Context<Self>) {
        log::info!("browser panel: set_active({active})");
        if !active {
            for tab in &self.tabs {
                tab.view.update(cx, |view, _| {
                    view.hide_surface_from_host(HIDDEN_PANEL_INACTIVE)
                });
            }
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

/// Windows 以外でビュー本体を素通しでコンパイルするためのスタブ。
/// `new` は必ず失敗し、ビューは「未対応」表示 + 外部ブラウザーへの導線になる。
#[cfg(not(windows))]
#[allow(dead_code)]
pub(crate) mod webview_unsupported {
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
        pub(crate) overlays: Vec<OverlayRect>,
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    pub(crate) struct OverlayRect {
        pub(crate) left: f32,
        pub(crate) top: f32,
        pub(crate) right: f32,
        pub(crate) bottom: f32,
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
    fn tab_tooltip_keeps_short_urls_and_truncates_long_ones() {
        assert_eq!(tab_tooltip("https://example.com/"), "https://example.com/");
        let long = format!("https://login.example.com/{}", "a".repeat(400));
        let tooltip = tab_tooltip(&long);
        assert_eq!(tooltip.chars().count(), MAX_TAB_TOOLTIP_CHARS + 1);
        assert!(tooltip.ends_with('…'));
        assert!(tooltip.starts_with("https://login.example.com/"));
    }

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
    fn closing_an_inactive_tab_keeps_the_active_page() {
        // 左のタブを閉じたら、アクティブなタブは 1 つ手前にずれる。
        assert_eq!(active_tab_after_close(3, 2, 0), Some(1));
        assert_eq!(active_tab_after_close(3, 1, 0), Some(0));
        // 右のタブを閉じてもアクティブな位置は動かない。
        assert_eq!(active_tab_after_close(3, 0, 2), Some(0));
        assert_eq!(active_tab_after_close(3, 0, 1), Some(0));
    }

    #[test]
    fn closing_the_active_tab_selects_its_neighbour() {
        // 右隣を選ぶ (右隣が繰り上がって同じ添字になる)。
        assert_eq!(active_tab_after_close(3, 1, 1), Some(1));
        assert_eq!(active_tab_after_close(3, 0, 0), Some(0));
        // 右端を閉じたときだけ左隣。
        assert_eq!(active_tab_after_close(3, 2, 2), Some(1));
        assert_eq!(active_tab_after_close(2, 1, 1), Some(0));
    }

    #[test]
    fn closing_the_last_tab_leaves_the_panel_empty() {
        assert_eq!(active_tab_after_close(1, 0, 0), None);
        // 空の状態からは何も閉じられない。
        assert_eq!(active_tab_after_close(0, 0, 0), None);
        // 範囲外を指しても今のタブは動かない。
        assert_eq!(active_tab_after_close(2, 1, 5), Some(1));
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
