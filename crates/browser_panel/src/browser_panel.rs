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
    Action, AnyElement, App, AsyncWindowContext, ClickEvent, Context, DragMoveEvent, Empty, Entity,
    EventEmitter, FocusHandle, Focusable, MouseButton, Pixels, Subscription, WeakEntity, Window,
    actions, px, relative,
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

/// 分割の境界のつまみの幅。境界の線そのものは 1px なので、掴める幅を別に取る。
const PANE_DIVIDER_HANDLE_WIDTH: f32 = 8.;
/// 内容領域のうち「ここへ落としたら分割」になる左右の端の割合。
const SPLIT_EDGE_FRACTION: f32 = 0.25;
/// 分割したグループ 1 つあたりの取り分の下限。境界を端まで引いても潰れないように。
const MIN_GROUP_FLEX: f32 = 0.15;

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
        /// 今のタブを右に切り出して、ブラウザーパネルを左右に分割します。
        SplitPanelRight,
        /// 分割したブラウザーパネルの次のペインにフォーカスを移します。
        FocusNextPaneGroup,
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

/// 横に並ぶペイン 1 枚。中はタブ列で、表に出るのは `active` の 1 枚だけ。
/// 入れ子の分割は持たない (ドックの幅で意味があるのは左右 2, 3 枚まで)。
struct BrowserPaneGroup {
    tabs: Vec<PanelTab>,
    /// 表に出ているタブの位置。空になったグループは畳むので常に有効な添字。
    active: usize,
    /// 横幅の取り分。全グループの合計が 1.0 になるよう正規化して持つ。
    flex: f32,
}

/// パネルのタブをドラッグしている間の持ち物。ワークスペースの `DraggedTab` とは
/// 別の型にしてある (エディターのペインに落とせてしまわないように)。
#[derive(Clone)]
struct DraggedPanelTab {
    group: usize,
    index: usize,
    label: SharedString,
}

impl Render for DraggedPanelTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .px_1p5()
            .py_0p5()
            .max_w(px(PANEL_TAB_MAX_WIDTH))
            .overflow_hidden()
            .rounded_sm()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().tab_active_background)
            .child(
                Label::new(self.label.clone())
                    .size(LabelSize::Small)
                    .truncate(),
            )
    }
}

/// 分割の境界 (グループ `index` と `index + 1` の間) をつまんでいる。
#[derive(Clone, Copy)]
struct DraggedPaneDivider {
    index: usize,
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
    /// 左から右へ並ぶペイン。空ならタブが 1 枚も無い状態。
    groups: Vec<BrowserPaneGroup>,
    /// 操作の対象になるグループ。`groups` が空のときは意味を持たない。
    active_group: usize,
}

/// タブを 1 枚閉じた後にアクティブにするタブ。`tab_count` は閉じる**前**の枚数。
/// アクティブなタブを閉じたときは右隣 (無ければ左隣) を選ぶ。最後の 1 枚を
/// 閉じたら `None` = タブ無しの空状態。グループを 1 つ畳んだ後の
/// [`BrowserPanel::active_group`] も同じ規則で繰り上げる。
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

/// 同じタブ列の中で `from` のタブを「`to` の手前」へ動かした後の添字。
/// `to` は取り除く**前**の添字で数えるので、後ろを指したときは 1 つ補正する。
fn moved_tab_index(from: usize, to: usize) -> usize {
    if to > from { to - 1 } else { to }
}

/// 取り分は合計 1.0 で持つ。壊れた値 (NaN や 0 以下) が混じっても均等割りに落とす。
fn normalized_flexes(flexes: &[f32]) -> Vec<f32> {
    if flexes.is_empty() {
        return Vec::new();
    }
    let sanitized: Vec<f32> = flexes
        .iter()
        .map(|flex| {
            if flex.is_finite() && *flex > 0. {
                *flex
            } else {
                MIN_GROUP_FLEX
            }
        })
        .collect();
    let total: f32 = sanitized.iter().sum();
    if total <= 0. {
        return vec![1. / sanitized.len() as f32; sanitized.len()];
    }
    sanitized.iter().map(|flex| flex / total).collect()
}

/// 境界 `divider` (グループ `divider` と `divider + 1` の間) を、パネル全体に対する
/// 割合 `ratio` の位置へ動かす。動くのは両隣の 2 つだけで、合計は変えない。
fn flexes_after_divider_drag(flexes: &[f32], divider: usize, ratio: f32) -> Vec<f32> {
    let mut flexes = normalized_flexes(flexes);
    let (Some(left), Some(right)) = (
        flexes.get(divider).copied(),
        flexes.get(divider + 1).copied(),
    ) else {
        return flexes;
    };
    if !ratio.is_finite() {
        return flexes;
    }
    let pair = left + right;
    let start: f32 = flexes.iter().take(divider).sum();
    // ペアの取り分が下限の 2 倍より狭くても `clamp` の前提 (min <= max) を壊さない。
    let low = MIN_GROUP_FLEX.min(pair / 2.);
    let high = (pair - MIN_GROUP_FLEX).max(pair / 2.);
    let new_left = (ratio - start).clamp(low, high);
    if let Some(flex) = flexes.get_mut(divider) {
        *flex = new_left;
    }
    if let Some(flex) = flexes.get_mut(divider + 1) {
        *flex = pair - new_left;
    }
    flexes
}

/// グループを 1 つ畳んだ後の取り分。空いた分は右隣、無ければ左隣に渡す。
fn flexes_after_group_removal(flexes: &[f32], removed: usize) -> Vec<f32> {
    let mut flexes = flexes.to_vec();
    if removed >= flexes.len() {
        return normalized_flexes(&flexes);
    }
    let freed = flexes.remove(removed);
    let neighbour = if removed < flexes.len() {
        Some(removed)
    } else {
        removed.checked_sub(1)
    };
    if let Some(neighbour) = neighbour
        && let Some(flex) = flexes.get_mut(neighbour)
    {
        *flex += freed;
    }
    normalized_flexes(&flexes)
}

/// タブを切り出して `at` に新しいグループを作った後の取り分。
/// 切り出し元の取り分を新旧で半分ずつ分ける。
fn flexes_after_split(flexes: &[f32], source: usize, at: usize) -> Vec<f32> {
    let mut flexes = flexes.to_vec();
    let share = flexes
        .get(source)
        .copied()
        .filter(|flex| flex.is_finite() && *flex > 0.)
        .unwrap_or(1.)
        / 2.;
    if let Some(flex) = flexes.get_mut(source) {
        *flex = share;
    }
    let at = at.min(flexes.len());
    flexes.insert(at, share);
    normalized_flexes(&flexes)
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
            groups: Vec::new(),
            active_group: 0,
        };
        panel.open_tab(None, NewTabPlacement::ForegroundWithoutFocus, window, cx);
        panel
    }

    fn current_group(&self) -> Option<&BrowserPaneGroup> {
        self.groups.get(self.active_group)
    }

    pub fn active_view(&self) -> Option<&Entity<BrowserView>> {
        let group = self.current_group()?;
        group.tabs.get(group.active).map(|tab| &tab.view)
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
        let group = self.active_group;
        self.open_tab_in_group(group, url, placement, window, cx);
    }

    fn open_tab_in_group(
        &mut self,
        group: usize,
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
        if self.groups.is_empty() {
            self.groups.push(BrowserPaneGroup {
                tabs: Vec::new(),
                active: 0,
                flex: 1.,
            });
        }
        let group = group.min(self.groups.len() - 1);
        let Some(pane) = self.groups.get_mut(group) else {
            return;
        };
        pane.tabs.push(PanelTab {
            view,
            _subscription: subscription,
        });
        let index = pane.tabs.len() - 1;
        let is_first_tab = pane.tabs.len() == 1;
        if placement != NewTabPlacement::Background || is_first_tab {
            let focus = placement == NewTabPlacement::Foreground;
            self.activate_tab(group, index, focus, window, cx);
        }
        cx.notify();
    }

    /// `focus` が false でも、既にパネルの中にフォーカスがあるならフォーカスは
    /// 新しいタブへ移る (タブを閉じたときにフォーカスが宙に浮かないように)。
    fn activate_tab(
        &mut self,
        group: usize,
        index: usize,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(focus_handle) = self
            .groups
            .get(group)
            .and_then(|pane| pane.tabs.get(index))
            .map(|tab| tab.view.read(cx).focus_handle(cx))
        else {
            return;
        };
        if let Some(pane) = self.groups.get_mut(group) {
            pane.active = index;
        }
        self.active_group = group;
        self.hide_inactive_surfaces(cx);
        if focus || self.focus_handle.contains_focused(window, cx) {
            window.focus(&focus_handle, cx);
        }
        cx.notify();
    }

    fn close_tab(
        &mut self,
        group: usize,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.groups.get_mut(group) else {
            return;
        };
        if index >= pane.tabs.len() {
            return;
        }
        let next_active = active_tab_after_close(pane.tabs.len(), pane.active, index);
        let tab = pane.tabs.remove(index);
        if let Some(next_active) = next_active {
            pane.active = next_active;
        }
        // コントローラーは `WebView` の drop (= `ICoreWebView2Controller::Close`) で
        // 閉じるが、drop が次の描画まで遅れても透明な子 HWND がクリックを飲まない
        // よう、外した時点で隠しておく。
        tab.view
            .update(cx, |view, _| view.hide_surface_from_host(HIDDEN_TAB_CLOSED));
        drop(tab);
        if next_active.is_none() {
            self.remove_group(group);
        }
        match self.current_group().map(|pane| pane.active) {
            Some(active) => {
                let group = self.active_group;
                self.activate_tab(group, active, false, window, cx);
            }
            None => self.active_group = 0,
        }
        cx.notify();
    }

    /// 空になったグループを畳む。取り分は隣に渡す。
    fn remove_group(&mut self, index: usize) {
        if index >= self.groups.len() {
            return;
        }
        let flexes = flexes_after_group_removal(&self.flexes(), index);
        self.groups.remove(index);
        self.apply_flexes(&flexes);
        self.active_group =
            active_tab_after_close(self.groups.len() + 1, self.active_group, index).unwrap_or(0);
    }

    fn flexes(&self) -> Vec<f32> {
        self.groups.iter().map(|group| group.flex).collect()
    }

    fn apply_flexes(&mut self, flexes: &[f32]) {
        for (group, flex) in self.groups.iter_mut().zip(flexes.iter().copied()) {
            group.flex = flex;
        }
    }

    /// タブを別の位置へ動かす。`to_index` は移動先のタブ列での**挿入位置**。
    fn move_tab(
        &mut self,
        from_group: usize,
        from_index: usize,
        to_group: usize,
        to_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if from_group >= self.groups.len() || to_group >= self.groups.len() {
            return;
        }
        if from_group == to_group {
            let Some(pane) = self.groups.get_mut(from_group) else {
                return;
            };
            if from_index >= pane.tabs.len() {
                return;
            }
            let target = moved_tab_index(from_index, to_index.min(pane.tabs.len()));
            if target != from_index {
                let tab = pane.tabs.remove(from_index);
                pane.tabs.insert(target, tab);
            }
            self.activate_tab(from_group, target, true, window, cx);
            return;
        }

        let Some(source) = self.groups.get(from_group) else {
            return;
        };
        if from_index >= source.tabs.len() {
            return;
        }
        let next_source_active =
            active_tab_after_close(source.tabs.len(), source.active, from_index);
        let Some(tab) = self
            .groups
            .get_mut(from_group)
            .map(|pane| pane.tabs.remove(from_index))
        else {
            return;
        };
        if let Some(active) = next_source_active
            && let Some(pane) = self.groups.get_mut(from_group)
        {
            pane.active = active;
        }
        let Some(target_pane) = self.groups.get_mut(to_group) else {
            return;
        };
        let target = to_index.min(target_pane.tabs.len());
        target_pane.tabs.insert(target, tab);

        let mut to_group = to_group;
        if next_source_active.is_none() {
            self.remove_group(from_group);
            if from_group < to_group {
                to_group -= 1;
            }
        }
        self.activate_tab(to_group, target, true, window, cx);
    }

    /// タブを新しいグループへ切り出して左右に分割する。`at` は新しいグループを
    /// 挿入する位置 (グループの並びでの添字)。
    fn split_tab_into_group(
        &mut self,
        from_group: usize,
        from_index: usize,
        at: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(source) = self.groups.get(from_group) else {
            return;
        };
        if from_index >= source.tabs.len() {
            return;
        }
        let at = at.min(self.groups.len());
        if source.tabs.len() == 1 && (at == from_group || at == from_group + 1) {
            // 1 枚しかないグループを自分の隣へ切り出しても並びは変わらない。
            return;
        }
        let next_source_active =
            active_tab_after_close(source.tabs.len(), source.active, from_index);
        let flexes = flexes_after_split(&self.flexes(), from_group, at);
        let Some(tab) = self
            .groups
            .get_mut(from_group)
            .map(|pane| pane.tabs.remove(from_index))
        else {
            return;
        };
        if let Some(active) = next_source_active
            && let Some(pane) = self.groups.get_mut(from_group)
        {
            pane.active = active;
        }
        self.groups.insert(
            at,
            BrowserPaneGroup {
                tabs: vec![tab],
                active: 0,
                flex: 0.,
            },
        );
        self.apply_flexes(&flexes);

        let mut target = at;
        if next_source_active.is_none() {
            // 挿入で 1 つ後ろにずれた切り出し元を畳む。
            let source_after_insert = if at <= from_group {
                from_group + 1
            } else {
                from_group
            };
            self.remove_group(source_after_insert);
            if source_after_insert < target {
                target -= 1;
            }
        }
        self.activate_tab(target, 0, true, window, cx);
    }

    fn split_active_tab_right(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let group = self.active_group;
        let Some(pane) = self.current_group() else {
            return;
        };
        let index = pane.active;
        if pane.tabs.len() > 1 {
            self.split_tab_into_group(group, index, group + 1, window, cx);
            return;
        }
        // 1 枚しかないグループを切り出しても並びは変わらないので、同じページを
        // もう 1 枚開いてから右へ出す (ブラウザーの「タブの複製」と同じ)。
        let url = pane
            .tabs
            .get(index)
            .map(|tab| tab.view.read(cx).current_url().trim().to_string())
            .filter(|url| !url.is_empty());
        self.open_tab_in_group(group, url, NewTabPlacement::Foreground, window, cx);
        let Some(duplicated) = self
            .groups
            .get(group)
            .and_then(|pane| pane.tabs.len().checked_sub(1))
        else {
            return;
        };
        self.split_tab_into_group(group, duplicated, group + 1, window, cx);
    }

    fn focus_next_group(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.groups.len() < 2 {
            return;
        }
        let group = (self.active_group + 1) % self.groups.len();
        let Some(index) = self.groups.get(group).map(|pane| pane.active) else {
            return;
        };
        self.activate_tab(group, index, true, window, cx);
    }

    fn drag_divider(&mut self, event: &DragMoveEvent<DraggedPaneDivider>, cx: &mut Context<Self>) {
        let Some(divider) = event
            .dragged_item()
            .downcast_ref::<DraggedPaneDivider>()
            .map(|divider| divider.index)
        else {
            return;
        };
        let width = event.bounds.size.width;
        if width <= px(0.) {
            return;
        }
        let ratio = (event.event.position.x - event.bounds.left()) / width;
        let flexes = flexes_after_divider_drag(&self.flexes(), divider, ratio);
        self.apply_flexes(&flexes);
        cx.notify();
    }

    /// アクティブなタブをパネルから外して URL を返す。「エディター領域で開く」は
    /// コピーではなく**移動**なので、開いた後にパネル側のタブは残さない。
    fn take_active_tab_url(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let group = self.active_group;
        let index = self.current_group()?.active;
        let url = self
            .groups
            .get(group)?
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
        self.close_tab(group, index, window, cx);
        Some(url)
    }

    /// 表に出ていないタブの子 HWND を隠す。子 HWND は GPUI の描画に従わないので、
    /// 「隠すのは push、見せるのは pull」(次の描画で canvas が矩形を流し直す)。
    /// 左右に分割しているときは**グループごとに 1 枚**が表に出る。
    fn hide_inactive_surfaces(&self, cx: &mut App) {
        for pane in &self.groups {
            for (index, tab) in pane.tabs.iter().enumerate() {
                if index != pane.active {
                    tab.view.update(cx, |view, _| {
                        view.hide_surface_from_host(HIDDEN_TAB_INACTIVE)
                    });
                }
            }
        }
    }

    fn locate_view(&self, view: &Entity<BrowserView>) -> Option<(usize, usize)> {
        self.groups.iter().enumerate().find_map(|(group, pane)| {
            pane.tabs
                .iter()
                .position(|tab| tab.view.entity_id() == view.entity_id())
                .map(|index| (group, index))
        })
    }

    fn on_view_event(
        &mut self,
        view: &Entity<BrowserView>,
        event: &BrowserViewEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            BrowserViewEvent::LocationChanged => cx.notify(),
            // ブラウザーと同じく背景タブとして開く (今のページは見えたまま)。
            // 分割しているときは、そのページを出しているグループの中に開く。
            BrowserViewEvent::OpenInNewTab(url) => {
                let group = self
                    .locate_view(view)
                    .map_or(self.active_group, |(group, _)| group);
                self.open_tab_in_group(
                    group,
                    Some(url.clone()),
                    NewTabPlacement::Background,
                    window,
                    cx,
                )
            }
            // ページ側のクリックは子 HWND が飲むので GPUI には届かない。どのペインを
            // 触っているかはビューからの申告でしか分からない。
            BrowserViewEvent::Focused => {
                if let Some((group, _)) = self.locate_view(view)
                    && self.active_group != group
                {
                    self.active_group = group;
                    cx.notify();
                }
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
    /// 左右に並べたグループ。境界はドラッグで動かせる。
    fn render_groups(&self, cx: &mut Context<Self>) -> AnyElement {
        let last = self.groups.len().saturating_sub(1);
        let mut row = h_flex()
            .id("browser-panel-groups")
            .size_full()
            .min_h_0()
            .on_drag_move::<DraggedPaneDivider>(cx.listener(
                |this, event: &DragMoveEvent<DraggedPaneDivider>, _, cx| {
                    this.drag_divider(event, cx)
                },
            ));
        for (index, group) in self.groups.iter().enumerate() {
            row = row.child(self.render_group(index, group, cx));
            if index < last {
                row = row.child(self.render_divider(index, cx));
            }
        }
        row.into_any_element()
    }

    fn render_group(
        &self,
        index: usize,
        group: &BrowserPaneGroup,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active_view = group.tabs.get(group.active).map(|tab| tab.view.clone());
        let is_split = self.groups.len() > 1;
        v_flex()
            .id(("browser-panel-group", index))
            .flex_shrink_1()
            .min_w_0()
            .h_full()
            .flex_basis(relative(group.flex))
            .when(is_split, |this| {
                // 子 HWND はクリックを飲むので、ページ側を触ったときは
                // `BrowserViewEvent::Focused` の方で拾う。
                this.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        if this.active_group != index {
                            this.active_group = index;
                            cx.notify();
                        }
                    }),
                )
            })
            .child(self.render_tab_bar(index, group, cx))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .children(active_view)
                    .children(self.render_drop_targets(index, cx)),
            )
            .into_any_element()
    }

    /// ドラッグ中だけ内容領域に敷く落とし先。左右の端はそこで分割、真ん中は
    /// このグループへ移動。ドラッグ中は WebView が隠れているので手前に見える
    /// (`BrowserView::surface_hidden_reason`)。
    fn render_drop_targets(&self, index: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !cx.has_active_drag() {
            return None;
        }
        let drop_background = cx.theme().colors().drop_target_background;
        let drop_border = cx.theme().colors().drop_target_border;
        Some(
            h_flex()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .child(
                    div()
                        .id(("browser-panel-split-before", index))
                        .h_full()
                        .w(relative(SPLIT_EDGE_FRACTION))
                        .border_color(drop_border)
                        .drag_over::<DraggedPanelTab>(move |style, _, _, _| {
                            style.bg(drop_background).border_l_2()
                        })
                        .on_drop(cx.listener(
                            move |this, dragged: &DraggedPanelTab, window, cx| {
                                this.split_tab_into_group(
                                    dragged.group,
                                    dragged.index,
                                    index,
                                    window,
                                    cx,
                                )
                            },
                        )),
                )
                .child(
                    div()
                        .id(("browser-panel-drop-into", index))
                        .h_full()
                        .flex_1()
                        .drag_over::<DraggedPanelTab>(move |style, _, _, _| {
                            style.bg(drop_background)
                        })
                        .on_drop(cx.listener(
                            move |this, dragged: &DraggedPanelTab, window, cx| {
                                let to_index =
                                    this.groups.get(index).map_or(0, |pane| pane.tabs.len());
                                this.move_tab(
                                    dragged.group,
                                    dragged.index,
                                    index,
                                    to_index,
                                    window,
                                    cx,
                                )
                            },
                        )),
                )
                .child(
                    div()
                        .id(("browser-panel-split-after", index))
                        .h_full()
                        .w(relative(SPLIT_EDGE_FRACTION))
                        .border_color(drop_border)
                        .drag_over::<DraggedPanelTab>(move |style, _, _, _| {
                            style.bg(drop_background).border_r_2()
                        })
                        .on_drop(cx.listener(
                            move |this, dragged: &DraggedPanelTab, window, cx| {
                                this.split_tab_into_group(
                                    dragged.group,
                                    dragged.index,
                                    index + 1,
                                    window,
                                    cx,
                                )
                            },
                        )),
                )
                .into_any_element(),
        )
    }

    /// グループの境界。1px の線の上に、掴める幅のつまみを重ねてある。
    fn render_divider(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id(("browser-panel-divider", index))
            .relative()
            .h_full()
            .flex_none()
            .w(px(1.))
            .bg(cx.theme().colors().border)
            .child(
                div()
                    .id(("browser-panel-divider-handle", index))
                    .absolute()
                    .top_0()
                    .left(px(-PANE_DIVIDER_HANDLE_WIDTH / 2.))
                    .w(px(PANE_DIVIDER_HANDLE_WIDTH))
                    .h_full()
                    .cursor_col_resize()
                    .block_mouse_except_scroll()
                    .on_drag(DraggedPaneDivider { index }, |_, _, _, cx| {
                        cx.new(|_| Empty)
                    }),
            )
            .into_any_element()
    }

    /// パネル内のタブバー。子 HWND はこの下の内容領域にしか出ないので、ここは
    /// 普通に GPUI で描いてよい (ページに覆われない)。
    fn render_tab_bar(
        &self,
        group_index: usize,
        group: &BrowserPaneGroup,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let focus_handle = self.focus_handle.clone();
        let tab_count = group.tabs.len();
        h_flex()
            .id(("browser-panel-tab-bar", group_index))
            .w_full()
            .flex_none()
            .gap_px()
            .px_1()
            .pt_1()
            .overflow_x_scroll()
            .bg(cx.theme().colors().tab_bar_background)
            .children(group.tabs.iter().enumerate().map(|(index, tab)| {
                self.render_tab(group_index, index, tab, index == group.active, cx)
            }))
            .child(
                IconButton::new(("browser-panel-new-tab", group_index), IconName::Plus)
                    .icon_size(IconSize::XSmall)
                    .tooltip(move |_, cx| {
                        Tooltip::for_action_in("新しいタブ", &NewPanelTab, &focus_handle, cx)
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_tab_in_group(
                            group_index,
                            None,
                            NewTabPlacement::Foreground,
                            window,
                            cx,
                        )
                    })),
            )
            // タブの無い余白に落としたら、このグループの末尾へ移す。
            .drag_over::<DraggedPanelTab>(|style, _, _, cx| {
                style.bg(cx.theme().colors().drop_target_background)
            })
            .on_drop(
                cx.listener(move |this, dragged: &DraggedPanelTab, window, cx| {
                    this.move_tab(
                        dragged.group,
                        dragged.index,
                        group_index,
                        tab_count,
                        window,
                        cx,
                    )
                }),
            )
            .into_any_element()
    }

    fn render_tab(
        &self,
        group_index: usize,
        index: usize,
        tab: &PanelTab,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity_id = tab.view.entity_id();
        let view = tab.view.read(cx);
        let label = view.tab_label();
        let url = view.current_url().trim().to_string();
        let background = if selected {
            cx.theme().colors().tab_active_background
        } else {
            cx.theme().colors().tab_inactive_background
        };

        h_flex()
            .id(("browser-panel-tab", entity_id))
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
                    Label::new(label.clone())
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
                IconButton::new(("browser-panel-tab-close", entity_id), IconName::Close)
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text("タブを閉じる"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        // タブ本体の on_click まで届くと、閉じた位置に繰り上がった
                        // 別のタブをアクティブにしてしまう。
                        cx.stop_propagation();
                        this.close_tab(group_index, index, window, cx);
                    })),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.activate_tab(group_index, index, true, window, cx)
            }))
            .on_aux_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if event.is_middle_click() {
                    this.close_tab(group_index, index, window, cx);
                }
            }))
            .on_drag(
                DraggedPanelTab {
                    group: group_index,
                    index,
                    label,
                },
                |tab, _, _, cx| cx.new(|_| tab.clone()),
            )
            .drag_over::<DraggedPanelTab>(move |style, dragged: &DraggedPanelTab, _, cx| {
                let style = style
                    .bg(cx.theme().colors().drop_target_background)
                    .border_color(cx.theme().colors().drop_target_border);
                // 同じ列の中なら、来る向きに合わせて落ちる側に線を出す。
                if dragged.group != group_index || index < dragged.index {
                    style.border_l_2()
                } else {
                    style.border_r_2()
                }
            })
            .on_drop(
                cx.listener(move |this, dragged: &DraggedPanelTab, window, cx| {
                    // タブバー全体の落とし先 (末尾へ移動) まで届かせない。
                    cx.stop_propagation();
                    // 「このタブの位置に入る」= 元が前にあるなら 1 つ後ろの挿入位置。
                    let to_index = if dragged.group == group_index && dragged.index < index {
                        index + 1
                    } else {
                        index
                    };
                    this.move_tab(
                        dragged.group,
                        dragged.index,
                        group_index,
                        to_index,
                        window,
                        cx,
                    )
                }),
            )
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
        let content = if self.groups.is_empty() {
            self.render_empty_state(cx)
        } else {
            self.render_groups(cx)
        };
        v_flex()
            .key_context(BROWSER_PANEL_KEY)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .on_action(cx.listener(|this, _: &NewPanelTab, window, cx| {
                this.open_tab(None, NewTabPlacement::Foreground, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ClosePanelTab, window, cx| {
                let group = this.active_group;
                let Some(index) = this.current_group().map(|pane| pane.active) else {
                    return;
                };
                this.close_tab(group, index, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SplitPanelRight, window, cx| {
                this.split_active_tab_right(window, cx)
            }))
            .on_action(cx.listener(|this, _: &FocusNextPaneGroup, window, cx| {
                this.focus_next_group(window, cx)
            }))
            .child(div().flex_1().min_h_0().w_full().child(content))
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
            for pane in &self.groups {
                for tab in &pane.tabs {
                    tab.view.update(cx, |view, _| {
                        view.hide_surface_from_host(HIDDEN_PANEL_INACTIVE)
                    });
                }
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

    /// 取り分の比較は丸めてから。正規化の割り算で最下位の桁がずれるため。
    fn rounded(flexes: &[f32]) -> Vec<f32> {
        flexes
            .iter()
            .map(|flex| (flex * 1000.).round() / 1000.)
            .collect()
    }

    #[test]
    fn dropping_a_tab_uses_insertion_positions() {
        // 後ろへ落とすときは、抜けた分だけ 1 つ手前に入る。
        assert_eq!(moved_tab_index(0, 3), 2);
        assert_eq!(moved_tab_index(0, 1), 0);
        // 前へ落とすときは挿入位置そのもの。
        assert_eq!(moved_tab_index(2, 0), 0);
        assert_eq!(moved_tab_index(2, 2), 2);
    }

    #[test]
    fn group_flexes_always_add_up_to_one() {
        assert_eq!(rounded(&normalized_flexes(&[1., 1.])), [0.5, 0.5]);
        assert_eq!(rounded(&normalized_flexes(&[3., 1.])), [0.75, 0.25]);
        assert!(normalized_flexes(&[]).is_empty());
        // 壊れた値 (0 以下・NaN・無限大) が混じっても割り算が壊れない。
        assert_eq!(rounded(&normalized_flexes(&[0., 0.])), [0.5, 0.5]);
        let repaired = normalized_flexes(&[f32::NAN, f32::INFINITY]);
        assert_eq!(rounded(&repaired), [0.5, 0.5]);
    }

    #[test]
    fn dragging_the_divider_only_moves_its_two_neighbours() {
        let flexes = flexes_after_divider_drag(&[0.5, 0.5], 0, 0.3);
        assert_eq!(rounded(&flexes), [0.3, 0.7]);
        // 3 枚のときは境界の外側の取り分が動かない。
        let flexes = flexes_after_divider_drag(&[0.25, 0.25, 0.5], 1, 0.4);
        assert_eq!(rounded(&flexes), [0.25, 0.15, 0.6]);
        // 端まで引いても下限までしか詰められない。
        let flexes = flexes_after_divider_drag(&[0.5, 0.5], 0, 0.);
        assert_eq!(rounded(&flexes), [0.15, 0.85]);
        // 無い境界や壊れた割合では何も動かさない。
        assert_eq!(rounded(&flexes_after_divider_drag(&[1.], 0, 0.5)), [1.]);
        assert_eq!(
            rounded(&flexes_after_divider_drag(&[0.5, 0.5], 0, f32::NAN)),
            [0.5, 0.5]
        );
    }

    #[test]
    fn a_folded_group_hands_its_share_to_a_neighbour() {
        // 空いた分は右隣へ。
        assert_eq!(
            rounded(&flexes_after_group_removal(&[0.5, 0.25, 0.25], 0)),
            [0.75, 0.25]
        );
        // 右端を畳んだときだけ左隣へ。
        assert_eq!(
            rounded(&flexes_after_group_removal(&[0.5, 0.25, 0.25], 2)),
            [0.5, 0.5]
        );
        // 最後の 1 枚を畳んだら残らない。範囲外は何も動かさない。
        assert!(flexes_after_group_removal(&[1.], 0).is_empty());
        assert_eq!(rounded(&flexes_after_group_removal(&[1.], 3)), [1.]);
    }

    #[test]
    fn splitting_halves_the_share_of_the_group_it_came_from() {
        assert_eq!(rounded(&flexes_after_split(&[1.], 0, 1)), [0.5, 0.5]);
        // 切り出し元の取り分だけを分ける (隣は動かない)。
        assert_eq!(
            rounded(&flexes_after_split(&[0.5, 0.5], 1, 2)),
            [0.5, 0.25, 0.25]
        );
        assert_eq!(
            rounded(&flexes_after_split(&[0.5, 0.5], 0, 0)),
            [0.25, 0.25, 0.5]
        );
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
