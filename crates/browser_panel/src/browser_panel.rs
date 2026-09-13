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
//! - ドックの [`BrowserPanel`] — ワークスペースに 1 つ。左・右・下のどこにでも
//!   置ける (既定は右)。エージェントパネルと同じドックなので、同時に出せるのは
//!   どちらか一方。パネルの中は**タブバー**で、ここでもページは何枚でも開ける
//! - エディター領域のタブ [`BrowserItem`] — **いくつでも開ける** (`browser_panel::NewBrowserTab`)
//!
//! どちらも中身は [`BrowserView`] で、WebView2 の環境 (`ICoreWebView2Environment`) は
//! プロセス内で 1 つを共有する (user data フォルダーは 1 つの環境しか開けない)。
//! **1 タブ = 1 コントローラー**で、履歴もフォーカスもページごとに独立する。
//! 裏に回ったタブの子 HWND は明示的に隠し (見せるのは描画からの pull)、
//! タブを閉じたら [`BrowserView`] ごと drop してコントローラーを Close する。
//!
//! ブックマークは設定ではなく `browser_bookmarks.json` に置く ([`browser_bookmarks`])。

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use async_recursion::async_recursion;
use db::kvp::KeyValueStore;
use editor::Editor;
use gpui::{
    Action, App, AsyncWindowContext, Axis, Context, Entity, EntityId, EventEmitter, FocusHandle,
    Focusable, MouseDownEvent, Pixels, Point, Task, WeakEntity, Window, actions,
};
use project::Project;
use serde::{Deserialize, Serialize};
use settings::{IntoGpui, RegisterSetting, Settings};
use ui::{Tooltip, prelude::*};
use util::{ResultExt as _, TryFutureExt as _};
use workspace::{
    CloseActiveItem, DraggedTab, ItemId, Member, Pane, PaneAxis, PaneGroup, SplitDirection,
    SplitMode, Workspace, WorkspaceId,
    dock::{DockPosition, Panel, PanelEvent},
    pane,
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
    pub starts_open: bool,
    pub default_width: Pixels,
    pub default_height: Pixels,
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
            starts_open: browser.starts_open.unwrap_or(false),
            default_width: browser
                .default_width
                .map(|width| width.into_gpui())
                .unwrap_or(px(480.)),
            default_height: browser
                .default_height
                .map(|height| height.into_gpui())
                .unwrap_or(px(320.)),
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
pub(crate) fn tab_tooltip(url: &str) -> SharedString {
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

/// 新しいタブをどう開くか。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum NewTabPlacement {
    /// 表に出してフォーカスも移す (「+」ボタンなど、利用者が明示的に開いたとき)。
    Foreground,
    /// 表には出すがフォーカスは動かさない。ドックの復元でパネルが作られたときに
    /// エディターからフォーカスを奪わないため。
    ForegroundWithoutFocus,
    /// 裏で開く (ブックマークの Ctrl+クリック)。WebView2 のコントローラーは
    /// 表に出るまで作られない。
    Background,
}

/// ドックの内蔵ブラウザー。中身はワークスペースと同じ [`PaneGroup`] + [`Pane`] で、
/// タブ 1 枚 = [`BrowserItem`] 1 つ = WebView2 コントローラー 1 つ。
///
/// ターミナルパネルと同じ作りにしてあるのは、**IDE 内のどのタブ列にもドラッグで
/// 移せる**ようにするため (第 12 便)。パネル独自のタブ実装だと、掴んだタブは
/// パネルの外へ出られない。
pub struct BrowserPanel {
    fs: Arc<dyn fs::Fs>,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    /// タブが 1 枚も無いときに使うフォーカス先。タブがあるときはペインに渡す。
    focus_handle: FocusHandle,
    pub(crate) center: PaneGroup,
    pub(crate) active_pane: Entity<Pane>,
    pending_serialization: Task<Option<()>>,
    /// 保存したレイアウトを組み立てている最中。この間の保存は「まだ空のパネル」を
    /// 書いてしまうので止める ([`BrowserPanel::load_inner`])。
    restoring: bool,
    active: bool,
}

pub fn init(cx: &mut App) {
    // ブラウザータブはパネルの外 (エディター領域 / ターミナルパネル) にも置けるので、
    // 中央のタブと同じ per-kind の復元機構に載せる (ADR-F37)。
    workspace::register_serializable_item::<BrowserItem>(cx);
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
                // 「開く」ではなく「移す」。同じアイテムを中央のペインへ渡すので、
                // 読み込み済みのページも履歴もそのまま付いていく。
                BrowserPanel::move_active_tab_to_center(workspace, window, cx);
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
    /// 復元しなかったアイテムの行の掃除は、中央とすべてのパネルの復元が揃ってから
    /// ワークスペースが 1 回だけ走らせる (ADR-F37)。ここでやることは待ち合わせへの
    /// 参加だけで、失敗しても必ず抜ける (抜けないとそのウィンドウで掃除が止まる)。
    ///
    /// このパネルが workspace に載る (`add_panel`) のは `load` が返ってからなので、
    /// ここで数えを戻した時点ではまだタブが「どのペインにも居ない」ように見える。
    /// 掃除が実際に走るのは zed の `initialize_panels` が全パネルを載せ終えてから
    /// (向こうでもう 1 つ数えている) なので、この順序に依存している。
    ///
    /// The first time that bookkeeping drains also ends the workspace's dock
    /// startup phase (ADR-F48), for good: a load that takes part in it after
    /// that does not bring startup back.
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
        let stored = workspace
            .read_with(&cx, |workspace, cx| {
                Self::serialization_key(workspace).map(|key| (key, KeyValueStore::global(cx)))
            })
            .ok()
            .flatten();
        let serialized = match stored {
            Some((key, kvp)) => cx
                .background_spawn(async move { kvp.read_kvp(&key) })
                .await
                .log_err()
                .flatten()
                .and_then(|raw| serde_json::from_str::<SerializedBrowserPanel>(&raw).log_err()),
            None => None,
        };

        let workspace_id = workspace
            .read_with(&cx, |workspace, _| workspace.database_id())
            .ok()
            .flatten();

        let panel = workspace.update_in(&mut cx, |workspace, window, cx| {
            cx.new(|cx| BrowserPanel::new(workspace, window, cx))
        })?;
        // タブを開くのはワークスペースの更新が終わってから。`BrowserItem` の生成が
        // ワークスペースを読みに行くので、`update_in` の中でやると二重借用になる。
        let restored = match serialized {
            Some(serialized) => {
                // 復元中のペインはまだ `panel.center` に載っていない。タブが 1 枚
                // 足されるたびに `AddItem` で保存すると、その空のレイアウトが
                // 保存済みのレイアウトを上書きしてしまう。
                panel.update(&mut cx, |panel, _| panel.restoring = true);
                let restored = restore(&panel, serialized, workspace_id, &mut cx).await;
                panel.update(&mut cx, |panel, _| panel.restoring = false);
                restored
            }
            None => false,
        };
        if !restored {
            panel.update_in(&mut cx, |panel, window, cx| {
                panel.open_tab(None, NewTabPlacement::ForegroundWithoutFocus, window, cx);
            })?;
        }
        Ok(panel)
    }

    fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let pane = new_browser_pane(workspace.weak_handle(), project.clone(), window, cx);
        let fs = workspace.app_state().fs.clone();
        Self {
            fs,
            workspace: workspace.weak_handle(),
            project,
            focus_handle: cx.focus_handle(),
            center: PaneGroup::new(pane.clone()),
            active_pane: pane,
            pending_serialization: Task::ready(None),
            restoring: false,
            active: false,
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
        let pane = self.active_pane.clone();
        self.open_tab_in_pane(&pane, url, placement, window, cx);
    }

    pub(crate) fn open_tab_in_pane(
        &mut self,
        pane: &Entity<Pane>,
        url: Option<String>,
        placement: NewTabPlacement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let item = cx.new(|cx| {
            BrowserItem::new(
                self.workspace.clone(),
                self.fs.clone(),
                BrowserHost::Panel,
                url,
                window,
                cx,
            )
        });
        // 既にパネルの中にフォーカスがあるなら、フォーカスは新しいタブへ移す
        // (タブを閉じた直後に開いたときにフォーカスが宙に浮かないように)。
        let focus = placement == NewTabPlacement::Foreground
            || (placement == NewTabPlacement::ForegroundWithoutFocus
                && self.contains_focus(window, cx));
        // `Pane::add_item` は必ず表に出してしまうので、背景タブは
        // `add_item_inner` で「足すだけ」にする (WebView2 はまだ作られない)。
        let activate = placement != NewTabPlacement::Background;
        pane.update(cx, |pane, cx| {
            pane.add_item_inner(Box::new(item), focus, focus, activate, None, window, cx);
        });
    }

    fn contains_focus(&self, window: &Window, cx: &App) -> bool {
        self.center
            .panes()
            .into_iter()
            .any(|pane| pane.focus_handle(cx).contains_focused(window, cx))
    }

    pub fn active_view(&self, cx: &App) -> Option<Entity<BrowserView>> {
        self.active_pane
            .read(cx)
            .active_item()?
            .downcast::<BrowserItem>()
            .map(|item| item.read(cx).view().clone())
    }

    /// ブックマークバーを出してよいビューか (ADR-F33 追記 2)。バーはビューの中の
    /// アドレス欄の下に戻したので、分割していても「アクティブなペインのアクティブな
    /// ビュー」だけが出す。ビュー側が描くたびに聞きに来る (push しないので、
    /// タブを閉じた・落とした後に古い旗が残らない)。
    pub(crate) fn is_active_view(&self, view_id: EntityId, cx: &App) -> bool {
        self.active_view(cx)
            .is_some_and(|view| view.entity_id() == view_id)
    }

    pub fn current_url(&self, cx: &App) -> String {
        self.active_view(cx)
            .map(|view| view.read(cx).current_url().to_string())
            .unwrap_or_default()
    }

    pub fn navigate_to(&mut self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        match self.active_view(cx) {
            Some(view) => view.update(cx, |view, cx| view.navigate_to(url, cx)),
            None => self.open_tab(
                Some(url.to_string()),
                NewTabPlacement::Foreground,
                window,
                cx,
            ),
        }
    }

    /// 「このタブの隣にもう 1 枚」の受け口。そのページを出していたペインの中に、
    /// ブラウザーと同じく**背景タブ**として足す (今のページは見えたまま)。
    pub(crate) fn open_beside_item(
        &mut self,
        item_id: EntityId,
        url: Option<String>,
        placement: NewTabPlacement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = self
            .pane_for_item(item_id, cx)
            .unwrap_or_else(|| self.active_pane.clone());
        self.open_tab_in_pane(&pane, url, placement, window, cx);
    }

    /// ファイルを落として開くときの受け口。端に落とされていたら (`split` が
    /// `Some`) まずそのペインを割り、開いたページを新しいペインに入れる。
    /// 1 つ目だけ表に出し、残りは背景タブ。
    pub(crate) fn open_dropped_urls(
        &mut self,
        item_id: EntityId,
        urls: Vec<String>,
        split: Option<SplitDirection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = self
            .pane_for_item(item_id, cx)
            .unwrap_or_else(|| self.active_pane.clone());
        let pane = match split {
            Some(direction) => self.split_pane(&pane, direction, window, cx),
            None => pane,
        };
        for (index, url) in urls.into_iter().enumerate() {
            let placement = if index == 0 {
                NewTabPlacement::Foreground
            } else {
                NewTabPlacement::Background
            };
            self.open_tab_in_pane(&pane, Some(url), placement, window, cx);
        }
    }

    /// ページの中がクリックされたときに、そのタブが載っているペインを
    /// アクティブにする。子 HWND のクリックは GPUI に届かないので、
    /// [`BrowserViewEvent::Focused`] の申告だけが手掛かりになる。
    pub(crate) fn activate_pane_for_item(
        &mut self,
        item_id: EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.pane_for_item(item_id, cx) else {
            return;
        };
        // 既にそのペインの中にフォーカスがあるなら触らない。アドレス欄を
        // クリックしたときもこの申告は飛ぶので、ここで奪うと入力できなくなる
        // (ペインにフォーカスを移すと `Pane::focus_in` がビューへ送り直す)。
        if !pane.focus_handle(cx).contains_focused(window, cx) {
            window.focus(&pane.focus_handle(cx), cx);
        }
        self.activate_pane(pane, cx);
    }

    /// このペインを「次の操作の宛先」にする (ブックマークバー・アドレス欄・
    /// `Ctrl+T` はすべてここを見る)。フォーカスは動かさない。
    fn activate_pane(&mut self, pane: Entity<Pane>, cx: &mut Context<Self>) {
        if pane == self.active_pane {
            return;
        }
        // 宛先が移った両側のビューを描き直させる (ブックマークバーの出し入れは
        // ビューの中で起きる)。ペインは `.cached()` なので、パネルの `notify` だけでは
        // 前のフレームの描画がそのまま流し直される。
        let previously_active_view = self.active_view(cx);
        self.active_pane = pane;
        let newly_active_view = self.active_view(cx);
        for view in [previously_active_view, newly_active_view]
            .into_iter()
            .flatten()
        {
            view.update(cx, |_, cx| cx.notify());
        }
        cx.notify();
    }

    /// クリックが落ちたペインをアクティブにする。
    ///
    /// [`pane::Event::Focus`] は**ペインの外から**フォーカスが入ったときにしか
    /// 鳴らないので、既にそのペインの中にフォーカスがある状態でタブバーや
    /// 「+」を押しても切り替わらない。逆に、フォーカスが子 HWND (ページ) に
    /// あるあいだは GPUI 側のフォーカスが動かないままタブバーだけが押される。
    /// どちらの取りこぼしもここで拾う。
    fn activate_pane_at(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(pane) = self.center.pane_at_pixel_position(position).cloned() else {
            return;
        };
        self.activate_pane(pane, cx);
    }

    fn url_of_active_item(&self, pane: &Entity<Pane>, cx: &App) -> Option<String> {
        let url = pane
            .read(cx)
            .active_item()?
            .downcast::<BrowserItem>()?
            .read(cx)
            .view()
            .read(cx)
            .current_url()
            .trim()
            .to_string();
        (!url.is_empty()).then_some(url)
    }

    fn pane_for_item(&self, item_id: EntityId, cx: &App) -> Option<Entity<Pane>> {
        self.center
            .panes()
            .into_iter()
            .find(|pane| pane.read(cx).items().any(|item| item.item_id() == item_id))
            .cloned()
    }

    /// 表に出ていないタブの子 HWND を隠す。子 HWND は GPUI の描画に従わないので
    /// 「隠すのは push、見せるのは pull」(次の描画で canvas が矩形を流し直す)。
    /// 分割しているときは**ペインごとに 1 枚**が表に出る。
    fn hide_inactive_surfaces(&self, cx: &mut App) {
        for pane in self.center.panes() {
            let active_item_id = pane.read(cx).active_item().map(|item| item.item_id());
            let inactive = pane
                .read(cx)
                .items()
                .filter(|item| Some(item.item_id()) != active_item_id)
                .filter_map(|item| item.downcast::<BrowserItem>())
                .collect::<Vec<_>>();
            for item in inactive {
                item.update(cx, |item, cx| item.hide_surface(HIDDEN_TAB_INACTIVE, cx));
            }
        }
    }

    fn hide_all_surfaces(&self, reason: &'static str, cx: &mut App) {
        for pane in self.center.panes() {
            let items = pane
                .read(cx)
                .items()
                .filter_map(|item| item.downcast::<BrowserItem>())
                .collect::<Vec<_>>();
            for item in items {
                item.update(cx, |item, cx| item.hide_surface(reason, cx));
            }
        }
    }

    fn has_no_tabs(&self, cx: &App) -> bool {
        self.center
            .panes()
            .into_iter()
            .all(|pane| pane.read(cx).items_len() == 0)
    }

    // -- ペイン --------------------------------------------------------------

    fn split_pane(
        &mut self,
        pane_to_split: &Entity<Pane>,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Pane> {
        let new_pane = new_browser_pane(self.workspace.clone(), self.project.clone(), window, cx);
        self.center.split(pane_to_split, &new_pane, direction, cx);
        cx.notify();
        new_pane
    }

    /// 今のタブを右に切り出して左右に分割する (`SplitPanelRight`)。ペインに 1 枚しか
    /// 無いときは同じページをもう 1 枚開いてから出す (ブラウザーの「タブの複製」)。
    fn split_active_tab_right(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pane = self.active_pane.clone();
        let items_len = pane.read(cx).items_len();
        if items_len == 0 {
            return;
        }
        if items_len == 1 {
            let url = self.url_of_active_item(&pane, cx);
            let new_pane = self.split_pane(&pane, SplitDirection::Right, window, cx);
            self.open_tab_in_pane(&new_pane, url, NewTabPlacement::Foreground, window, cx);
            return;
        }
        let Some(item) = pane.update(cx, |pane, cx| pane.take_active_item(window, cx)) else {
            return;
        };
        let new_pane = self.split_pane(&pane, SplitDirection::Right, window, cx);
        new_pane.update(cx, |pane, cx| {
            pane.add_item(item, true, true, None, window, cx);
        });
        // フォーカスの往復を待たずに、次の操作 (新しいタブなど) が新しいペインに
        // 向くようにしておく。
        self.activate_pane(new_pane.clone(), cx);
        window.focus(&new_pane.focus_handle(cx), cx);
    }

    fn focus_next_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let panes = self.center.panes();
        if panes.len() < 2 {
            return;
        }
        let Some(index) = panes.iter().position(|pane| **pane == self.active_pane) else {
            return;
        };
        let Some(next) = panes.get((index + 1) % panes.len()) else {
            return;
        };
        window.focus(&next.focus_handle(cx), cx);
    }

    fn handle_pane_event(
        &mut self,
        pane: &Entity<Pane>,
        event: &pane::Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            pane::Event::ActivateItem { local, .. } => {
                // `local` = そのペインの中でユーザーが選んだ (タブのクリック・
                // 新しいタブ)。復元や裏での差し替えでは立たないので、これを
                // 宛先の切り替えに使ってよい。
                if *local {
                    self.activate_pane(pane.clone(), cx);
                }
                self.hide_inactive_surfaces(cx);
                self.serialize(cx);
            }
            pane::Event::RemovedItem { item } => {
                if let Some(item) = item.downcast::<BrowserItem>() {
                    item.update(cx, |item, cx| item.hide_surface(HIDDEN_TAB_CLOSED, cx));
                }
                self.serialize(cx);
            }
            pane::Event::Remove { focus_on_pane } => {
                let pane_count_before_removal = self.center.panes().len();
                if let Err(error) = self.center.remove(pane, cx) {
                    log::warn!("browser panel: could not remove a pane: {error:#}");
                }
                if pane_count_before_removal == 1 {
                    self.center.first_pane().update(cx, |pane, cx| {
                        pane.set_zoomed(false, cx);
                    });
                    cx.emit(PanelEvent::Close);
                } else if let Some(focus_on_pane) =
                    focus_on_pane.as_ref().or_else(|| self.center.panes().pop())
                {
                    focus_on_pane.focus_handle(cx).focus(window, cx);
                }
                self.serialize(cx);
            }
            pane::Event::ZoomIn => {
                for pane in self.center.panes() {
                    pane.update(cx, |pane, cx| pane.set_zoomed(true, cx));
                }
                cx.emit(PanelEvent::ZoomIn);
                cx.notify();
            }
            pane::Event::ZoomOut => {
                for pane in self.center.panes() {
                    pane.update(cx, |pane, cx| pane.set_zoomed(false, cx));
                }
                cx.emit(PanelEvent::ZoomOut);
                cx.notify();
            }
            pane::Event::AddItem { item } => {
                if let Some(workspace) = self.workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        item.added_to_pane(workspace, pane.clone(), window, cx)
                    })
                }
                // `Item::added_to_workspace` が既定の「エディター領域のタブ」に
                // 戻すので、パネルに載ったことはここで上書きして教える
                // (ツールバーの「エディター領域で開く」の出し分けに効く)。
                if let Some(item) = item.downcast::<BrowserItem>() {
                    item.update(cx, |item, cx| item.set_host(BrowserHost::Panel, cx));
                }
                self.hide_inactive_surfaces(cx);
                self.serialize(cx);
            }
            &pane::Event::Split { direction, mode } => {
                let pane = pane.clone();
                match mode {
                    SplitMode::ClonePane | SplitMode::EmptyPane => {
                        let url = if matches!(mode, SplitMode::ClonePane) {
                            self.url_of_active_item(&pane, cx)
                        } else {
                            None
                        };
                        let new_pane = self.split_pane(&pane, direction, window, cx);
                        self.activate_pane(new_pane.clone(), cx);
                        self.open_tab_in_pane(
                            &new_pane,
                            url,
                            NewTabPlacement::Foreground,
                            window,
                            cx,
                        );
                        window.focus(&new_pane.focus_handle(cx), cx);
                    }
                    SplitMode::MovePane => {
                        let Some(item) =
                            pane.update(cx, |pane, cx| pane.take_active_item(window, cx))
                        else {
                            return;
                        };
                        let new_pane = self.split_pane(&pane, direction, window, cx);
                        new_pane.update(cx, |pane, cx| {
                            pane.add_item(item, true, true, None, window, cx);
                        });
                        self.activate_pane(new_pane.clone(), cx);
                        window.focus(&new_pane.focus_handle(cx), cx);
                    }
                }
            }
            pane::Event::Focus => self.activate_pane(pane.clone(), cx),
            pane::Event::ItemPinned | pane::Event::ItemUnpinned => self.serialize(cx),
            _ => {}
        }
    }

    // -- 永続化 --------------------------------------------------------------

    fn serialization_key(workspace: &Workspace) -> Option<String> {
        workspace
            .database_id()
            .map(|id| i64::from(id).to_string())
            .or(workspace.session_id())
            .map(|id| format!("{BROWSER_PANEL_KEY:?}-{id:?}"))
    }

    fn serialize(&mut self, cx: &mut Context<Self>) {
        if self.restoring {
            return;
        }
        let Some((serialization_key, format)) = self
            .workspace
            .read_with(cx, |workspace, _| {
                let key = Self::serialization_key(workspace)?;
                Some((key, PanelLayoutFormat::of(workspace)))
            })
            .ok()
            .flatten()
        else {
            return;
        };
        let kvp = KeyValueStore::global(cx);
        self.pending_serialization = cx.spawn(async move |panel, cx| {
            // まとめて動く操作 (ドロップ = 取り外し + 追加) で 2 回書かないように。
            cx.background_executor()
                .timer(Duration::from_millis(50))
                .await;
            let panel = panel.upgrade()?;
            let serialized = panel.update(cx, |panel, cx| SerializedBrowserPanel {
                root: serialize_pane_group(&panel.center.root, &panel.active_pane, format, cx),
            });
            cx.background_spawn(
                async move {
                    kvp.write_kvp(serialization_key, serde_json::to_string(&serialized)?)
                        .await?;
                    anyhow::Ok(())
                }
                .log_err(),
            )
            .await;
            Some(())
        });
    }

    // -- エディター領域との行き来 --------------------------------------------

    /// パネルの今のタブを中央のペインへ**移す** (`OpenInCenter`)。
    fn move_active_tab_to_center(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(panel) = workspace.panel::<BrowserPanel>(cx) else {
            return;
        };
        let source_pane = panel.read(cx).active_pane.clone();
        let Some(item_id) = source_pane
            .read(cx)
            .active_item()
            .map(|item| item.item_id())
        else {
            return;
        };
        let destination = workspace.active_pane().clone();
        if destination == source_pane {
            return;
        }
        let destination_index = destination.read(cx).items_len();
        workspace::move_item(
            &source_pane,
            &destination,
            item_id,
            destination_index,
            true,
            window,
            cx,
        );
    }

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
                if let Some(view) = panel.active_view(cx) {
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

/// パネルの中のペインを 1 枚作る。ターミナルパネルの `new_terminal_pane` と同じ作りで、
/// 違うのは「+」が新しいブラウザータブになることと、分割を中央ではなくこのパネルの
/// [`PaneGroup`] に向けること。
fn new_browser_pane(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    window: &mut Window,
    cx: &mut Context<BrowserPanel>,
) -> Entity<Pane> {
    let browser_panel = cx.entity();
    let pane = cx.new(|cx| {
        let mut pane = Pane::new(
            workspace.clone(),
            project.clone(),
            Default::default(),
            None,
            NewPanelTab.boxed_clone(),
            false,
            window,
            cx,
        );
        pane.set_can_navigate(false, cx);
        pane.display_nav_history_buttons(None);
        pane.set_should_display_tab_bar(|_, _| true);
        pane.set_zoom_out_on_close(false);

        let split_predicate_panel = browser_panel.downgrade();
        pane.set_can_split(Some(Arc::new(move |pane, dragged_item, _window, cx| {
            let Some(tab) = dragged_item.downcast_ref::<DraggedTab>() else {
                return false;
            };
            let is_current_pane = tab.pane == cx.entity();
            let Ok(can_drag_away) = split_predicate_panel.read_with(cx, |panel, _| {
                let panes = panel.center.panes();
                !panes.contains(&&tab.pane)
                    || panes.len() > 1
                    || (!is_current_pane || pane.items_len() > 1)
            }) else {
                return false;
            };
            if !can_drag_away {
                return false;
            }
            // 第 12 便: IDE 内のどのタブでも同じ機構でここへ落とせる。
            if is_current_pane {
                pane.item_for_index(tab.ix).is_some()
            } else {
                tab.pane.read(cx).item_for_index(tab.ix).is_some()
            }
        })));

        // 端に落として分割するときは、ワークスペースの中央ではなくこのパネルの
        // `PaneGroup` を割る (`Workspace::split_pane` は中央しか割れない)。
        let split_handler_panel = browser_panel.downgrade();
        pane.set_tab_drop_split_handler(Some(Arc::new(
            move |pane_to_split, direction, window, cx| {
                let panel = split_handler_panel.upgrade()?;
                panel.update(cx, |panel, cx| {
                    if !panel.center.panes().contains(&pane_to_split) {
                        return None;
                    }
                    Some(panel.split_pane(pane_to_split, direction, window, cx))
                })
            },
        )));

        let buttons_panel = browser_panel.downgrade();
        pane.set_render_tab_bar_buttons(cx, move |pane, _window, cx| {
            let new_tab_panel = buttons_panel.clone();
            let split_panel = buttons_panel.clone();
            let pane_handle = cx.entity();
            let focus_handle = pane.focus_handle(cx);
            let split_focus_handle = focus_handle.clone();
            (
                None,
                Some(
                    h_flex()
                        .gap(DynamicSpacing::Base02.rems(cx))
                        .child(
                            IconButton::new("browser-panel-new-tab", IconName::Plus)
                                .icon_size(IconSize::Small)
                                .tooltip(move |_, cx| {
                                    Tooltip::for_action_in(
                                        "新しいタブ",
                                        &NewPanelTab,
                                        &focus_handle,
                                        cx,
                                    )
                                })
                                .on_click(move |_, window, cx| {
                                    new_tab_panel
                                        .update(cx, |panel, cx| {
                                            panel.open_tab_in_pane(
                                                &pane_handle,
                                                None,
                                                NewTabPlacement::Foreground,
                                                window,
                                                cx,
                                            )
                                        })
                                        .log_err();
                                }),
                        )
                        .child(
                            IconButton::new("browser-panel-split-right", IconName::Split)
                                .icon_size(IconSize::Small)
                                .tooltip(move |_, cx| {
                                    Tooltip::for_action_in(
                                        "右に分割",
                                        &SplitPanelRight,
                                        &split_focus_handle,
                                        cx,
                                    )
                                })
                                .on_click(move |_, window, cx| {
                                    split_panel
                                        .update(cx, |panel, cx| {
                                            panel.split_active_tab_right(window, cx)
                                        })
                                        .log_err();
                                }),
                        )
                        .into_any_element(),
                ),
            )
        });

        pane
    });

    cx.subscribe_in(&pane, window, BrowserPanel::handle_pane_event)
        .detach();
    cx.observe(&pane, |_, _, cx| cx.notify()).detach();

    pane
}

impl Render for BrowserPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let center = self
            .workspace
            .update(cx, |workspace, cx| {
                let follower_states = Default::default();
                let workspace_handle = workspace.weak_handle();
                self.center
                    .render(
                        workspace.zoomed_item(),
                        None,
                        &workspace::PaneRenderContext {
                            follower_states: &follower_states,
                            active_call: workspace.active_call(),
                            active_pane: &self.active_pane,
                            app_state: workspace.app_state(),
                            project: workspace.project(),
                            workspace: &workspace_handle,
                        },
                        window,
                        cx,
                    )
                    .into_any_element()
            })
            .ok();

        v_flex()
            .key_context(BROWSER_PANEL_KEY)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            // 触ったペインを宛先にする。捕捉フェーズで拾うのは、タブや「+」が
            // クリックを消費してもここまで届かせるため (何も止めない)。
            .capture_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, _, cx| {
                this.activate_pane_at(event.position, cx);
            }))
            .on_action(cx.listener(|this, _: &NewPanelTab, window, cx| {
                this.open_tab(None, NewTabPlacement::Foreground, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ClosePanelTab, window, cx| {
                let pane = this.active_pane.clone();
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
            .children(center)
    }
}

impl Focusable for BrowserPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if self.active_pane.read(cx).items_len() == 0 {
            return self.focus_handle.clone();
        }
        self.active_pane.focus_handle(cx)
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

    fn position_is_valid(&self, _position: DockPosition) -> bool {
        true
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        settings::update_settings_file(self.fs.clone(), cx, move |settings, _| {
            settings.browser.get_or_insert_default().dock = Some(position.into())
        });
    }

    fn default_size(&self, window: &Window, cx: &App) -> Pixels {
        let settings = BrowserSettings::get_global(cx);
        match self.position(window, cx) {
            DockPosition::Left | DockPosition::Right => settings.default_width,
            DockPosition::Bottom => settings.default_height,
        }
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

    fn starts_open(&self, _: &Window, cx: &App) -> bool {
        BrowserSettings::get_global(cx).starts_open
    }

    fn pane(&self) -> Option<Entity<Pane>> {
        Some(self.active_pane.clone())
    }

    fn panes(&self) -> Vec<Entity<Pane>> {
        self.center.panes().into_iter().cloned().collect()
    }

    fn is_zoomed(&self, _: &Window, cx: &App) -> bool {
        self.center
            .panes()
            .into_iter()
            .any(|pane| pane.read(cx).is_zoomed())
    }

    fn set_zoomed(&mut self, zoomed: bool, _: &mut Window, cx: &mut Context<Self>) {
        for pane in self.center.panes() {
            pane.update(cx, |pane, cx| pane.set_zoomed(zoomed, cx));
        }
        cx.notify();
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        let was_active = self.active;
        self.active = active;
        if !active {
            self.hide_all_surfaces(HIDDEN_PANEL_INACTIVE, cx);
            cx.notify();
            return;
        }
        cx.notify();
        if was_active || !self.has_no_tabs(cx) {
            return;
        }
        // 最後のタブを閉じてパネルを畳んだあと、また開いたとき。空のペインには
        // タブバーすら出ないので、ホームのページを 1 枚用意する。
        cx.defer_in(window, |this, window, cx| {
            this.open_tab(None, NewTabPlacement::Foreground, window, cx)
        });
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

// ---------------------------------------------------------------------------
// 永続化
// ---------------------------------------------------------------------------

/// パネルのレイアウトはワークスペースの kvp に置く (ターミナルパネルと同じ流儀)。
/// タブの中身は種類ごとの `SerializableItem` が自分の DB 行に書くので、ここには
/// 「どの種類のタブがどの順で並んでいたか」だけを残す (ADR-F37)。
#[derive(Debug, Serialize, Deserialize)]
struct SerializedBrowserPanel {
    root: SerializedBrowserPaneGroup,
}

/// パネルのタブ 1 枚。中央の `SerializedItem` と同じ 4 つ組で、種類ごとの復元は
/// `workspace::deserialize_serializable_item` に投げる。
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct SerializedPanelItem {
    kind: Arc<str>,
    item_id: ItemId,
    active: bool,
    /// 中央の形式に合わせて書き出すが、戻すのは今のところ中央だけ
    /// (`Pane::set_preview_item_id` が workspace の外に出ていない)。
    #[serde(default)]
    preview: bool,
}

#[derive(Debug, Serialize, Deserialize)]
enum SerializedBrowserPaneGroup {
    Pane(SerializedBrowserPane),
    Group {
        axis: SerializedAxis,
        flexes: Option<Vec<f32>>,
        children: Vec<SerializedBrowserPaneGroup>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct SerializedBrowserPane {
    active: bool,
    #[serde(default)]
    items: Vec<SerializedPanelItem>,
    /// 第 12 便の形式。ブラウザーのタブを URL の並びで持っていた。移行のため
    /// 読むだけ残してある (次の保存で `items` に書き換わる)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    urls: Vec<String>,
    /// 同上。`urls` の添字。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active_item: Option<usize>,
}

/// `gpui::Axis` は `serde` を実装していないので、読める文字列で持つ
/// (ターミナルパネルの `SerializedAxis` と同じ形式)。
#[derive(Debug)]
struct SerializedAxis(Axis);

impl Serialize for SerializedAxis {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self.0 {
            Axis::Horizontal => serializer.serialize_str("horizontal"),
            Axis::Vertical => serializer.serialize_str("vertical"),
        }
    }
}

impl<'de> Deserialize<'de> for SerializedAxis {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let axis = String::deserialize(deserializer)?;
        match axis.as_str() {
            "horizontal" => Ok(SerializedAxis(Axis::Horizontal)),
            "vertical" => Ok(SerializedAxis(Axis::Vertical)),
            invalid => Err(serde::de::Error::custom(format!(
                "Invalid axis value: '{invalid}'"
            ))),
        }
    }
}

/// どちらの形式で書くか。
#[derive(Clone, Copy, PartialEq, Eq)]
enum PanelLayoutFormat {
    /// 通常。タブは `(種類, item_id)` で書き、中身は種類ごとの `SerializableItem` が
    /// ワークスペースの DB に持つ。
    Items,
    /// 切り離しウィンドウ (`workspace_id` が無い)。行を書く先が無いので
    /// (`BrowserItem::serialize` は `database_id` を要る)、第 12 便の形式
    /// (ブラウザーのタブの URL の並び) で書く。セッション中は今までどおり戻せる。
    Urls,
}

impl PanelLayoutFormat {
    fn of(workspace: &Workspace) -> Self {
        if workspace.database_id().is_some() {
            Self::Items
        } else {
            Self::Urls
        }
    }
}

fn serialize_pane_group(
    member: &Member,
    active_pane: &Entity<Pane>,
    format: PanelLayoutFormat,
    cx: &App,
) -> SerializedBrowserPaneGroup {
    match member {
        Member::Axis(PaneAxis {
            axis,
            members,
            flexes,
            bounding_boxes: _,
        }) => SerializedBrowserPaneGroup::Group {
            axis: SerializedAxis(*axis),
            children: members
                .iter()
                .map(|member| serialize_pane_group(member, active_pane, format, cx))
                .collect(),
            flexes: Some(flexes.lock().clone()),
        },
        Member::Pane(pane) => {
            SerializedBrowserPaneGroup::Pane(serialize_pane(pane, pane == active_pane, format, cx))
        }
    }
}

/// 種類を問わず、復元できるタブをすべて書き出す。ドラッグでターミナルや
/// エディターがパネルに載っていても、次の起動でこのペインに戻ってくる (ADR-F37)。
fn serialize_pane(
    pane: &Entity<Pane>,
    active: bool,
    format: PanelLayoutFormat,
    cx: &App,
) -> SerializedBrowserPane {
    let active_item_index = pane.read(cx).active_item_index();
    if format == PanelLayoutFormat::Urls {
        let mut urls = Vec::new();
        let mut active_item = None;
        for (index, item) in pane.read(cx).items().enumerate() {
            let Some(item) = item.downcast::<BrowserItem>() else {
                continue;
            };
            if index == active_item_index {
                active_item = Some(urls.len());
            }
            urls.push(item.read(cx).view().read(cx).current_url().to_string());
        }
        return SerializedBrowserPane {
            active,
            items: Vec::new(),
            urls,
            active_item,
        };
    }

    let preview_item_id = pane.read(cx).preview_item_id();
    let items = pane
        .read(cx)
        .items()
        .enumerate()
        .filter_map(|(index, item)| {
            let Some(handle) = item.to_serializable_item_handle(cx) else {
                log::info!(
                    "browser panel: a tab of a kind that cannot be restored is not being saved"
                );
                return None;
            };
            Some(SerializedPanelItem {
                kind: Arc::from(handle.serialized_item_kind()),
                item_id: item.item_id().as_u64(),
                active: index == active_item_index,
                preview: Some(item.item_id()) == preview_item_id,
            })
        })
        .collect();
    SerializedBrowserPane {
        active,
        items,
        urls: Vec::new(),
        active_item: None,
    }
}

/// 保存したレイアウトを組み立て直す。1 枚も復元できなければ `false` を返し、
/// 呼び出し側がホームのタブを 1 枚開く。
async fn restore(
    panel: &Entity<BrowserPanel>,
    serialized: SerializedBrowserPanel,
    workspace_id: Option<WorkspaceId>,
    cx: &mut AsyncWindowContext,
) -> bool {
    let Some((root, active_pane)) =
        deserialize_pane_group(panel, &serialized.root, workspace_id, cx).await
    else {
        return false;
    };
    panel.update(cx, |panel, cx| {
        panel.center = PaneGroup::with_root(root);
        panel.active_pane = active_pane.unwrap_or_else(|| panel.center.first_pane());
        panel.hide_inactive_surfaces(cx);
    });
    true
}

#[async_recursion(?Send)]
async fn deserialize_pane_group(
    panel: &Entity<BrowserPanel>,
    serialized: &SerializedBrowserPaneGroup,
    workspace_id: Option<WorkspaceId>,
    cx: &mut AsyncWindowContext,
) -> Option<(Member, Option<Entity<Pane>>)> {
    match serialized {
        SerializedBrowserPaneGroup::Group {
            axis,
            flexes,
            children,
        } => {
            let mut active_pane = None;
            let mut members = Vec::new();
            for child in children {
                if let Some((member, child_active)) =
                    deserialize_pane_group(panel, child, workspace_id, cx).await
                {
                    members.push(member);
                    active_pane = active_pane.or(child_active);
                }
            }
            match members.len() {
                0 => None,
                1 => Some((members.remove(0), active_pane)),
                _ => Some((
                    Member::Axis(PaneAxis::load(axis.0, members, flexes.clone())),
                    active_pane,
                )),
            }
        }
        SerializedBrowserPaneGroup::Pane(serialized_pane) => {
            if serialized_pane.items.is_empty() && serialized_pane.urls.is_empty() {
                return None;
            }
            let pane = panel
                .update_in(cx, |panel, window, cx| {
                    new_browser_pane(panel.workspace.clone(), panel.project.clone(), window, cx)
                })
                .log_err()?;

            if serialized_pane.items.is_empty() {
                restore_urls(panel, &pane, serialized_pane, cx);
            } else {
                restore_items(panel, &pane, &serialized_pane.items, workspace_id, cx).await;
            }

            // 1 枚も戻せなかったペインは畳む (空のペインはタブバーすら描かれない)。
            if pane.read_with(cx, |pane, _| pane.items_len()) == 0 {
                return None;
            }
            Some((
                Member::Pane(pane.clone()),
                serialized_pane.active.then_some(pane),
            ))
        }
    }
}

/// 第 12 便の形式 (URL の並び) の読み出し。ワークスペース ID を要らないので、
/// 切り離しウィンドウのセッション鍵で保存されたものもここまでは戻せる。
fn restore_urls(
    panel: &Entity<BrowserPanel>,
    pane: &Entity<Pane>,
    serialized_pane: &SerializedBrowserPane,
    cx: &mut AsyncWindowContext,
) {
    panel
        .update_in(cx, |panel, window, cx| {
            for (index, url) in serialized_pane.urls.iter().enumerate() {
                // 復元直後はどのタブも裏。表に出す 1 枚だけをこの後選ぶので、
                // 起動時に作られる WebView2 はペインごとに 1 つで済む。
                panel.open_tab_in_pane(
                    pane,
                    Some(url.clone()),
                    NewTabPlacement::Background,
                    window,
                    cx,
                );
                // `Pane` は「表に出ているタブの次」に挿すので、裏で足し続けると
                // 3 枚目から順序が崩れる。足すたびに末尾を選んで並びを保つ。
                pane.update(cx, |pane, cx| {
                    pane.activate_item(index, false, false, window, cx);
                });
            }
            let active_item = serialized_pane
                .active_item
                .unwrap_or(0)
                .min(serialized_pane.urls.len().saturating_sub(1));
            pane.update(cx, |pane, cx| {
                pane.activate_item(active_item, false, false, window, cx);
            });
        })
        .log_err();
}

/// 種類ごとの復元。`workspace` に登録の無い種類は飛ばす (それを知っていた版で
/// 保存された可能性があるので、失敗ではなく読み飛ばしとして扱う)。
async fn restore_items(
    panel: &Entity<BrowserPanel>,
    pane: &Entity<Pane>,
    items: &[SerializedPanelItem],
    workspace_id: Option<WorkspaceId>,
    cx: &mut AsyncWindowContext,
) {
    let Some(workspace_id) = workspace_id else {
        log::info!(
            "browser panel: this window has no workspace id, so its tabs cannot be restored"
        );
        return;
    };
    let (workspace, project) = panel.read_with(cx, |panel, _| {
        (panel.workspace.clone(), panel.project.clone())
    });

    let mut tasks = Vec::new();
    for item in items {
        let task = pane.update_in(cx, |_, window, cx| {
            workspace::deserialize_serializable_item(
                &item.kind,
                project.clone(),
                workspace.clone(),
                workspace_id,
                item.item_id,
                window,
                cx,
            )
        });
        match task {
            Ok(Some(task)) => tasks.push((item.kind.clone(), item.active, task)),
            Ok(None) => log::info!(
                "browser panel: no restore handler for a {:?} tab; skipping it",
                item.kind
            ),
            Err(error) => {
                log::warn!(
                    "browser panel: the window went away while restoring its tabs: {error:#}"
                );
                return;
            }
        }
    }

    let mut active_index = None;
    let mut next_index = 0;
    for (kind, active, task) in tasks {
        // 復元できないタブは飛ばすだけ。中身の記録が (終了間際に足したタブの
        // ように) 書かれていなかった、というのが主な理由なので、警告ではなく
        // 記録として残す。
        let item = match task.await {
            Ok(item) => item,
            Err(error) => {
                log::info!(
                    "browser panel: skipping a {kind:?} tab that could not be restored: {error:#}"
                );
                continue;
            }
        };
        // 表に出すのは最後に選ぶ 1 枚だけ (裏のブラウザータブは WebView2 を
        // 作らない)。裏で足すときの挿し先は `Pane` に任せると「表のタブの次」に
        // なって並びが崩れるので、末尾を明示する。
        let added = pane
            .update_in(cx, |pane, window, cx| {
                pane.add_item_inner(item, false, false, false, Some(next_index), window, cx);
            })
            .log_err()
            .is_some();
        if added {
            if active {
                active_index = Some(next_index);
            }
            next_index += 1;
        }
    }
    pane.update_in(cx, |pane, window, cx| {
        pane.activate_item(active_index.unwrap_or(0), false, false, window, cx);
    })
    .log_err();
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
        pub(crate) input_blocked: bool,
        pub(crate) overlays: Vec<OverlayRect>,
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    pub(crate) struct OverlayRect {
        pub(crate) left: f32,
        pub(crate) top: f32,
        pub(crate) right: f32,
        pub(crate) bottom: f32,
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
    use crate::browser_item::BrowserItemDb;
    use gpui::{AsyncWindowContext, ExternalPaths, TestAppContext, VisualTestContext};
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use std::path::PathBuf;
    use workspace::{MultiWorkspace, item::test::TestItem};

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
    fn settings_fall_back_to_the_fork_defaults() {
        let settings = BrowserSettings::from_settings(&settings::SettingsContent::default());
        assert_eq!(settings.home_url, DEFAULT_HOME_URL);
        assert_eq!(settings.search_url, DEFAULT_SEARCH_URL);
        assert_eq!(settings.copilot_url, DEFAULT_COPILOT_URL);
        assert_eq!(settings.dock, DockPosition::Right);
        assert!(settings.button);
        assert!(!settings.starts_open);
        assert!(!settings.use_egress_proxy);
    }
    // -- ペイン化したパネル ---------------------------------------------------

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
            // ドラッグでパネルに流れてくる「ブラウザー以外のタブ」の代役。
            workspace::register_serializable_item::<TestItem>(cx);
        });
    }

    async fn init_workspace_with_panel(
        cx: &mut TestAppContext,
    ) -> (gpui::WindowHandle<MultiWorkspace>, Entity<BrowserPanel>) {
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let panel = window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    cx.new(|cx| BrowserPanel::new(workspace, window, cx))
                })
            })
            .expect("failed to build the browser panel");
        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    workspace.add_panel(panel.clone(), window, cx);
                })
            })
            .expect("failed to add the browser panel to the workspace");

        (window_handle, panel)
    }

    fn pane_urls(pane: &Entity<Pane>, cx: &App) -> Vec<String> {
        pane.read(cx)
            .items()
            .filter_map(|item| item.downcast::<BrowserItem>())
            .map(|item| item.read(cx).view().read(cx).current_url().to_string())
            .collect()
    }

    fn pane_kinds(pane: &Entity<Pane>, cx: &App) -> Vec<&'static str> {
        pane.read(cx)
            .items()
            .filter_map(|item| item.to_serializable_item_handle(cx))
            .map(|handle| handle.serialized_item_kind())
            .collect()
    }

    fn async_window(cx: &mut VisualTestContext) -> AsyncWindowContext {
        cx.update(|window, cx| window.to_async(cx))
    }

    fn new_panel(
        window_handle: &gpui::WindowHandle<MultiWorkspace>,
        cx: &mut VisualTestContext,
    ) -> Entity<BrowserPanel> {
        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    cx.new(|cx| BrowserPanel::new(workspace, window, cx))
                })
            })
            .expect("failed to build the browser panel")
    }

    /// パネルに載っているブラウザーのタブの URL を、`BrowserItem::serialize` が
    /// 書くのと同じ行に置く。テストのワークスペースは DB の ID を持たないので、
    /// 保存の側だけ手で代行する。
    async fn store_browser_urls(
        panel: &Entity<BrowserPanel>,
        workspace_id: WorkspaceId,
        cx: &mut VisualTestContext,
    ) {
        let rows = panel.read_with(cx, |panel, cx| {
            panel
                .center
                .panes()
                .into_iter()
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
        store_urls(rows, workspace_id, cx).await;
    }

    /// `browser_items` に行を直接置く。
    async fn store_urls(
        rows: Vec<(ItemId, String)>,
        workspace_id: WorkspaceId,
        cx: &mut VisualTestContext,
    ) {
        let db = cx.update(|_, cx| BrowserItemDb::global(cx));
        // `browser_items` は `workspaces` を参照するので、先に親の行を作る。
        db.write(move |connection| {
            connection.exec(&format!(
                "INSERT OR IGNORE INTO workspaces (workspace_id) VALUES ({})",
                i64::from(workspace_id)
            ))?()
        })
        .await
        .expect("the browser_items rows need a workspace to reference");
        for (item_id, url) in rows {
            db.save_url(item_id, workspace_id, url)
                .await
                .expect("saving a browser tab's url");
        }
    }

    fn open_tabs(panel: &Entity<BrowserPanel>, urls: &[&str], cx: &mut VisualTestContext) {
        panel.update_in(cx, |panel, window, cx| {
            for url in urls {
                panel.open_tab(
                    Some((*url).to_string()),
                    NewTabPlacement::Foreground,
                    window,
                    cx,
                );
            }
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn a_new_tab_lands_in_the_panel_s_own_pane(cx: &mut TestAppContext) {
        init_test(cx);
        let (window_handle, panel) = init_workspace_with_panel(cx).await;
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        open_tabs(&panel, &["https://example.com/one"], cx);
        panel.update_in(cx, |panel, window, cx| {
            panel.open_tab(
                Some("https://example.com/two".to_string()),
                NewTabPlacement::Background,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, cx| {
            assert_eq!(panel.center.panes().len(), 1);
            assert_eq!(
                pane_urls(&panel.active_pane, cx),
                vec![
                    "https://example.com/one".to_string(),
                    "https://example.com/two".to_string()
                ]
            );
            // 背景タブは表に出ない = そのタブの WebView2 はまだ作られない。
            assert_eq!(panel.active_pane.read(cx).active_item_index(), 0);
        });
    }

    #[gpui::test]
    async fn splitting_the_panel_moves_the_tab_into_a_second_pane(cx: &mut TestAppContext) {
        init_test(cx);
        let (window_handle, panel) = init_workspace_with_panel(cx).await;
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        open_tabs(
            &panel,
            &["https://example.com/one", "https://example.com/two"],
            cx,
        );
        panel.update_in(cx, |panel, window, cx| {
            panel.split_active_tab_right(window, cx)
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, cx| {
            let panes = panel.center.panes();
            assert_eq!(panes.len(), 2, "the panel should own both panes itself");
            assert_eq!(
                pane_urls(panes[0], cx),
                vec!["https://example.com/one".to_string()]
            );
            assert_eq!(
                pane_urls(panes[1], cx),
                vec!["https://example.com/two".to_string()]
            );
        });
        window_handle
            .read_with(cx, |multi_workspace, cx| {
                assert_eq!(
                    multi_workspace.workspace().read(cx).panes().len(),
                    1,
                    "the center must not be split by a panel-local split"
                );
            })
            .expect("failed to read the workspace panes");
    }

    /// 分割したパネルを 1 枚描いた状態にする。フォーカスのイベント
    /// (`Pane::focus_in`) は描画の終わりに配られるので、描かないと出ない。
    fn split_panel(
        window_handle: &gpui::WindowHandle<MultiWorkspace>,
        panel: &Entity<BrowserPanel>,
        cx: &mut VisualTestContext,
    ) -> (Entity<Pane>, Entity<Pane>) {
        open_tabs(
            panel,
            &["https://example.com/one", "https://example.com/two"],
            cx,
        );
        panel.update_in(cx, |panel, window, cx| {
            panel.split_active_tab_right(window, cx)
        });
        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    workspace.focus_panel::<BrowserPanel>(window, cx);
                });
            })
            .expect("failed to open the browser panel");
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, _| {
            let panes = panel.center.panes();
            assert_eq!(panes.len(), 2);
            (panes[0].clone(), panes[1].clone())
        })
    }

    #[gpui::test]
    async fn focusing_a_split_pane_makes_it_the_target_of_the_panel(cx: &mut TestAppContext) {
        init_test(cx);
        let (window_handle, panel) = init_workspace_with_panel(cx).await;
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);
        let (first, second) = split_panel(&window_handle, &panel, cx);
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.active_pane, second,
                "the pane the split created takes over"
            );
        });

        // アドレス欄やページをクリックしたときと同じ経路 (ペインの部分木にフォーカスが入る)。
        panel.update_in(cx, |_, window, cx| {
            window.focus(&first.focus_handle(cx), cx);
        });
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, cx| {
            assert_eq!(
                panel.active_pane, first,
                "focus moving into a pane makes it the active one"
            );
            let view = panel
                .active_view(cx)
                .expect("the focused pane has a browser tab");
            assert_eq!(view.read(cx).current_url(), "https://example.com/one");
        });

        // ブックマークのクリック (= アクティブなビューへのナビゲーション) と
        // 新しいタブは、どちらもそのペインに向く。
        panel.update_in(cx, |panel, window, cx| {
            panel.navigate_to("https://example.com/bookmark", window, cx);
            panel.open_tab(
                Some("https://example.com/new".to_string()),
                NewTabPlacement::Foreground,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, cx| {
            assert_eq!(
                pane_urls(&first, cx),
                vec![
                    "https://example.com/bookmark".to_string(),
                    "https://example.com/new".to_string()
                ],
                "both the navigation and the new tab land in the focused pane"
            );
            assert_eq!(
                pane_urls(&second, cx),
                vec!["https://example.com/two".to_string()],
                "the other pane is left alone"
            );
            assert_eq!(panel.active_pane, first);
        });
    }

    #[gpui::test]
    async fn clicking_a_tab_in_the_other_pane_makes_that_pane_the_target(cx: &mut TestAppContext) {
        init_test(cx);
        let (window_handle, panel) = init_workspace_with_panel(cx).await;
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);
        let (first, second) = split_panel(&window_handle, &panel, cx);

        // タブのクリックそのもの (`Pane::render_tab` の `on_click`)。フォーカスが
        // 既にそのペインの中にあると `focus_in` は鳴らないので、宛先の切り替えは
        // `ActivateItem { local: true }` でも拾えていないといけない。
        first.update_in(cx, |pane, window, cx| {
            pane.activate_item(0, true, true, window, cx);
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, cx| {
            assert_eq!(panel.active_pane, first);
            assert_eq!(
                panel
                    .active_view(cx)
                    .expect("the clicked pane has a browser tab")
                    .read(cx)
                    .current_url(),
                "https://example.com/one"
            );
        });

        // 「+」ボタンはそのペインに足し、そのペインが宛先になる。
        panel.update_in(cx, |panel, window, cx| {
            panel.open_tab_in_pane(
                &second,
                Some("https://example.com/plus".to_string()),
                NewTabPlacement::Foreground,
                window,
                cx,
            );
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, cx| {
            assert_eq!(panel.active_pane, second);
            assert_eq!(
                pane_urls(&second, cx),
                vec![
                    "https://example.com/two".to_string(),
                    "https://example.com/plus".to_string()
                ]
            );
            assert_eq!(
                pane_urls(&first, cx),
                vec!["https://example.com/one".to_string()],
                "the tab must not land in the other pane"
            );
        });
    }

    #[gpui::test]
    async fn a_tab_dropped_on_the_edge_splits_the_panel_not_the_center(cx: &mut TestAppContext) {
        init_test(cx);
        let (window_handle, panel) = init_workspace_with_panel(cx).await;
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        open_tabs(
            &panel,
            &["https://example.com/one", "https://example.com/two"],
            cx,
        );

        let pane = panel.read_with(cx, |panel, _| panel.active_pane.clone());
        pane.update_in(cx, |pane, window, cx| {
            let item = pane
                .item_for_index(1)
                .expect("two tabs are open")
                .boxed_clone();
            // 端に落とした = `handle_drag_move` が向きを立てた状態。
            pane.drag_split_direction = Some(SplitDirection::Right);
            let dragged = DraggedTab {
                pane: cx.entity(),
                item,
                ix: 1,
                detail: 0,
                is_active: true,
            };
            pane.handle_tab_drop(&dragged, 1, true, window, cx);
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, cx| {
            let panes = panel.center.panes();
            assert_eq!(panes.len(), 2, "the drop should split the panel");
            assert_eq!(
                pane_urls(panes[0], cx),
                vec!["https://example.com/one".to_string()]
            );
            assert_eq!(
                pane_urls(panes[1], cx),
                vec!["https://example.com/two".to_string()]
            );
        });
        window_handle
            .read_with(cx, |multi_workspace, cx| {
                assert_eq!(
                    multi_workspace.workspace().read(cx).panes().len(),
                    1,
                    "the center must stay whole when a panel tab is dropped on an edge"
                );
            })
            .expect("failed to read the workspace panes");
    }

    #[gpui::test]
    async fn dropping_an_html_file_on_the_edge_splits_the_panel_not_the_center(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let (window_handle, panel) = init_workspace_with_panel(cx).await;
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        open_tabs(&panel, &["https://example.com/one"], cx);

        let pane = panel.read_with(cx, |panel, _| panel.active_pane.clone());
        let dropped = if cfg!(windows) {
            PathBuf::from(r"C:\tmp\page.html")
        } else {
            PathBuf::from("/tmp/page.html")
        };
        pane.update_in(cx, |pane, window, cx| {
            // 端に落とした = `handle_drag_move` が向きを立てた状態。
            pane.drag_split_direction = Some(SplitDirection::Right);
            let item = pane.active_item().expect("the browser tab is open");
            let paths = ExternalPaths(vec![dropped].into());
            assert!(
                item.handle_drop(pane, &paths, window, cx),
                "an HTML file dropped on a browser pane should be opened as a page"
            );
        });
        cx.run_until_parked();

        panel.read_with(cx, |panel, cx| {
            let panes = panel.center.panes();
            assert_eq!(panes.len(), 2, "the drop should split the panel");
            assert_eq!(
                pane_urls(panes[0], cx),
                vec!["https://example.com/one".to_string()]
            );
            let opened = pane_urls(panes[1], cx);
            assert_eq!(opened.len(), 1);
            assert!(
                opened[0].starts_with("file://") && opened[0].ends_with("page.html"),
                "an HTML file should open as a browser tab, got {:?}",
                opened[0]
            );
        });
        window_handle
            .read_with(cx, |multi_workspace, cx| {
                assert_eq!(
                    multi_workspace.workspace().read(cx).panes().len(),
                    1,
                    "the center must stay whole when a file is dropped on a panel edge"
                );
            })
            .expect("failed to read the workspace panes");
    }

    #[gpui::test]
    async fn the_panel_layout_is_restored_after_a_restart(cx: &mut TestAppContext) {
        init_test(cx);
        let (window_handle, panel) = init_workspace_with_panel(cx).await;
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);
        let workspace_id = WorkspaceId::from_i64(4201);

        open_tabs(
            &panel,
            &["https://example.com/one", "https://example.com/two"],
            cx,
        );
        panel.update_in(cx, |panel, window, cx| {
            panel.split_active_tab_right(window, cx);
            panel.open_tab(
                Some("https://example.com/three".to_string()),
                NewTabPlacement::Foreground,
                window,
                cx,
            );
        });
        cx.run_until_parked();
        store_browser_urls(&panel, workspace_id, cx).await;

        let serialized = panel.update(cx, |panel, cx| SerializedBrowserPanel {
            root: serialize_pane_group(
                &panel.center.root,
                &panel.active_pane,
                PanelLayoutFormat::Items,
                cx,
            ),
        });
        let round_tripped: SerializedBrowserPanel = serde_json::from_str(
            &serde_json::to_string(&serialized).expect("the layout should serialize"),
        )
        .expect("the layout should deserialize");

        let restored = new_panel(&window_handle, cx);
        let mut async_cx = async_window(cx);
        assert!(restore(&restored, round_tripped, Some(workspace_id), &mut async_cx).await);
        cx.run_until_parked();

        restored.read_with(cx, |panel, cx| {
            let panes = panel.center.panes();
            assert_eq!(panes.len(), 2);
            assert_eq!(
                pane_urls(panes[0], cx),
                vec!["https://example.com/one".to_string()]
            );
            assert_eq!(
                pane_urls(panes[1], cx),
                vec![
                    "https://example.com/two".to_string(),
                    "https://example.com/three".to_string()
                ]
            );
            assert_eq!(panes[1].read(cx).active_item_index(), 1);
        });
    }

    #[gpui::test]
    async fn tabs_of_every_kind_in_the_panel_are_restored(cx: &mut TestAppContext) {
        init_test(cx);
        let (window_handle, panel) = init_workspace_with_panel(cx).await;
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);
        let workspace_id = WorkspaceId::from_i64(4202);

        open_tabs(&panel, &["https://example.com/one"], cx);
        // ドラッグでブラウザー以外のタブが載った状態。
        panel.update_in(cx, |panel, window, cx| {
            let item = cx.new(TestItem::new);
            panel.active_pane.update(cx, |pane, cx| {
                pane.add_item(Box::new(item), false, false, None, window, cx);
            });
        });
        cx.run_until_parked();
        store_browser_urls(&panel, workspace_id, cx).await;

        let mut serialized = panel.update(cx, |panel, cx| SerializedBrowserPanel {
            root: serialize_pane_group(
                &panel.center.root,
                &panel.active_pane,
                PanelLayoutFormat::Items,
                cx,
            ),
        });
        let SerializedBrowserPaneGroup::Pane(serialized_pane) = &mut serialized.root else {
            panic!("the panel has a single pane");
        };
        assert_eq!(
            serialized_pane
                .items
                .iter()
                .map(|item| item.kind.as_ref())
                .collect::<Vec<_>>(),
            vec!["Browser", "TestItem"],
            "every restorable tab is written, not just the browser ones"
        );
        assert!(
            serialized_pane.urls.is_empty(),
            "the url-only format is not written any more"
        );
        // 知らない種類 (別の版が保存した) は読み飛ばすだけ。
        serialized_pane.items.push(SerializedPanelItem {
            kind: Arc::from("SomethingThisBuildDoesNotKnow"),
            item_id: 987654,
            active: false,
            preview: false,
        });

        let restored = new_panel(&window_handle, cx);
        let mut async_cx = async_window(cx);
        assert!(restore(&restored, serialized, Some(workspace_id), &mut async_cx).await);
        cx.run_until_parked();

        restored.read_with(cx, |panel, cx| {
            let panes = panel.center.panes();
            assert_eq!(panes.len(), 1);
            assert_eq!(pane_kinds(panes[0], cx), vec!["Browser", "TestItem"]);
            assert_eq!(
                pane_urls(panes[0], cx),
                vec!["https://example.com/one".to_string()]
            );
        });
    }

    #[gpui::test]
    async fn the_url_only_layout_of_the_previous_build_still_restores(cx: &mut TestAppContext) {
        init_test(cx);
        let (window_handle, panel) = init_workspace_with_panel(cx).await;
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);
        drop(panel);

        let stored = r#"{"root":{"Pane":{"active":true,"urls":["https://example.com/one","https://example.com/two"],"active_item":1}}}"#;
        let serialized: SerializedBrowserPanel =
            serde_json::from_str(stored).expect("the old layout should still deserialize");

        let restored = new_panel(&window_handle, cx);
        let mut async_cx = async_window(cx);
        // 旧形式はワークスペース ID を要らない (URL がその場に書いてある)。
        assert!(restore(&restored, serialized, None, &mut async_cx).await);
        cx.run_until_parked();

        restored.read_with(cx, |panel, cx| {
            let panes = panel.center.panes();
            assert_eq!(panes.len(), 1);
            assert_eq!(
                pane_urls(panes[0], cx),
                vec![
                    "https://example.com/one".to_string(),
                    "https://example.com/two".to_string()
                ]
            );
            assert_eq!(panes[0].read(cx).active_item_index(), 1);
        });

        // 次の保存で新形式に移る。
        let reserialized = restored.update(cx, |panel, cx| {
            serialize_pane_group(
                &panel.center.root,
                &panel.active_pane,
                PanelLayoutFormat::Items,
                cx,
            )
        });
        let SerializedBrowserPaneGroup::Pane(pane) = &reserialized else {
            panic!("the restored panel has a single pane");
        };
        assert!(pane.urls.is_empty());
        assert_eq!(
            pane.items
                .iter()
                .map(|item| item.kind.as_ref())
                .collect::<Vec<_>>(),
            vec!["Browser", "Browser"]
        );
        assert!(pane.items[1].active);
    }

    #[gpui::test]
    async fn a_window_without_a_workspace_id_writes_the_url_list(cx: &mut TestAppContext) {
        init_test(cx);
        let (window_handle, panel) = init_workspace_with_panel(cx).await;
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        // テストのワークスペースは DB の ID を持たない = 切り離しウィンドウと同じ。
        // 種類ごとの行を書く先が無いので、URL の並びで書かないと戻せなくなる。
        let format = window_handle
            .read_with(cx, |multi_workspace, cx| {
                PanelLayoutFormat::of(multi_workspace.workspace().read(cx))
            })
            .expect("the window should hold a workspace");
        assert!(format == PanelLayoutFormat::Urls);

        open_tabs(
            &panel,
            &["https://example.com/one", "https://example.com/two"],
            cx,
        );
        let root = panel.update(cx, |panel, cx| {
            serialize_pane_group(&panel.center.root, &panel.active_pane, format, cx)
        });
        let SerializedBrowserPaneGroup::Pane(serialized_pane) = &root else {
            panic!("the panel has a single pane");
        };
        assert!(serialized_pane.items.is_empty());
        assert_eq!(
            serialized_pane.urls,
            vec![
                "https://example.com/one".to_string(),
                "https://example.com/two".to_string()
            ]
        );
        assert_eq!(serialized_pane.active_item, Some(1));

        // 同じセッションのうちはそのまま戻る。
        let restored = new_panel(&window_handle, cx);
        let mut async_cx = async_window(cx);
        assert!(
            restore(
                &restored,
                SerializedBrowserPanel { root },
                None,
                &mut async_cx
            )
            .await
        );
        cx.run_until_parked();
        restored.read_with(cx, |panel, cx| {
            assert_eq!(
                pane_urls(&panel.active_pane, cx),
                vec![
                    "https://example.com/one".to_string(),
                    "https://example.com/two".to_string()
                ]
            );
            assert_eq!(panel.active_pane.read(cx).active_item_index(), 1);
        });
    }

    /// 各ペインのアクティブなビューがブックマークバーを出しているか。ブラウザーで
    /// ないタブを見ているペインは `false` になる。
    fn bookmark_bar_per_pane(
        panel: &Entity<BrowserPanel>,
        cx: &mut VisualTestContext,
    ) -> Vec<bool> {
        let views = panel.read_with(cx, |panel, cx| {
            panel
                .center
                .panes()
                .into_iter()
                .map(|pane| {
                    pane.read(cx)
                        .active_item()
                        .and_then(|item| item.downcast::<BrowserItem>())
                        .map(|item| item.read(cx).view().clone())
                })
                .collect::<Vec<_>>()
        });
        views
            .into_iter()
            .map(|view| {
                view.is_some_and(|view| view.update(cx, |view, cx| view.shows_bookmark_bar(cx)))
            })
            .collect()
    }

    #[gpui::test]
    async fn the_bookmark_bar_follows_the_panel_s_active_pane(cx: &mut TestAppContext) {
        init_test(cx);
        let (window_handle, panel) = init_workspace_with_panel(cx).await;
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        let fs = panel.read_with(cx, |panel, _| panel.fs.clone());
        cx.update(|_, cx| {
            let bookmarks = BookmarkStore::global(fs, cx);
            bookmarks.update(cx, |bookmarks, cx| {
                bookmarks
                    .write(
                        BookmarksFile {
                            items: vec![BookmarkNode::bookmark("例", "https://example.com/")],
                        },
                        cx,
                    )
                    .detach();
            });
        });

        let (first, second) = split_panel(&window_handle, &panel, cx);
        // 分割で切り出した右のペインがアクティブ。バーはそのビューの中だけ。
        panel.read_with(cx, |panel, _| assert_eq!(panel.active_pane, second));
        assert_eq!(
            bookmark_bar_per_pane(&panel, cx),
            vec![false, true],
            "only the active pane's view shows the bookmark bar"
        );

        // 宛先のペインが移るとバーも移る。
        panel.update_in(cx, |_, window, cx| {
            window.focus(&first.focus_handle(cx), cx);
        });
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| assert_eq!(panel.active_pane, first));
        assert_eq!(bookmark_bar_per_pane(&panel, cx), vec![true, false]);

        // ブラウザーでないタブを落としてもパネルの枠組みは動かない (そのタブに
        // バーが無いだけ)。前のビューにバーが残らないことも見る。
        panel.update_in(cx, |panel, window, cx| {
            let item = cx.new(TestItem::new);
            panel.active_pane.update(cx, |pane, cx| {
                pane.add_item(Box::new(item), false, false, None, window, cx);
            });
        });
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
        assert_eq!(bookmark_bar_per_pane(&panel, cx), vec![false, false]);
    }

    #[gpui::test]
    async fn moving_a_tab_to_the_center_takes_it_out_of_the_panel(cx: &mut TestAppContext) {
        init_test(cx);
        let (window_handle, panel) = init_workspace_with_panel(cx).await;
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        open_tabs(&panel, &["https://example.com/one"], cx);

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    BrowserPanel::move_active_tab_to_center(workspace, window, cx);
                })
            })
            .expect("failed to move the browser tab to the center");
        cx.run_until_parked();

        panel.read_with(cx, |panel, cx| {
            assert_eq!(panel.active_pane.read(cx).items_len(), 0);
        });
        let center_view = window_handle
            .read_with(cx, |multi_workspace, cx| {
                let workspace = multi_workspace.workspace().read(cx);
                let center = workspace.active_pane();
                assert_eq!(
                    pane_urls(center, cx),
                    vec!["https://example.com/one".to_string()]
                );
                let item = center
                    .read(cx)
                    .active_item()
                    .and_then(|item| item.downcast::<BrowserItem>())
                    .expect("the browser tab should be the active center item");
                // 中央に来たタブは「エディター領域で開く」を出さない。
                assert_eq!(item.read(cx).view().read(cx).host(), BrowserHost::EditorTab);
                item.read(cx).view().clone()
            })
            .expect("failed to read the center pane");
        // エディター領域のタブは自分のバーを出す (パネルの「1 本だけ」に縛られない)。
        assert!(center_view.update(cx, |view, cx| view.shows_bookmark_bar(cx)));
    }

    /// ワークスペースに DB の ID を与えて、掃除が走る状態にする。
    fn workspace_with_database_id(
        window_handle: &gpui::WindowHandle<MultiWorkspace>,
        cx: &mut VisualTestContext,
    ) -> (Entity<Workspace>, WorkspaceId) {
        let workspace = window_handle
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("the window should hold a workspace");
        let workspace_id = workspace.update(cx, |workspace, _| {
            workspace.set_random_database_id();
            workspace.database_id()
        });
        (
            workspace,
            workspace_id.expect("the workspace should have a database id now"),
        )
    }

    fn browser_item_ids(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<ItemId> {
        workspace.read_with(cx, |workspace, cx| {
            workspace
                .all_panes(cx)
                .into_iter()
                .flat_map(|pane| {
                    pane.read(cx)
                        .items()
                        .filter(|item| item.downcast::<BrowserItem>().is_some())
                        .map(|item| item.item_id().as_u64())
                        .collect::<Vec<ItemId>>()
                })
                .collect::<Vec<_>>()
        })
    }

    #[gpui::test]
    async fn the_cleanup_spares_tabs_that_live_outside_the_panel(cx: &mut TestAppContext) {
        init_test(cx);
        let (window_handle, panel) = init_workspace_with_panel(cx).await;
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);
        let (workspace, workspace_id) = workspace_with_database_id(&window_handle, cx);

        // 同じ種類のタブがパネルと中央に 1 枚ずつ (ADR-F37 の交差する後始末)。
        open_tabs(&panel, &["https://example.com/panel"], cx);
        workspace.update_in(cx, |workspace, window, cx| {
            BrowserItem::open(
                workspace,
                Some("https://example.com/center".to_string()),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let alive = browser_item_ids(&workspace, cx);
        assert_eq!(alive.len(), 2, "one tab in the panel and one in the center");
        let stale_item_id: ItemId = 987_654_321;
        let mut rows = alive
            .iter()
            .map(|item_id| (*item_id, "https://example.com/saved".to_string()))
            .collect::<Vec<_>>();
        rows.push((stale_item_id, "https://example.com/stale".to_string()));
        store_urls(rows, workspace_id, cx).await;

        // 中央もパネルも復元が終わった状態。
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.begin_item_restore();
            workspace.end_item_restore(window, cx);
        });
        cx.run_until_parked();

        let db = cx.update(|_, cx| BrowserItemDb::global(cx));
        for item_id in alive {
            assert!(
                db.get_url(item_id, workspace_id)
                    .expect("reading a browser tab's row")
                    .is_some(),
                "a tab that is alive anywhere in the window keeps its row"
            );
        }
        assert!(
            db.get_url(stale_item_id, workspace_id)
                .expect("reading a browser tab's row")
                .is_none(),
            "the row of a tab that no longer exists is dropped"
        );
    }

    #[gpui::test]
    async fn the_cleanup_waits_until_every_restore_has_finished(cx: &mut TestAppContext) {
        init_test(cx);
        let (window_handle, panel) = init_workspace_with_panel(cx).await;
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);
        let (workspace, workspace_id) = workspace_with_database_id(&window_handle, cx);

        open_tabs(&panel, &["https://example.com/panel"], cx);
        cx.run_until_parked();
        let stale_item_id: ItemId = 123_456_789;
        store_urls(
            vec![(stale_item_id, "https://example.com/stale".to_string())],
            workspace_id,
            cx,
        )
        .await;

        // 中央とパネル、2 つの復元が走っている。
        workspace.update(cx, |workspace, _| {
            workspace.begin_item_restore();
            workspace.begin_item_restore();
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.end_item_restore(window, cx);
        });
        cx.run_until_parked();

        let db = cx.update(|_, cx| BrowserItemDb::global(cx));
        assert!(
            db.get_url(stale_item_id, workspace_id)
                .expect("reading a browser tab's row")
                .is_some(),
            "nothing is deleted while another restore is still running"
        );

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.end_item_restore(window, cx);
        });
        cx.run_until_parked();
        assert!(
            db.get_url(stale_item_id, workspace_id)
                .expect("reading a browser tab's row")
                .is_none(),
            "the last restore to finish runs the cleanup"
        );
    }
}
