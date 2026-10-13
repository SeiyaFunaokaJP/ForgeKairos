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

use anyhow::{Context as _, Result, bail};
use browser_control::{NavigateTarget, PageState, TabId};
use editor::Editor;
use fs::Fs;
use futures::{StreamExt as _, channel::oneshot};
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
    OpenDeveloperTools, OpenInCenter, OpenInExternalBrowser, Reload, ResumeAiControl,
    SetAsHomePage, StopAiControl, SubmitAddress, ToggleBookmarks,
    browser_automation::{
        self, AiNavigationVerdict, AiTabState, AutomationStatus, AutomationSurface, CommandTicket,
        NavigationOutcome,
    },
    normalize_bookmark_url, resolve_address_input,
};

use crate::webview::{
    self, OverlayRect, Viewport, WebView, WebViewEvent, WebViewOptions, WebViewProfile,
};

const WEBVIEW2_DOWNLOAD_URL: &str = "https://developer.microsoft.com/microsoft-edge/webview2/";

/// ブックマークのサイドペインの幅。ツリーの行に「編集 / 上へ / 下へ / 削除」の
/// アイコンボタンが並ぶので、名前が潰れない程度には要る。
const BOOKMARKS_PANE_WIDTH: f32 = 244.;

/// タブに出すページタイトルの上限。長いタイトルでタブが伸び切らないようにする。
const MAX_TAB_TITLE_CHARS: usize = 40;

/// URL もタイトルも無いときのタブ名。
pub(crate) const UNTITLED_TAB_LABEL: &str = i18n::msg!("Browser");

/// AI タブの印 (タブ・ツールバー) に添える説明。
pub(crate) const AI_TAB_TOOLTIP: &str =
    i18n::msg!("AI tab: separate profile, network limited to the allowlist");

/// egress プロキシの起動を待つ AI タブが見に行く間隔。
const EGRESS_PROXY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// AI タブが最初に開くページ。利用者のホームページ (既定は Microsoft 365) へは出ていかない。
pub(crate) const AI_TAB_BLANK_URL: &str = "about:blank";

#[cfg(test)]
type TestSurfaceFactory = Rc<dyn Fn() -> Result<Rc<dyn AutomationSurface>, FailedStart>>;

#[cfg(test)]
thread_local! {
    /// The stand-in that views created from now on start with, for tabs a test cannot reach
    /// before they are created (a tab a command opens).
    static NEW_VIEW_SURFACE_FACTORY: std::cell::RefCell<Option<TestSurfaceFactory>> =
        const { std::cell::RefCell::new(None) };
}

/// How a test's stand-in for creating a page fails.
#[cfg(test)]
pub(crate) enum FailedStart {
    /// The page's browser went away as soon as it started, with the reason.
    Crashed(String),
    /// The browser could not be created at all (what `WebView::new` reports).
    CouldNotStart(String),
}

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
        /// AI のツールに返す英文の理由。`None` なら `message` をそのまま返す。
        detail: Option<String>,
        /// AI タブの面が生成で失敗した。描画からは作り直さないが、次の AI のコマンドが
        /// 1 回だけ作り直す (落ちた面と同じ扱い。一時的な失敗で AI タブを使えなくしない)。
        retry_on_command: bool,
    },
    /// ページのプロセスが落ちて面を捨てた。描画からは作り直さない (落ち続けるページで
    /// 作り直しを繰り返さないため)。利用者の「再読み込み」か、AI タブなら次のコマンドが
    /// 作り直して `current_url` を開き直す。`message` は AI のツールに返す英文の理由。
    Crashed {
        message: String,
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
    /// AI ブラウザーパネル (ADR-F89 追記 10)。AI タブだけが載る。
    AiPanel,
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
    /// AI のコマンドがページに触る入口。`webview` と同じものを指すが、テストでは
    /// 偽物に差し替える。
    automation_surface: Option<Rc<dyn AutomationSurface>>,
    /// 生成時に決まり、以後変わらない。AI タブは別プロファイル・プロキシ必須・
    /// ナビゲーションの事前検査付きで作る (ADR-F89)。
    profile: WebViewProfile,
    /// AI タブだけが持つ状態。`profile == Ai` のときに限り `Some`。
    ai: Option<AiTabState>,
    /// サーフェスを作ったときのホストウィンドウ。タブを別ウィンドウへ移すと
    /// 子 HWND は元のウィンドウにぶら下がったままなので、変わったら作り直す。
    host_hwnd: Option<isize>,
    surface: SurfaceState,
    /// 面が一度でも描画されたか。AI タブの面は描画より先にコマンドから作られることがあり
    /// ([`Self::ensure_background_surface`])、その間は既定の大きさで裏に置いておく。
    surface_painted: Rc<std::cell::Cell<bool>>,
    warned_missing_window_handle: bool,
    current_url: String,
    /// ホームページを「ユーザーがブラウザーに向くまで」待たせているタブの
    /// 行き先 ([`Self::defer_home_page`])。空文字は `browser.home_url` が空の
    /// ときで、待機が解ければブックマークのペインを開く (新しいタブと同じ)。
    pending_home_url: Option<String>,
    page_title: SharedString,
    status_message: Option<SharedString>,
    /// 直前のナビゲーションがフィルターに止められた。WebView2 は止めた行き先についても
    /// 失敗の `LoadFinished` を後から送り (WebKit もリダイレクトを止めたときは同じ)、
    /// そのときも元のページは残っているので、「読み込めませんでした」で遮断の通知を
    /// 上書きしない。
    navigation_was_blocked: bool,
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
    /// AI タブが egress プロキシの起動を待つ間だけ動く。プロキシは許可リストのファイルを
    /// 読んだ後に (起動直後は少し遅れて) 立ち上がるので、それまでに描かれたタブを
    /// 「開けない」のまま置き去りにしない。
    proxy_wait_task: Option<Task<()>>,
    /// Tests have no native window to host a page in; this stands in for creating one.
    #[cfg(test)]
    test_surface_factory: Option<TestSurfaceFactory>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<BrowserViewEvent> for BrowserView {}

impl BrowserView {
    pub(crate) fn new(
        workspace: WeakEntity<Workspace>,
        fs: Arc<dyn Fs>,
        host: BrowserHost,
        profile: WebViewProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let address_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(i18n::t!("Enter a URL or search terms"), window, cx);
            editor
        });

        let bookmarks = BookmarkStore::global(fs.clone(), cx);
        let focus_handle = cx.focus_handle();

        let mut subscriptions = Vec::new();
        // ショートカットなどで GPUI のフォーカスがビューの外へ移ったら、キーボードも
        // ページから取り上げる (macOS ではページが握ったままだとキー入力がページへ行く)。
        subscriptions.push(cx.on_focus_out(&focus_handle, window, |this, _, _, _| {
            if let Some(webview) = &this.webview {
                webview.release_keyboard_focus();
            }
        }));
        subscriptions.push(cx.observe_global::<SettingsStore>(|this, cx| {
            this.sync_appearance(cx);
            cx.notify();
        }));
        subscriptions.push(cx.observe(&bookmarks, |_, _, cx| cx.notify()));

        Self {
            workspace,
            fs,
            host,
            focus_handle,
            address_editor,
            webview: None,
            automation_surface: None,
            profile,
            ai: (profile == WebViewProfile::Ai).then(AiTabState::new),
            host_hwnd: None,
            surface: SurfaceState::Idle,
            surface_painted: Rc::default(),
            warned_missing_window_handle: false,
            current_url: String::new(),
            pending_home_url: None,
            page_title: SharedString::default(),
            status_message: None,
            navigation_was_blocked: false,
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
            proxy_wait_task: None,
            #[cfg(test)]
            test_surface_factory: NEW_VIEW_SURFACE_FACTORY.with(|factory| factory.borrow().clone()),
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

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// AI のツールに見せる読み込み状態。復元しただけのタブは面が無いまま行き先を
    /// 持って待っている (`loading` も立っている) ので、「読み込み中」とは言わない。
    pub(crate) fn page_state(&self) -> PageState {
        match self.surface {
            SurfaceState::Ready if self.loading => PageState::Loading,
            SurfaceState::Ready => PageState::Loaded,
            SurfaceState::Starting => PageState::Loading,
            SurfaceState::Idle
            | SurfaceState::Unavailable { .. }
            | SurfaceState::Crashed { .. } => PageState::NotLoaded,
        }
    }

    // -- AI タブ (ADR-F89) ---------------------------------------------------

    pub(crate) fn profile(&self) -> WebViewProfile {
        self.profile
    }

    pub fn is_ai_tab(&self) -> bool {
        self.profile == WebViewProfile::Ai
    }

    pub(crate) fn ai_tab_id(&self) -> Option<&TabId> {
        self.ai.as_ref()?.id.as_ref()
    }

    pub(crate) fn set_ai_tab_id(&mut self, id: TabId) {
        if let Some(ai) = &mut self.ai {
            ai.id = Some(id);
        }
    }

    pub(crate) fn ai_last_used(&self) -> u64 {
        self.ai.as_ref().map_or(0, |ai| ai.last_used)
    }

    /// このタブを「今の AI タブ」(ツールが `tab` を省いたときの宛先) にする。
    pub(crate) fn touch_ai_tab(&mut self, cx: &mut Context<Self>) {
        if let Some(ai) = &mut self.ai {
            ai.last_used = browser_automation::next_use_stamp();
            cx.notify();
        }
    }

    pub(crate) fn begin_ai_command(&mut self, cx: &mut Context<Self>) -> Result<CommandTicket> {
        let ai = self.ai.as_mut().context("This tab is not an AI tab.")?;
        let ticket = ai.begin_command()?;
        cx.notify();
        Ok(ticket)
    }

    pub(crate) fn end_ai_command(&mut self, command_id: u64, cx: &mut Context<Self>) {
        if let Some(ai) = &mut self.ai {
            ai.end_command(command_id);
            cx.notify();
        }
    }

    pub(crate) fn ai_blocked_since(&self, start: usize) -> Vec<String> {
        self.ai
            .as_ref()
            .map(|ai| ai.blocked_since(start).to_vec())
            .unwrap_or_default()
    }

    pub(crate) fn ai_is_paused(&self) -> bool {
        self.ai.as_ref().is_some_and(|ai| ai.paused)
    }

    #[cfg(test)]
    pub(crate) fn ai_is_running(&self) -> bool {
        self.ai.as_ref().is_some_and(AiTabState::is_running)
    }

    pub(crate) fn automation_status(&self) -> AutomationStatus {
        match (&self.surface, &self.automation_surface) {
            (
                SurfaceState::Unavailable {
                    message,
                    detail,
                    retry_on_command,
                    ..
                },
                _,
            ) => {
                let reason = detail.clone().unwrap_or_else(|| message.to_string());
                if *retry_on_command {
                    AutomationStatus::Restartable(reason)
                } else {
                    AutomationStatus::Unavailable(reason)
                }
            }
            (SurfaceState::Ready, Some(surface)) => AutomationStatus::Ready(surface.clone()),
            (SurfaceState::Crashed { message }, _) => {
                AutomationStatus::Restartable(message.clone())
            }
            (state, surface) => AutomationStatus::Waiting {
                has_surface: surface.is_some() || matches!(state, SurfaceState::Starting),
            },
        }
    }

    /// AI のコマンドからのナビゲーション。読み込みの終わり (または遮断) を待つ受け口を返す。
    /// 行き先は呼び出し側が許可リストで確かめ済み。ページが自分で動く分はバックエンドの
    /// フィルターが止める。
    pub(crate) fn ai_navigate(
        &mut self,
        surface: &Rc<dyn AutomationSurface>,
        target: &NavigateTarget,
        cx: &mut Context<Self>,
    ) -> Result<oneshot::Receiver<NavigationOutcome>> {
        match target {
            NavigateTarget::Back if !self.can_go_back => {
                bail!("There is no previous page in this AI tab's history.")
            }
            NavigateTarget::Forward if !self.can_go_forward => {
                bail!("There is no next page in this AI tab's history.")
            }
            _ => {}
        }
        self.navigation_was_blocked = false;
        let ai = self.ai.as_mut().context("This tab is not an AI tab.")?;
        let waiter = ai.expect_navigation();
        ai.requested_navigation = None;
        match target {
            NavigateTarget::Url(url) => {
                self.pending_home_url = None;
                self.status_message = None;
                ai.requested_navigation = Some(url.clone());
                surface.navigate(url);
            }
            NavigateTarget::Back => surface.go_back(),
            NavigateTarget::Forward => surface.go_forward(),
            NavigateTarget::Reload => surface.reload(),
        }
        self.loading = true;
        cx.notify();
        Ok(waiter)
    }

    pub(crate) fn stop_ai_control(
        &mut self,
        _: &StopAiControl,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ai) = &mut self.ai {
            ai.stop();
            cx.notify();
        }
    }

    pub(crate) fn resume_ai_control(
        &mut self,
        _: &ResumeAiControl,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ai) = &mut self.ai {
            ai.paused = false;
            cx.notify();
        }
    }

    fn open_developer_tools(
        &mut self,
        _: &OpenDeveloperTools,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &self.webview {
            Some(webview) => webview.open_developer_tools(),
            None => self.set_status_message(i18n::t!("Open a page first."), cx),
        }
    }

    /// テストではウィンドウにページを載せられないので、偽の入口を据えて「起動済み」にする。
    #[cfg(test)]
    pub(crate) fn set_automation_surface_for_test(
        &mut self,
        surface: Rc<dyn AutomationSurface>,
        cx: &mut Context<Self>,
    ) {
        self.automation_surface = Some(surface);
        self.surface = SurfaceState::Ready;
        cx.notify();
    }

    /// Makes surface creation (from painting or from a command) hand out `factory`'s surfaces,
    /// as if the window could host a page.
    #[cfg(test)]
    pub(crate) fn set_surface_factory_for_test(&mut self, factory: TestSurfaceFactory) {
        self.test_surface_factory = Some(factory);
    }

    /// [`Self::set_surface_factory_for_test`] for every view created afterwards on this thread.
    #[cfg(test)]
    pub(crate) fn set_surface_factory_for_new_views_for_test(factory: TestSurfaceFactory) {
        NEW_VIEW_SURFACE_FACTORY.with(|new_view_factory| {
            *new_view_factory.borrow_mut() = Some(factory);
        });
    }

    #[cfg(test)]
    pub(crate) fn set_surface_unavailable_for_test(
        &mut self,
        message: SharedString,
        cx: &mut Context<Self>,
    ) {
        self.automation_surface = None;
        self.surface = SurfaceState::Unavailable {
            message,
            offer_runtime_download: false,
            detail: None,
            retry_on_command: false,
        };
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn status_message_for_test(&self) -> Option<SharedString> {
        self.status_message.clone()
    }

    /// バックエンドからのイベントを、テストから直接流す。
    #[cfg(test)]
    pub(crate) fn handle_webview_event_for_test(
        &mut self,
        event: WebViewEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.handle_webview_event(event, window, cx);
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
        let host_hwnd = webview::host_window_handle(window);
        if let (Some(current), Some(previous)) = (host_hwnd, self.host_hwnd)
            && current != previous
        {
            // タブが別ウィンドウへ移った。子 HWND は元のウィンドウの下にいるので
            // 作り直す。`event_task` をここで落とせるのは、この関数が描画か
            // AI のコマンドからしか呼ばれない (= タスクの中ではない) ため。
            log::info!("browser view: the host window changed; recreating the browsing surface");
            self.discard_surface();
            self.event_task = None;
            self.surface = SurfaceState::Idle;
        }

        if self.webview.is_some() || !matches!(self.surface, SurfaceState::Idle) {
            return;
        }

        #[cfg(test)]
        if let Some(factory) = self.test_surface_factory.clone() {
            match factory() {
                Ok(surface) => {
                    if !self.current_url.is_empty() {
                        surface.navigate(&self.current_url);
                    }
                    self.automation_surface = Some(surface);
                    self.surface = SurfaceState::Ready;
                }
                Err(FailedStart::Crashed(detail)) => {
                    self.surface = SurfaceState::Crashed {
                        message: webview::crashed_page_message(&detail),
                    };
                }
                Err(FailedStart::CouldNotStart(detail)) => self.set_surface_failed(detail),
            }
            cx.notify();
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
        self.start_surface(host_hwnd, window, cx);
    }

    /// AI のコマンドが、まだ描画されたことのない AI タブ (裏のタブ・閉じたパネル・畳んだ
    /// ドック・最小化したウィンドウ) の面を求めたときに、描画を待たずに作る。作るのは
    /// 描画の経路と同じ面で、`window` はタブが載るワークスペースのウィンドウ。描画される
    /// までは見えない場所に既定の大きさで置いたまま描かせておき (バックエンドの「待避」)、
    /// 後で描画されれば同じ面がその場所へ置き直される。落ちた面はここで作り直す。
    pub(crate) fn ensure_background_surface(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.is_ai_tab() {
            return;
        }
        if matches!(
            self.surface,
            SurfaceState::Crashed { .. }
                | SurfaceState::Unavailable {
                    retry_on_command: true,
                    ..
                }
        ) {
            log::info!("browser view: restarting the AI tab's browser for a command");
            self.surface = SurfaceState::Idle;
        }
        self.ensure_surface(window, cx);
        if self.surface_painted.get() {
            return;
        }
        if let Some(webview) = &self.webview {
            let deferred = webview.sync_viewport(
                Viewport {
                    origin_x: 0.,
                    origin_y: 0.,
                    width: webview::BACKGROUND_VIEWPORT_WIDTH,
                    height: webview::BACKGROUND_VIEWPORT_HEIGHT,
                    scale_factor: window.scale_factor(),
                    hidden_reason: Some(webview::PARKED_IN_BACKGROUND),
                    input_blocked: false,
                    overlays: Vec::new(),
                },
                false,
            );
            // Nothing is cut out of a page that is out of sight; should a backend still ask
            // for it, there is no frame to wait for.
            if deferred {
                webview.apply_deferred_clip();
            }
        }
    }

    /// 面を手放す。`event_task` は残す: 面のイベントの処理中に呼ばれることがあり、
    /// タスクが自分自身を落としてはいけないため (次に面を作るときに差し替わる)。
    fn discard_surface(&mut self) {
        self.webview = None;
        self.automation_surface = None;
        self.host_hwnd = None;
        self.surface_painted.set(false);
        self.applied_dark_mode = None;
        self.applied_background_color = None;
    }

    /// 利用者の「再読み込み」で、落ちた面を作り直す。次の描画が `current_url` を開き直す。
    fn restart_crashed_surface(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.surface, SurfaceState::Crashed { .. }) {
            return;
        }
        log::info!("browser view: restarting the browser after the page stopped");
        self.surface = SurfaceState::Idle;
        self.status_message = None;
        cx.notify();
    }

    fn start_surface(&mut self, host_hwnd: isize, window: &mut Window, cx: &mut Context<Self>) {
        self.host_is_composited = webview::host_uses_direct_composition(host_hwnd);

        let Some(options) = self.surface_options(cx) else {
            // AI タブはプロキシ無しでは作らない (直結すると AI の通信が許可リストの
            // 外に出る)。閉じた状態のまま理由を出す。
            log::warn!("browser view: no egress proxy is running, so the AI tab cannot start");
            self.surface = SurfaceState::Unavailable {
                message: i18n::t!(
                    "AI tabs need the network allowlist proxy, which is not running, so this tab cannot open pages."
                )
                .into(),
                offer_runtime_download: false,
                detail: Some(
                    "The network allowlist proxy is not running, so AI tabs cannot open pages \
                     (they never connect directly)."
                        .to_string(),
                ),
                retry_on_command: false,
            };
            self.wait_for_egress_proxy(cx);
            cx.notify();
            return;
        };
        let (events_tx, mut events_rx) = futures::channel::mpsc::unbounded();
        match WebView::new(host_hwnd, options, events_tx) {
            Ok(webview) => {
                let webview = Rc::new(webview);
                let surface: Rc<dyn AutomationSurface> = webview.clone();
                self.automation_surface = Some(surface);
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
                // GTK の main context は誰も回さないので、WebKitGTK のページが
                // 1 つでもある間は gpui のタスクから回す。
                #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                if webview::claim_main_loop_pump() {
                    cx.spawn(async move |_, cx| {
                        while webview::pump_main_loop() {
                            cx.background_executor()
                                .timer(webview::MAIN_LOOP_PUMP_INTERVAL)
                                .await;
                        }
                    })
                    .detach();
                }
            }
            Err(error) => {
                log::error!("failed to start the built-in browser: {error:#}");
                self.set_surface_failed(format!("{error:#}"));
            }
        }
        cx.notify();
    }

    pub(crate) fn wait_for_egress_proxy(&mut self, cx: &mut Context<Self>) {
        self.proxy_wait_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(EGRESS_PROXY_POLL_INTERVAL)
                    .await;
                if http_client::child_process_proxy().is_none() {
                    continue;
                }
                let retried = this.update(cx, |this, cx| {
                    this.proxy_wait_task = None;
                    if matches!(this.surface, SurfaceState::Unavailable { .. }) {
                        // 次の描画で作り直す。
                        this.surface = SurfaceState::Idle;
                        cx.notify();
                    }
                });
                if retried.is_err() {
                    log::debug!(
                        "browser view: the AI tab closed while waiting for the egress proxy"
                    );
                }
                return;
            }
        }));
    }

    /// 面を作れなかった。普段のタブはランタイムの案内を出す。AI タブだけが断られる理由
    /// (プロキシの食い違い・古い macOS など) もあるので、AI タブは原因をそのまま添え、
    /// ツールにも英文のまま返す。
    fn set_surface_failed(&mut self, detail: String) {
        self.surface = match self.profile {
            WebViewProfile::User => SurfaceState::Unavailable {
                message: webview::runtime_missing_message(),
                // ダウンロードページは WebView2 のもの。他の OS には出さない。
                offer_runtime_download: cfg!(windows),
                detail: None,
                retry_on_command: false,
            },
            WebViewProfile::Ai => SurfaceState::Unavailable {
                message: i18n::t!("The AI tab's browser could not start: {detail}").into(),
                offer_runtime_download: false,
                detail: Some(format!("The AI tab's browser could not start: {detail}")),
                retry_on_command: true,
            },
        };
    }

    /// 生成に渡すもの。`None` は「AI タブなのに egress プロキシが動いていない」。
    fn surface_options(&self, cx: &App) -> Option<WebViewOptions> {
        match self.profile {
            WebViewProfile::User => {
                let settings = Self::settings(cx);
                let proxy_endpoint = settings
                    .use_egress_proxy
                    .then(http_client::child_process_proxy)
                    .flatten()
                    .map(|endpoint| browser_automation::proxy_endpoint(&endpoint));
                if settings.use_egress_proxy && proxy_endpoint.is_none() {
                    log::warn!(
                        "browser.use_egress_proxy is on but no egress proxy is running; \
                         the built-in browser will connect directly"
                    );
                }
                Some(WebViewOptions {
                    profile: WebViewProfile::User,
                    user_data_folder: browser_automation::profile_data_folder(WebViewProfile::User),
                    proxy_endpoint,
                    document_start_script: None,
                    navigation_filter: None,
                    block_downloads: false,
                })
            }
            // `browser.use_egress_proxy` に関わらず、AI タブは必ずプロキシを通す。
            WebViewProfile::Ai => browser_automation::ai_web_view_options(
                http_client::child_process_proxy().as_ref(),
                browser_automation::egress_policy(cx),
            ),
        }
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
            WebViewEvent::Crashed(detail) => {
                log::error!("browser view: the page's browser process went away: {detail}");
                self.discard_surface();
                let message = webview::crashed_page_message(&detail);
                self.surface = SurfaceState::Crashed {
                    message: message.clone(),
                };
                self.loading = false;
                if let Some(ai) = &mut self.ai {
                    ai.record_blocked(message.clone());
                    ai.navigation_ended(NavigationOutcome::Blocked { message });
                }
            }
            #[cfg(windows)]
            WebViewEvent::Failed(detail) => {
                log::error!("the built-in browser is unavailable: {detail}");
                // Dropping the surface drops the event sender, which ends the
                // stream this handler is running inside; the task must not be
                // dropped from within itself.
                self.webview = None;
                self.automation_surface = None;
                self.set_surface_failed(detail);
                if let Some(ai) = &mut self.ai {
                    ai.navigation_ended(NavigationOutcome::Finished { succeeded: false });
                }
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
                let navigation_was_blocked = std::mem::take(&mut self.navigation_was_blocked);
                if succeeded {
                    self.status_message = None;
                } else if !navigation_was_blocked {
                    self.status_message = Some(i18n::t!("Couldn't load the page.").into());
                }
                if let Some(ai) = &mut self.ai {
                    ai.navigation_ended(NavigationOutcome::Finished { succeeded });
                }
            }
            WebViewEvent::FocusReturned => {
                self.page_has_focus = false;
                window.focus(&self.focus_handle, cx);
            }
            WebViewEvent::PageFocusChanged(focused) => {
                self.page_has_focus = focused;
                if focused {
                    // ページがキーボードを握ったら GPUI のフォーカスもこのビューへ寄せる。
                    // パネルのペインはペインの切り替えで寄るが、中央のタブはここでしか寄らない。
                    if !self.focus_handle.contains_focused(window, cx) {
                        window.focus(&self.focus_handle, cx);
                    }
                    self.touch_ai_tab(cx);
                    cx.emit(BrowserViewEvent::Focused);
                }
            }
            WebViewEvent::OpenInPlace(url) => self.navigate_to(&url, cx),
            WebViewEvent::NavigationBlocked { url, reason } => {
                log::info!("browser view: blocked a navigation to {url}: {reason}");
                self.loading = false;
                self.status_message = Some(blocked_navigation_notice(&url));
                self.navigation_was_blocked = true;
                if let Some(ai) = &mut self.ai {
                    ai.navigation_blocked(&url, &reason);
                }
            }
            WebViewEvent::DownloadBlocked { url } => {
                log::info!("browser view: blocked a download of {url}");
                self.status_message =
                    Some(i18n::t!("Blocked a download in this AI tab: {url}").into());
                if let Some(ai) = &mut self.ai {
                    ai.record_blocked(browser_automation::blocked_download_message(&url));
                }
            }
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
            let (red, green, blue) = background;
            webview.set_background_color(red, green, blue);
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
        // 人間がアドレス欄に打った行き先も、AI タブでは AI と同じ規則で止める
        // (このタブのプロファイルとページは AI が読めるため)。
        if self.profile == WebViewProfile::Ai
            && browser_automation::ai_navigation_verdict(
                url,
                &browser_automation::egress_policy(cx),
            ) != AiNavigationVerdict::Allowed
        {
            self.status_message = Some(blocked_navigation_notice(url));
            cx.notify();
            return;
        }
        // ユーザーが行き先を選んだ = 待たせてあるホームは用済み (ブックマークを
        // 押した直後にホームが追い掛けてこないように)。
        self.pending_home_url = None;
        self.current_url = url.to_string();
        self.loading = true;
        self.status_message = None;
        self.navigation_was_blocked = false;
        match &self.webview {
            Some(webview) => webview.navigate(url),
            // AI タブの行き先を利用者の既定のブラウザーへ回すと、その先は許可リストの
            // 外になる。サーフェスが出来たときに `current_url` へ飛ぶのを待つ。
            None if self.profile == WebViewProfile::Ai => {}
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
            self.navigation_was_blocked = false;
            webview.go_back();
        }
        cx.notify();
    }

    fn navigate_forward(&mut self, _: &NavigateForward, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(webview) = &self.webview {
            self.navigation_was_blocked = false;
            webview.go_forward();
        }
        cx.notify();
    }

    fn reload(&mut self, _: &Reload, _: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.surface, SurfaceState::Crashed { .. }) {
            self.restart_crashed_surface(cx);
            return;
        }
        match &self.webview {
            Some(webview) => {
                self.loading = true;
                self.navigation_was_blocked = false;
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
            self.set_status_message(i18n::t!("Open a page first."), cx);
            return;
        }
        settings::update_settings_file(self.fs.clone(), cx, {
            let url = url.clone();
            move |settings, _| {
                settings.browser.get_or_insert_default().home_url = Some(url);
            }
        });
        self.set_status_message(i18n::t!("Set the startup page to {url}."), cx);
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
            self.status_message =
                Some(i18n::t!("Bookmarks are still loading. Try again in a moment.").into());
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
                    this.status_message = Some(i18n::t!("Couldn't save the bookmarks.").into());
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
            editor.set_placeholder_text(i18n::tc!("browser bookmark", "Name"), window, cx);
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
                i18n::t!("Enter a name.").into()
            } else {
                i18n::t!("Enter a URL.").into()
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
                            Tooltip::for_action_in(
                                i18n::t!("Back"),
                                &NavigateBack,
                                &focus_handle,
                                cx,
                            )
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
                            Tooltip::for_action_in(
                                i18n::t!("Forward"),
                                &NavigateForward,
                                &focus_handle,
                                cx,
                            )
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
                            Tooltip::for_action_in(i18n::t!("Reload"), &Reload, &focus_handle, cx)
                        }
                    })
                    .on_click(cx.listener(|this, _, window, cx| this.reload(&Reload, window, cx))),
            )
            .when(self.is_ai_tab(), |this| {
                this.child(
                    h_flex()
                        .id("browser-ai-tab-badge")
                        .flex_none()
                        .gap_0p5()
                        .px_1()
                        .rounded_sm()
                        .bg(cx.theme().colors().element_background)
                        .child(
                            Icon::new(IconName::Sparkle)
                                .size(IconSize::XSmall)
                                .color(Color::Accent),
                        )
                        .child(
                            Label::new(i18n::tc!("browser AI tab badge", "AI"))
                                .size(LabelSize::XSmall)
                                .color(Color::Accent),
                        )
                        .tooltip(Tooltip::text(i18n::tr(AI_TAB_TOOLTIP))),
                )
            })
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
                                i18n::t!("Bookmark This Page"),
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
                                i18n::t!("Open This Page at Startup"),
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
                                i18n::tc!("browser bookmark", "Bookmarks"),
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
                                i18n::t!("Move to Editor Area"),
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
                IconButton::new("browser-developer-tools", IconName::Code)
                    .icon_size(IconSize::Small)
                    .tooltip({
                        let focus_handle = focus_handle.clone();
                        move |_, cx| {
                            Tooltip::for_action_in(
                                i18n::t!("Open Developer Tools"),
                                &OpenDeveloperTools,
                                &focus_handle,
                                cx,
                            )
                        }
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_developer_tools(&OpenDeveloperTools, window, cx)
                    })),
            )
            .child(
                IconButton::new("browser-external", IconName::ArrowUpRight)
                    .icon_size(IconSize::Small)
                    .tooltip(move |_, cx| {
                        Tooltip::for_action_in(
                            i18n::t!("Open in External Browser"),
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

    /// 状態行の右端に出す AI 操作の状態と、止める / 再開するボタン。AI タブ以外では出さない。
    fn render_ai_control_state(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let ai = self.ai.as_ref()?;
        let state = if !Self::settings(cx).ai_control {
            h_flex()
                .child(
                    Label::new(i18n::t!("AI control is off"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element()
        } else if ai.paused {
            h_flex()
                .gap_1()
                .child(
                    Label::new(i18n::t!("AI control paused"))
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                )
                .child(
                    Button::new("browser-resume-ai", i18n::t!("Resume"))
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.resume_ai_control(&ResumeAiControl, window, cx)
                        })),
                )
                .into_any_element()
        } else if ai.is_running() {
            h_flex()
                .gap_1()
                .child(
                    Label::new(i18n::t!("AI is operating…"))
                        .size(LabelSize::Small)
                        .color(Color::Accent),
                )
                .child(
                    Button::new("browser-stop-ai", i18n::t!("Stop AI"))
                        .label_size(LabelSize::Small)
                        .start_icon(
                            Icon::new(IconName::Stop)
                                .size(IconSize::XSmall)
                                .color(Color::Error),
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.stop_ai_control(&StopAiControl, window, cx)
                        })),
                )
                .into_any_element()
        } else {
            return None;
        };
        Some(h_flex().flex_none().child(state).into_any_element())
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
            // AI タブで開くのは開発中のアプリ。利用者のブックマークは並べない。
            BrowserHost::AiPanel => false,
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
                                .tooltip(Tooltip::text(i18n::t!("Show Folder Contents")))
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
                        Label::new(i18n::tc!("browser bookmark", "Bookmarks"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(
                        IconButton::new("browser-bookmark-add-folder", IconName::FolderAdd)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text(i18n::t!("Add Folder")))
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
                            .tooltip(Tooltip::text(i18n::t!("Close Bookmarks")))
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
                                    i18n::t!("No bookmarks yet. Use the star button in the toolbar to add the current page.")
                                } else {
                                    i18n::t!("Loading…")
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
                        i18n::t!("Delete the folder and everything in it?")
                    } else {
                        i18n::t!("Delete?")
                    })
                    .size(LabelSize::Small),
                )
                .child(div().flex_1())
                .child(
                    Button::new(
                        ("browser-bookmark-confirm-delete", index),
                        i18n::t!("Delete"),
                    )
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let path = path.clone();
                        this.update_bookmarks(cx, move |file| file.remove(&path).is_some());
                    })),
                )
                .child(
                    Button::new(
                        ("browser-bookmark-cancel-delete", index),
                        i18n::tc!("browser bookmark", "Cancel"),
                    )
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
                    .tooltip(Tooltip::text(i18n::t!("Edit")))
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
                    .tooltip(Tooltip::text(i18n::tc!("browser bookmark", "Move Up")))
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
                    .tooltip(Tooltip::text(i18n::tc!("browser bookmark", "Move Down")))
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
                    .tooltip(Tooltip::text(i18n::t!("Delete")))
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
            (Some(_), true) => i18n::t!("Edit Folder"),
            (Some(_), false) => i18n::tc!("browser bookmark", "Edit Bookmark"),
            (None, true) => i18n::t!("Add Folder"),
            (None, false) => i18n::tc!("browser bookmark", "Add Bookmark"),
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
        let mut folder_buttons = vec![(i18n::t!("Top Level").to_string(), 0usize, Vec::new())];
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
                    Label::new(i18n::tc!("browser bookmark", "Location"))
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
                            Button::new("browser-bookmark-save", i18n::t!("Save"))
                                .label_size(LabelSize::Small)
                                .full_width()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.submit_bookmark_form(window, cx)
                                })),
                        )
                        .child(
                            Button::new(
                                "browser-bookmark-cancel",
                                i18n::tc!("browser bookmark", "Cancel"),
                            )
                            .label_size(LabelSize::Small)
                            .full_width()
                            .on_click(cx.listener(
                                |this, _, window, cx| {
                                    this.bookmark_form = None;
                                    window.focus(&this.focus_handle, cx);
                                    cx.notify();
                                },
                            )),
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
                ..
            } => self.render_unavailable(message.clone(), *offer_runtime_download, cx),
            SurfaceState::Crashed { .. } => self.render_crashed(cx),
            SurfaceState::Idle | SurfaceState::Starting | SurfaceState::Ready => {
                let presentation = self.surface_presentation(window, cx);
                // 穴を開けてよいのは実際にページが載っているときだけ。ページの
                // 無い矩形を透明にすると、ウィンドウの背後 (デスクトップ) が
                // そのまま見えてしまう。
                // Wayland ではページが別ウィンドウに出る (他のクライアントの面を
                // 埋め込めない)。パネルには案内と、そのウィンドウを前に出すボタンを置く。
                let detached = self
                    .webview
                    .as_ref()
                    .is_some_and(|webview| webview.is_detached());
                let surface_is_up = matches!(self.surface, SurfaceState::Ready) && !detached;
                // ドラッグプレビューは毎フレーム動くので、切り抜くと 1 フレームに
                // 1 回 `SetWindowRgn` を撃つことになり、プレビューの周りに継ぎ目が
                // 出る。ドラッグ中はウィンドウがマウスをキャプチャしていて子 HWND
                // にクリックが渡らないので、そもそも切り抜く必要がない。
                let cut_overlays = !cx.has_active_drag();
                let webview = self.webview.clone();
                let focus_handle = self.focus_handle.clone();
                let surface_painted = self.surface_painted.clone();
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
                            .when(
                                matches!(self.surface, SurfaceState::Starting) && !detached,
                                |this| {
                                    this.child(
                                        Label::new(i18n::t!("Starting the built-in browser…"))
                                            .color(Color::Muted),
                                    )
                                },
                            )
                            .when(detached, |this| {
                                this.child(
                                    Label::new(i18n::t!("Shown in a separate window"))
                                        .color(Color::Muted),
                                )
                                .child(
                                    Button::new("browser-raise-detached", i18n::t!("Show Window"))
                                        .on_click(cx.listener(|this, _, _, _| {
                                            if let Some(webview) = &this.webview {
                                                webview.raise_detached_window();
                                            }
                                        })),
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
                                surface_painted.set(true);
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

    fn render_crashed(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .p_4()
            .child(Label::new(i18n::t!("This page stopped working.")).color(Color::Muted))
            .child(
                Button::new("browser-restart-crashed", i18n::t!("Reload"))
                    .on_click(cx.listener(|this, _, _, cx| this.restart_crashed_surface(cx))),
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
                    Button::new("browser-install-runtime", i18n::t!("Open Download Page"))
                        .on_click(|_, _, cx| cx.open_url(WEBVIEW2_DOWNLOAD_URL)),
                )
            })
            .child(
                Button::new(
                    "browser-open-external",
                    i18n::t!("Open in External Browser"),
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    this.open_in_external_browser(&OpenInExternalBrowser, window, cx)
                })),
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
            (None, true) => i18n::t!("Loading…").into(),
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
            .on_action(cx.listener(Self::open_developer_tools))
            .on_action(cx.listener(Self::stop_ai_control))
            .on_action(cx.listener(Self::resume_ai_control))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if let Some(webview) = &this.webview {
                        webview.release_keyboard_focus();
                    }
                    this.touch_ai_tab(cx);
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
                    .gap_2()
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(status)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                    )
                    .children(self.render_ai_control_state(cx)),
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

/// AI タブで止めたナビゲーションの知らせ。ツールの呼び出し側には別に、モデル向けの
/// 英文で理由を返している。
fn blocked_navigation_notice(url: &str) -> SharedString {
    i18n::t!("Blocked {url}: AI tabs only open http and https pages on the network allowlist.")
        .into()
}

/// ブックマークのツールチップ。「Ctrl+クリックで新しいタブ」は見た目からは
/// 分からないので、URL と一緒に添える。
fn bookmark_tooltip(url: &str) -> SharedString {
    i18n::t!("{url} — Ctrl-click or middle-click to open in a new tab").into()
}

/// タブに出す名前。ページタイトルが無ければ URL、それも無ければ既定名。
/// 長いページタイトルはタブを押し広げるので、ここで切り詰める。
pub(crate) fn tab_label(page_title: &str, url: &str) -> SharedString {
    let source = match (page_title.trim(), url.trim()) {
        ("", "") => return i18n::tr(UNTITLED_TAB_LABEL).into(),
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
        assert_eq!(tab_label("", ""), "Browser");
        assert_eq!(tab_label("   ", "  "), "Browser");
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
