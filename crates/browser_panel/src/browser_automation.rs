//! AI が内蔵ブラウザーを操作するための提供側 (ADR-F89)。
//!
//! `browser_control` の契約 ([`BrowserControl`]) をここで実装し、MCP と内蔵エージェントの
//! ブラウザーツールはすべてここを通る。AI が触れるのは **AI タブ** (別プロファイル・
//! egress プロキシ必須・ナビゲーションは許可リストで事前検査) だけで、利用者の普段の
//! タブは列挙すらしない。
//!
//! ページの中の仕事 (要素の参照・スナップショット・合成入力・コンソールとネットワークの
//! 記録) は `automation.js` が担い、ここからは `execute_script` で名前空間の関数を呼ぶ。
//! ページは同じ世界で動くので名前空間を書き換えられるが、それは許容する: 通信の上限は
//! JavaScript ではなくプロキシが守っている。ページから返った値を Rust 側の安全上の
//! 判断には使わない。

use std::{
    path::PathBuf,
    pin::pin,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow, bail};
use browser_control::{
    BrowserCommand, BrowserControl, BrowserOutput, ElementTarget, GlobalBrowserControl,
    KeyModifier, MAX_CHARS_LIMIT, MAX_SCREENSHOT_EDGE_PX, MouseButton, NavigateTarget,
    ScrollDirection, TabId, TabInfo,
};
use egress_policy::EgressAllowlistGlobal;
use futures::{
    channel::oneshot,
    future::{self, Either},
};
use gpui::{
    AnyWindowHandle, App, AppContext as _, AsyncApp, BackgroundExecutor, Entity, Task, WeakEntity,
    Window,
};
use http_client::{EgressPolicyHandle, Url};
use project::Project;
use serde::Deserialize;
use serde_json::{Value, json};
use settings::Settings as _;
use workspace::{MultiWorkspace, Pane, Workspace};

use crate::webview::{
    NativeInput, NativeInputUnsupported, NavigationDecision, WebView, WebViewOptions,
    WebViewProfile,
};
use crate::{BrowserItem, BrowserPanel, BrowserSettings, BrowserView, NewTabPlacement};

/// The page instrumentation, injected at document start into AI tabs only.
pub(crate) const AUTOMATION_SCRIPT: &str = include_str!("automation.js");

/// The global `automation.js` defines. Kept in one place so that the scripts built here and
/// the script itself cannot disagree.
const NAMESPACE: &str = "__forgeKairosAutomation";

/// How long a command waits for an AI tab's browsing surface to come up.
const SURFACE_WAIT: Duration = Duration::from_secs(20);
/// How often polling commands (`browser_wait_for`, a pending `browser_eval`, the surface wait)
/// look again.
const POLL_INTERVAL: Duration = Duration::from_millis(150);
/// Pause after synthesised input so that a navigation it starts is reported by the command that
/// started it.
const INPUT_SETTLE: Duration = Duration::from_millis(120);
/// `browser_eval` has no timeout argument; a Promise that takes longer than this is reported
/// as still pending.
const EVAL_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest a single script, screenshot or input may take in the backend.
const BACKEND_ANSWER_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_STACK_LINES: usize = 6;
/// One `browser_scroll` step, as the tool description promises.
const SCROLL_STEP_PX: f64 = 100.;

const SENSITIVE_FIELD_REFUSAL: &str = "Typing into password or payment fields is not allowed.";
/// A trusted click on a file input opens the native file dialog, which is modal to the editor
/// and would let the AI pick a local file.
const FILE_UPLOAD_REFUSAL: &str = "File uploads are not supported in AI tabs.";
const CROSS_ORIGIN_FRAME_REFUSAL: &str = "The focus is inside a frame from another site, which \
     may hold a password or payment field; typing into it is not allowed.";
const STOPPED_BY_USER: &str = "The user stopped AI control of this AI tab. Ask the user to resume it from the browser toolbar.";
const TAB_CLOSED: &str = "The AI tab was closed.";
const NO_AI_TAB: &str =
    "No AI tab is open in this workspace. browser_open_tab or browser_navigate opens one.";
const EVAL_UNSUPPORTED: &str = "browser_eval is not available on this platform: its browser \
     engine cannot keep page scripts from opening direct WebRTC connections around the network \
     allowlist.";
const NOT_INSTRUMENTED: &str = "The page did not run the browser automation script; it may still \
     be loading or navigating. Try again in a moment.";

static NEXT_USE_STAMP: AtomicU64 = AtomicU64::new(1);

/// A stamp for "most recently used": the current AI tab of a workspace is the one with the
/// highest stamp.
pub(crate) fn next_use_stamp() -> u64 {
    NEXT_USE_STAMP.fetch_add(1, Ordering::Relaxed)
}

pub(crate) fn init(cx: &mut App) {
    cx.set_global(GlobalBrowserControl(Arc::new(BrowserControlProvider)));
}

// ---------------------------------------------------------------------------
// プロファイルと通信制限
// ---------------------------------------------------------------------------

/// Where a profile keeps its cookies, storage and cache. The user's profile stays where the
/// Windows build has always kept it, so existing sign-ins survive.
pub(crate) fn profile_data_folder(profile: WebViewProfile) -> PathBuf {
    let data_dir = paths::data_dir();
    if cfg!(windows) {
        match profile {
            WebViewProfile::User => data_dir.join("webview2"),
            WebViewProfile::Ai => data_dir.join("webview2-ai"),
        }
    } else {
        data_dir.join("webview").join(match profile {
            WebViewProfile::User => "user",
            WebViewProfile::Ai => "ai",
        })
    }
}

/// The live egress allowlist. Without one (a process that never installed a policy) only
/// loopback is allowed, so a missing global fails closed.
pub(crate) fn egress_policy(cx: &App) -> EgressPolicyHandle {
    cx.try_global::<EgressAllowlistGlobal>()
        .map(|global| global.0.clone())
        .unwrap_or_default()
}

/// Whether an AI tab may load a URL as its top-level document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AiNavigationVerdict {
    Allowed,
    /// Not http, https, data: or about:blank. `file:` in particular would let the AI read local
    /// files around `private_files`.
    SchemeNotAllowed,
    HostNotAllowed {
        host: String,
    },
}

impl AiNavigationVerdict {
    /// The reason given to the model, or `None` when the URL is allowed.
    pub(crate) fn denial_reason(&self) -> Option<String> {
        match self {
            Self::Allowed => None,
            Self::SchemeNotAllowed => {
                Some("AI tabs can only open http, https, data: and about:blank URLs".to_string())
            }
            Self::HostNotAllowed { host } => Some(format!(
                "`{host}` is not on the network allowlist; the user can add it to the network \
                 allowlist"
            )),
        }
    }
}

/// Reads `policy` at the moment of the decision, so an allowlist edit applies to the very next
/// navigation.
pub(crate) fn ai_navigation_verdict(url: &str, policy: &EgressPolicyHandle) -> AiNavigationVerdict {
    verdict_for_engine(url, policy, crate::webview::SUPPORTS_SCRIPT_EVAL)
}

/// A `data:` document is a page whose scripts the AI wrote, so it is `browser_eval` by another
/// name and only allowed where `browser_eval` is.
fn verdict_for_engine(
    url: &str,
    policy: &EgressPolicyHandle,
    engine_runs_ai_scripts: bool,
) -> AiNavigationVerdict {
    let Ok(parsed) = Url::parse(url.trim()) else {
        return AiNavigationVerdict::SchemeNotAllowed;
    };
    match parsed.scheme() {
        "http" | "https" => match policy.check_url(&parsed) {
            Ok(()) => AiNavigationVerdict::Allowed,
            Err(denied) => AiNavigationVerdict::HostNotAllowed { host: denied.host },
        },
        "data" if engine_runs_ai_scripts => AiNavigationVerdict::Allowed,
        "about" if parsed.path() == "blank" => AiNavigationVerdict::Allowed,
        _ => AiNavigationVerdict::SchemeNotAllowed,
    }
}

pub(crate) fn ai_navigation_filter(
    policy: EgressPolicyHandle,
) -> Rc<dyn Fn(&str) -> NavigationDecision> {
    Rc::new(
        move |url| match ai_navigation_verdict(url, &policy).denial_reason() {
            None => NavigationDecision::Allow,
            Some(reason) => NavigationDecision::Deny(reason),
        },
    )
}

/// The loopback proxy endpoint as the backends take it (`http://127.0.0.1:<port>`).
pub(crate) fn proxy_endpoint(endpoint: &Url) -> String {
    endpoint.as_str().trim_end_matches('/').to_string()
}

/// How an AI tab's surface is created. Without the egress proxy there is no AI tab at all
/// (`None`): connecting directly would take the AI's browsing out of the allowlist.
pub(crate) fn ai_web_view_options(
    proxy: Option<&Url>,
    policy: EgressPolicyHandle,
) -> Option<WebViewOptions> {
    let proxy = proxy?;
    Some(WebViewOptions {
        profile: WebViewProfile::Ai,
        user_data_folder: profile_data_folder(WebViewProfile::Ai),
        proxy_endpoint: Some(proxy_endpoint(proxy)),
        document_start_script: Some(AUTOMATION_SCRIPT.to_string()),
        navigation_filter: Some(ai_navigation_filter(policy)),
        block_downloads: true,
    })
}

// ---------------------------------------------------------------------------
// ページへの入口
// ---------------------------------------------------------------------------

/// What the automation needs from a browsing surface. `WebView` is the real one; tests drive
/// the commands through a fake.
pub(crate) trait AutomationSurface {
    fn navigate(&self, url: &str);
    fn go_back(&self);
    fn go_forward(&self);
    fn reload(&self);
    fn execute_script(&self, script: &str) -> oneshot::Receiver<Result<String>>;
    fn capture_png(&self) -> oneshot::Receiver<Result<Vec<u8>>>;
    fn dispatch_input(&self, input: NativeInput) -> oneshot::Receiver<Result<()>>;
}

impl AutomationSurface for WebView {
    fn navigate(&self, url: &str) {
        WebView::navigate(self, url)
    }

    fn go_back(&self) {
        WebView::go_back(self)
    }

    fn go_forward(&self) {
        WebView::go_forward(self)
    }

    fn reload(&self) {
        WebView::reload(self)
    }

    fn execute_script(&self, script: &str) -> oneshot::Receiver<Result<String>> {
        WebView::execute_script(self, script)
    }

    fn capture_png(&self) -> oneshot::Receiver<Result<Vec<u8>>> {
        WebView::capture_png(self)
    }

    fn dispatch_input(&self, input: NativeInput) -> oneshot::Receiver<Result<()>> {
        WebView::dispatch_input(self, input)
    }
}

/// Puts a deadline on every answer a backend owes. An engine can sit on a script, a capture or
/// an input forever (WKWebView does not run scripts of a page that is not on screen), and a
/// command must end with an error rather than hang the agent that is waiting for it.
struct DeadlineSurface {
    inner: Rc<dyn AutomationSurface>,
    executor: BackgroundExecutor,
}

impl DeadlineSurface {
    fn answer_within<T: Send + 'static>(
        &self,
        receiver: oneshot::Receiver<Result<T>>,
    ) -> oneshot::Receiver<Result<T>> {
        let (sender, answer) = oneshot::channel();
        let deadline = self.executor.timer(BACKEND_ANSWER_TIMEOUT);
        self.executor
            .spawn(async move {
                let outcome = match future::select(receiver, deadline).await {
                    Either::Left((Ok(outcome), _)) => outcome,
                    // The backend dropped its sender: dropping ours tells the caller the same.
                    Either::Left((Err(oneshot::Canceled), _)) => return,
                    Either::Right(_) => Err(anyhow!(
                        "The AI tab's browser did not answer within {} seconds; the page may be \
                         hidden or busy.",
                        BACKEND_ANSWER_TIMEOUT.as_secs()
                    )),
                };
                // The command may have been stopped in the meantime; then nobody is waiting.
                sender.send(outcome).ok();
            })
            .detach();
        answer
    }
}

impl AutomationSurface for DeadlineSurface {
    fn navigate(&self, url: &str) {
        self.inner.navigate(url)
    }

    fn go_back(&self) {
        self.inner.go_back()
    }

    fn go_forward(&self) {
        self.inner.go_forward()
    }

    fn reload(&self) {
        self.inner.reload()
    }

    fn execute_script(&self, script: &str) -> oneshot::Receiver<Result<String>> {
        self.answer_within(self.inner.execute_script(script))
    }

    fn capture_png(&self) -> oneshot::Receiver<Result<Vec<u8>>> {
        self.answer_within(self.inner.capture_png())
    }

    fn dispatch_input(&self, input: NativeInput) -> oneshot::Receiver<Result<()>> {
        self.answer_within(self.inner.dispatch_input(input))
    }
}

/// How a navigation an AI command waits for ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NavigationOutcome {
    Finished { succeeded: bool },
    Blocked { message: String },
}

/// The AI-only state of an AI tab's view.
#[derive(Default)]
pub(crate) struct AiTabState {
    pub(crate) id: Option<TabId>,
    /// Set by "Stop AI"; every command is refused until the user resumes.
    pub(crate) paused: bool,
    pub(crate) last_used: u64,
    next_command: u64,
    /// Commands in flight and the senders that cancel them.
    running: Vec<(u64, oneshot::Sender<()>)>,
    /// Navigations and downloads blocked so far, as messages for the model. Commands compare the
    /// length before and after to report the ones they caused.
    pub(crate) blocked: Vec<String>,
    navigation_waiters: Vec<oneshot::Sender<NavigationOutcome>>,
    /// The URL a `browser_navigate` asked for, while it waits.
    pub(crate) requested_navigation: Option<String>,
    /// A blocked navigation to somewhere else while a `browser_navigate` waits: a redirect of
    /// the requested page, or (on WebKitGTK) one of its frames. Named in the error if the load
    /// then fails, since a blocked redirect is why it failed.
    unmatched_block: Option<String>,
}

/// A running command's claim on its tab.
pub(crate) struct CommandTicket {
    pub(crate) command_id: u64,
    pub(crate) cancelled: oneshot::Receiver<()>,
    pub(crate) blocked_before: usize,
}

impl AiTabState {
    pub(crate) fn new() -> Self {
        Self {
            last_used: next_use_stamp(),
            ..Self::default()
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        !self.running.is_empty()
    }

    pub(crate) fn begin_command(&mut self) -> Result<CommandTicket> {
        if self.paused {
            bail!(STOPPED_BY_USER);
        }
        self.last_used = next_use_stamp();
        self.next_command += 1;
        let (sender, cancelled) = oneshot::channel();
        self.running.push((self.next_command, sender));
        Ok(CommandTicket {
            command_id: self.next_command,
            cancelled,
            blocked_before: self.blocked.len(),
        })
    }

    pub(crate) fn end_command(&mut self, command_id: u64) {
        self.running.retain(|(id, _)| *id != command_id);
    }

    /// "Stop AI": cancels what is running and refuses everything after it.
    pub(crate) fn stop(&mut self) {
        self.paused = true;
        for (_, sender) in self.running.drain(..) {
            // The command may have finished in the meantime; then nothing is waiting.
            sender.send(()).ok();
        }
    }

    pub(crate) fn expect_navigation(&mut self) -> oneshot::Receiver<NavigationOutcome> {
        self.unmatched_block = None;
        let (sender, receiver) = oneshot::channel();
        self.navigation_waiters.push(sender);
        receiver
    }

    /// A blocked navigation fails the waiting `browser_navigate` only when it is the one that
    /// was asked for (or when no URL was asked for: back, forward, reload). WebKitGTK cannot
    /// tell a blocked iframe from the main frame, so anything else is only a notice.
    pub(crate) fn navigation_blocked(&mut self, url: &str, reason: &str) {
        let message = blocked_navigation_message(url, reason);
        let concerns_the_request = self
            .requested_navigation
            .as_deref()
            .is_none_or(|requested| same_url(requested, url));
        if concerns_the_request {
            self.record_blocked(message.clone());
            self.navigation_ended(NavigationOutcome::Blocked { message });
        } else {
            log::info!("browser automation: {message} (not the page that was asked for)");
            self.unmatched_block = Some(message);
        }
    }

    pub(crate) fn navigation_ended(&mut self, outcome: NavigationOutcome) {
        self.requested_navigation = None;
        let outcome = match (outcome, self.unmatched_block.take()) {
            (NavigationOutcome::Finished { succeeded: false }, Some(message)) => {
                NavigationOutcome::Blocked { message }
            }
            (outcome, _) => outcome,
        };
        for waiter in self.navigation_waiters.drain(..) {
            // A waiter whose command already gave up has dropped its receiver.
            waiter.send(outcome.clone()).ok();
        }
    }

    pub(crate) fn record_blocked(&mut self, message: String) {
        self.blocked.push(message);
    }

    pub(crate) fn blocked_since(&self, start: usize) -> &[String] {
        self.blocked.get(start..).unwrap_or_default()
    }
}

fn same_url(left: &str, right: &str) -> bool {
    match (Url::parse(left.trim()), Url::parse(right.trim())) {
        (Ok(left), Ok(right)) => left == right,
        _ => left.trim().trim_end_matches('/') == right.trim().trim_end_matches('/'),
    }
}

pub(crate) fn blocked_navigation_message(url: &str, reason: &str) -> String {
    format!("The navigation to {url} was blocked: {reason}.")
}

pub(crate) fn blocked_download_message(url: &str) -> String {
    format!("A download of {url} was blocked: downloads are not allowed in AI tabs.")
}

// ---------------------------------------------------------------------------
// 提供側
// ---------------------------------------------------------------------------

struct BrowserControlProvider;

impl BrowserControl for BrowserControlProvider {
    fn is_enabled(&self, cx: &App) -> bool {
        BrowserSettings::get_global(cx).ai_control
    }

    fn run(
        &self,
        project: &Entity<Project>,
        command: BrowserCommand,
        cx: &mut App,
    ) -> Task<Result<BrowserOutput>> {
        if !self.is_enabled(cx) {
            return Task::ready(Err(anyhow!(browser_control::AI_CONTROL_OFF_MESSAGE)));
        }
        let Some((window, workspace)) = workspace_for_project(project, cx) else {
            return Task::ready(Err(anyhow!(
                "No editor window shows this project, so there is no built-in browser to use."
            )));
        };
        let target = WorkspaceTarget {
            window,
            workspace: workspace.downgrade(),
        };
        cx.spawn(async move |cx| run_command(&target, command, cx).await)
    }
}

fn workspace_for_project(
    project: &Entity<Project>,
    cx: &App,
) -> Option<(AnyWindowHandle, Entity<Workspace>)> {
    cx.windows().into_iter().find_map(|window| {
        if let Some(multi_workspace) = window.downcast::<MultiWorkspace>() {
            let multi_workspace = multi_workspace.read(cx).ok()?;
            multi_workspace
                .workspaces()
                .find(|workspace| workspace.read(cx).project() == project)
                .cloned()
                .map(|workspace| (window, workspace))
        } else {
            let workspace = window.downcast::<Workspace>()?.entity(cx).ok()?;
            (workspace.read(cx).project() == project).then_some((window, workspace))
        }
    })
}

struct WorkspaceTarget {
    window: AnyWindowHandle,
    workspace: WeakEntity<Workspace>,
}

impl WorkspaceTarget {
    fn update<R>(
        &self,
        cx: &mut AsyncApp,
        update: impl FnOnce(&mut Workspace, &mut Window, &mut gpui::Context<Workspace>) -> R,
    ) -> Result<R> {
        let workspace = self
            .workspace
            .upgrade()
            .context("The editor window of this project was closed.")?;
        cx.update_window(self.window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| update(workspace, window, cx))
        })
        .context("The editor window of this project was closed.")
    }
}

/// One AI tab as found in a workspace.
#[derive(Clone)]
pub(crate) struct AiTab {
    pub(crate) id: TabId,
    pub(crate) last_used: u64,
    pub(crate) pane: Entity<Pane>,
    pub(crate) item: Entity<BrowserItem>,
    pub(crate) view: Entity<BrowserView>,
}

impl AiTab {
    fn downgrade(&self) -> WeakAiTab {
        WeakAiTab {
            pane: self.pane.downgrade(),
            item: self.item.downgrade(),
            view: self.view.downgrade(),
        }
    }
}

/// What a running command holds on to. Weak, so that closing the tab drops its view (and its
/// browser with it), which cancels the command instead of keeping a removed page alive.
struct WeakAiTab {
    pane: WeakEntity<Pane>,
    item: WeakEntity<BrowserItem>,
    view: WeakEntity<BrowserView>,
}

impl WeakAiTab {
    fn reveal(&self, workspace: &mut Workspace, window: &mut Window, cx: &mut App) -> Result<()> {
        let closed = || anyhow!(TAB_CLOSED);
        let tab = AiTab {
            id: TabId(String::new()),
            last_used: 0,
            pane: self.pane.upgrade().ok_or_else(closed)?,
            item: self.item.upgrade().ok_or_else(closed)?,
            view: self.view.upgrade().ok_or_else(closed)?,
        };
        reveal_tab(&tab, workspace, window, cx);
        Ok(())
    }
}

/// Every AI tab of the workspace, in pane order: the center, then the docks' panes (the browser
/// panel, the terminal panel, ...). Ids are handed out here, the first time a tab is seen.
pub(crate) fn ai_tabs(workspace: &Workspace, cx: &mut App) -> Vec<AiTab> {
    let mut found = Vec::new();
    for pane in workspace.all_panes(cx) {
        let items = pane
            .read(cx)
            .items_of_type::<BrowserItem>()
            .collect::<Vec<_>>();
        for item in items {
            let view = item.read(cx).view().clone();
            if view.read(cx).is_ai_tab() {
                found.push((pane.clone(), item, view));
            }
        }
    }
    let existing = found
        .iter()
        .map(|(_, _, view)| view.read(cx).ai_tab_id().cloned())
        .collect::<Vec<_>>();
    let ids = assign_tab_ids(&existing);
    found
        .into_iter()
        .zip(ids)
        .map(|((pane, item, view), id)| {
            let last_used = view.update(cx, |view, _| {
                view.set_ai_tab_id(id.clone());
                view.ai_last_used()
            });
            AiTab {
                id,
                last_used,
                pane,
                item,
                view,
            }
        })
        .collect()
}

/// Keeps every id that is still unique and numbers the rest after the highest one. A tab that
/// was moved in from another window can bring an id that is already taken here.
pub(crate) fn assign_tab_ids(existing: &[Option<TabId>]) -> Vec<TabId> {
    let mut highest = existing
        .iter()
        .flatten()
        .filter_map(|id| id.0.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    let mut taken = std::collections::HashSet::new();
    existing
        .iter()
        .map(|id| match id {
            Some(id) if taken.insert(id.clone()) => id.clone(),
            _ => {
                highest += 1;
                let id = TabId(highest.to_string());
                taken.insert(id.clone());
                id
            }
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TabResolutionError {
    NoTabs,
    Unknown(TabId),
}

impl TabResolutionError {
    fn message(&self) -> String {
        match self {
            Self::NoTabs => NO_AI_TAB.to_string(),
            Self::Unknown(id) => format!(
                "There is no AI tab with the id `{id}` in this workspace. browser_tabs lists them."
            ),
        }
    }
}

/// `requested: None` picks the most recently used tab.
pub(crate) fn resolve_tab(
    tabs: &[(TabId, u64)],
    requested: Option<&TabId>,
) -> Result<usize, TabResolutionError> {
    match requested {
        Some(requested) => tabs
            .iter()
            .position(|(id, _)| id == requested)
            .ok_or_else(|| TabResolutionError::Unknown(requested.clone())),
        None => tabs
            .iter()
            .enumerate()
            .max_by_key(|(_, (_, last_used))| *last_used)
            .map(|(index, _)| index)
            .ok_or(TabResolutionError::NoTabs),
    }
}

fn tab_infos(tabs: &[AiTab], cx: &App) -> Vec<TabInfo> {
    let current = resolve_tab(&tab_keys(tabs), None).ok();
    tabs.iter()
        .enumerate()
        .map(|(index, tab)| {
            let view = tab.view.read(cx);
            TabInfo {
                id: tab.id.clone(),
                title: view.page_title().to_string(),
                url: view.current_url().to_string(),
                active: Some(index) == current,
                loading: view.is_loading(),
            }
        })
        .collect()
}

fn tab_keys(tabs: &[AiTab]) -> Vec<(TabId, u64)> {
    tabs.iter()
        .map(|tab| (tab.id.clone(), tab.last_used))
        .collect()
}

/// Shows the pane a tab lives in without moving keyboard focus: the tab becomes its pane's
/// active item and the dock holding the pane (if any) opens. The browser panel is not told that
/// the user asked for it, so its own waiting home tab stays unloaded (ADR-F48).
fn reveal_tab(tab: &AiTab, workspace: &mut Workspace, window: &mut Window, cx: &mut App) {
    tab.pane.update(cx, |pane, cx| {
        if let Some(index) = pane.index_for_item(&tab.item) {
            pane.activate_item(index, false, false, window, cx);
        }
    });
    for dock in workspace.all_docks() {
        let index = dock
            .read(cx)
            .panels()
            .position(|panel| panel.panes(cx).contains(&tab.pane));
        if let Some(index) = index {
            dock.update(cx, |dock, cx| {
                dock.restore_active_panel(index, window, cx);
                if !dock.is_open() {
                    dock.set_open(true, window, cx);
                }
            });
        }
    }
}

/// Opens an AI tab in the browser panel (or, without one, the active center pane) without
/// taking keyboard focus.
fn open_ai_tab(
    workspace: &mut Workspace,
    url: Option<String>,
    window: &mut Window,
    cx: &mut gpui::Context<Workspace>,
) -> Entity<BrowserItem> {
    let item = match workspace.panel::<BrowserPanel>(cx) {
        Some(panel) => panel.update(cx, |panel, cx| {
            panel.open_ai_tab(url, NewTabPlacement::ForegroundWithoutFocus, window, cx)
        }),
        None => {
            let workspace_handle = workspace.weak_handle();
            let fs = workspace.app_state().fs.clone();
            let item = cx.new(|cx| {
                BrowserItem::new_ai(
                    workspace_handle,
                    fs,
                    crate::BrowserHost::EditorTab,
                    url,
                    window,
                    cx,
                )
            });
            workspace.add_item_to_active_pane(Box::new(item.clone()), None, false, window, cx);
            item
        }
    };
    let tab = ai_tabs(workspace, cx)
        .into_iter()
        .find(|tab| tab.item == item);
    if let Some(tab) = tab {
        reveal_tab(&tab, workspace, window, cx);
    }
    item
}

// ---------------------------------------------------------------------------
// コマンドの実行
// ---------------------------------------------------------------------------

async fn run_command(
    target: &WorkspaceTarget,
    command: BrowserCommand,
    cx: &mut AsyncApp,
) -> Result<BrowserOutput> {
    match command {
        BrowserCommand::ListTabs => {
            let tabs = target.update(cx, |workspace, _, cx| {
                let tabs = ai_tabs(workspace, cx);
                tab_infos(&tabs, cx)
            })?;
            Ok(text_output(browser_control::format_tabs(&tabs)))
        }
        BrowserCommand::OpenTab { url } => {
            if let Some(url) = &url {
                refuse_denied_url(url, cx)?;
            }
            ensure_egress_proxy(http_client::child_process_proxy().as_ref())?;
            let id = target.update(cx, |workspace, window, cx| {
                let item = open_ai_tab(workspace, url.clone(), window, cx);
                ai_tabs(workspace, cx)
                    .into_iter()
                    .find(|tab| tab.item == item)
                    .map(|tab| tab.id)
            })?;
            let id = id.context("The AI tab could not be opened.")?;
            Ok(text_output(match url {
                Some(url) => format!(
                    "Opened AI tab {id} and started loading {url}; it is now the current AI tab."
                ),
                None => format!("Opened AI tab {id}; it is now the current AI tab."),
            }))
        }
        BrowserCommand::CloseTab { tab } => {
            let close = target.update(cx, |workspace, window, cx| {
                let tabs = ai_tabs(workspace, cx);
                let index = resolve_tab(&tab_keys(&tabs), Some(&tab))
                    .map_err(|error| anyhow!(error.message()))?;
                let tab = tabs.get(index).cloned().context("The AI tab went away.")?;
                if tab.view.read(cx).ai_is_paused() {
                    bail!(STOPPED_BY_USER);
                }
                anyhow::Ok(tab.pane.update(cx, |pane, cx| {
                    pane.close_item_by_id(
                        tab.item.entity_id(),
                        workspace::SaveIntent::Skip,
                        window,
                        cx,
                    )
                }))
            })??;
            close.await?;
            Ok(text_output(format!("Closed AI tab {tab}.")))
        }
        BrowserCommand::SelectTab { tab } => {
            target.update(cx, |workspace, window, cx| {
                let tabs = ai_tabs(workspace, cx);
                let index = resolve_tab(&tab_keys(&tabs), Some(&tab))
                    .map_err(|error| anyhow!(error.message()))?;
                let selected = tabs.get(index).context("The AI tab went away.")?;
                if selected.view.read(cx).ai_is_paused() {
                    bail!(STOPPED_BY_USER);
                }
                selected.view.update(cx, |view, cx| view.touch_ai_tab(cx));
                reveal_tab(selected, workspace, window, cx);
                anyhow::Ok(())
            })??;
            Ok(text_output(format!(
                "AI tab {tab} is now the current AI tab."
            )))
        }
        command => {
            let tab = resolve_command_tab(target, &command, cx)?;
            run_in_tab(target, tab, command, cx).await
        }
    }
}

fn resolve_command_tab(
    target: &WorkspaceTarget,
    command: &BrowserCommand,
    cx: &mut AsyncApp,
) -> Result<AiTab> {
    if let BrowserCommand::Navigate {
        target: NavigateTarget::Url(url),
        ..
    } = command
    {
        refuse_denied_url(url, cx)?;
    }
    target.update(cx, |workspace, window, cx| {
        let tabs = ai_tabs(workspace, cx);
        match resolve_tab(&tab_keys(&tabs), command.tab()) {
            Ok(index) => tabs.get(index).cloned().context("The AI tab went away."),
            Err(TabResolutionError::NoTabs)
                if matches!(command, BrowserCommand::Navigate { .. }) =>
            {
                ensure_egress_proxy(http_client::child_process_proxy().as_ref())?;
                let item = open_ai_tab(workspace, None, window, cx);
                ai_tabs(workspace, cx)
                    .into_iter()
                    .find(|tab| tab.item == item)
                    .context("The AI tab could not be opened.")
            }
            Err(error) => Err(anyhow!(error.message())),
        }
    })?
}

/// An AI tab without the egress proxy could never load a page (it never connects directly), so
/// the tools do not create one and say why instead.
pub(crate) fn ensure_egress_proxy(proxy: Option<&Url>) -> Result<()> {
    if proxy.is_none() {
        bail!(
            "The network allowlist proxy is not running, so no AI tab can be opened (AI tabs \
             never connect directly). It runs while proxying for child processes is on in the \
             Network Allowlist settings."
        );
    }
    Ok(())
}

fn refuse_denied_url(url: &str, cx: &mut AsyncApp) -> Result<()> {
    let policy = cx.update(|cx| egress_policy(cx));
    match ai_navigation_verdict(url, &policy).denial_reason() {
        None => Ok(()),
        Some(reason) => Err(anyhow!("Did not open {url}: {reason}.")),
    }
}

async fn run_in_tab(
    target: &WorkspaceTarget,
    tab: AiTab,
    command: BrowserCommand,
    cx: &mut AsyncApp,
) -> Result<BrowserOutput> {
    let tab = {
        let strong = tab;
        strong.downgrade()
    };
    let ticket = tab
        .view
        .update(cx, |view, cx| view.begin_ai_command(cx))
        .context(TAB_CLOSED)??;
    let blocked_before = ticket.blocked_before;
    let command_id = ticket.command_id;

    let outcome = {
        let work = pin!(execute_in_tab(target, &tab, command, cx.clone()));
        match future::select(work, ticket.cancelled).await {
            Either::Left((result, _)) => result,
            Either::Right((Ok(()), _)) => Err(anyhow!(STOPPED_BY_USER)),
            // The view (and the cancel sender it held) was dropped with its tab.
            Either::Right((Err(oneshot::Canceled), _)) => Err(anyhow!(TAB_CLOSED)),
        }
    };

    let blocked = tab
        .view
        .update(cx, |view, cx| {
            view.end_ai_command(command_id, cx);
            view.ai_blocked_since(blocked_before)
        })
        .unwrap_or_default();
    match outcome {
        Ok(output) if !blocked.is_empty() => Err(anyhow!(
            "{}\nAfterwards: {}",
            output.text.trim_end(),
            blocked.join(" ")
        )),
        outcome => outcome,
    }
}

async fn execute_in_tab(
    target: &WorkspaceTarget,
    tab: &WeakAiTab,
    command: BrowserCommand,
    mut cx: AsyncApp,
) -> Result<BrowserOutput> {
    let cx = &mut cx;
    let surface = ready_surface(target, tab, cx).await?;
    match command {
        BrowserCommand::Navigate {
            target: navigate_target,
            timeout_ms,
            ..
        } => navigate(tab, &surface, navigate_target, timeout_ms, cx).await,
        BrowserCommand::PageText { max_chars, .. } => {
            let value = call(&surface, "pageText", json!({ "maxChars": max_chars })).await?;
            Ok(text_output(format_page_text(&value, max_chars)))
        }
        BrowserCommand::Snapshot {
            interactive_only,
            max_chars,
            ..
        } => {
            let value = call(
                &surface,
                "snapshot",
                json!({ "interactiveOnly": interactive_only, "maxChars": max_chars }),
            )
            .await?;
            Ok(text_output(format_page_text(&value, max_chars)))
        }
        BrowserCommand::Find { query, limit, .. } => {
            let value = call(&surface, "find", json!({ "query": query, "limit": limit })).await?;
            Ok(text_output(format_find(&query, &value)))
        }
        BrowserCommand::Click {
            target: element,
            button,
            click_count,
            modifiers,
            ..
        } => {
            let point = target_point(&surface, &element).await?;
            if point.opens_file_chooser {
                bail!(FILE_UPLOAD_REFUSAL);
            }
            let native = NativeInput::MouseClick {
                x: point.x,
                y: point.y,
                button,
                click_count,
                modifiers: modifiers.clone(),
            };
            if !dispatch_native(&surface, native).await? {
                call(
                    &surface,
                    "click",
                    json!({
                        "x": point.x,
                        "y": point.y,
                        "button": mouse_button_name(button),
                        "clickCount": click_count,
                        "modifiers": modifier_names(&modifiers),
                    }),
                )
                .await?;
            }
            settle(cx).await;
            Ok(text_output(format!(
                "Clicked {} at ({}, {}).{}",
                point.description,
                round_coordinate(point.x),
                round_coordinate(point.y),
                point.covered_note()
            )))
        }
        BrowserCommand::Hover {
            target: element, ..
        } => {
            let point = target_point(&surface, &element).await?;
            let native = NativeInput::MouseMove {
                x: point.x,
                y: point.y,
            };
            if !dispatch_native(&surface, native).await? {
                call(&surface, "hover", json!({ "x": point.x, "y": point.y })).await?;
            }
            settle(cx).await;
            Ok(text_output(format!(
                "Moved the mouse over {} at ({}, {}).{}",
                point.description,
                round_coordinate(point.x),
                round_coordinate(point.y),
                point.covered_note()
            )))
        }
        BrowserCommand::Type {
            element,
            text,
            submit,
            ..
        } => {
            if let Some(element) = &element {
                call(
                    &surface,
                    "focus",
                    json!({ "ref": element, "caretAtEnd": true }),
                )
                .await?;
            }
            // The text goes to whatever took the focus, which can be a field inside the
            // element that was named.
            let field = field_descriptor(&surface, None).await?;
            refuse_sensitive_field(&field)?;
            if !dispatch_native(&surface, NativeInput::InsertText(text.clone())).await? {
                call(&surface, "typeText", json!({ "text": text })).await?;
            }
            if submit {
                press_keys(&surface, &[KeyPress::named("Enter")], 1).await?;
            }
            settle(cx).await;
            let characters = text.chars().count();
            Ok(text_output(format!(
                "Typed {characters} character{} into {}{}.",
                if characters == 1 { "" } else { "s" },
                field.description,
                if submit { " and pressed Enter" } else { "" }
            )))
        }
        BrowserCommand::PressKey { keys, repeat, .. } => {
            let presses = parse_key_sequence(&keys)?;
            press_keys(&surface, &presses, repeat).await?;
            settle(cx).await;
            Ok(text_output(if repeat > 1 {
                format!("Pressed {keys} {repeat} times.")
            } else {
                format!("Pressed {keys}.")
            }))
        }
        BrowserCommand::Scroll {
            target: element,
            direction,
            amount,
            ..
        } => {
            let (delta_x, delta_y) = scroll_delta(direction, amount);
            let description = match &element {
                None => {
                    call(
                        &surface,
                        "scroll",
                        json!({ "deltaX": delta_x, "deltaY": delta_y }),
                    )
                    .await?;
                    "the page".to_string()
                }
                Some(element) => {
                    let point = target_point(&surface, element).await?;
                    let native = NativeInput::Wheel {
                        x: point.x,
                        y: point.y,
                        delta_x,
                        delta_y,
                    };
                    if !dispatch_native(&surface, native).await? {
                        call(
                            &surface,
                            "scroll",
                            json!({ "x": point.x, "y": point.y, "deltaX": delta_x, "deltaY": delta_y }),
                        )
                        .await?;
                    }
                    point.description
                }
            };
            settle(cx).await;
            let position = call(&surface, "scrollPosition", json!({})).await?;
            Ok(text_output(format!(
                "Scrolled {description} {} by {} CSS pixels. {}",
                scroll_direction_name(direction),
                f64::from(amount) * SCROLL_STEP_PX,
                format_scroll_position(&position)
            )))
        }
        BrowserCommand::SetValue { element, value, .. } => {
            let field = field_descriptor(&surface, Some(&element)).await?;
            refuse_sensitive_field(&field)?;
            refuse_file_chooser(&field)?;
            let result = call(
                &surface,
                "setValue",
                json!({ "ref": element, "value": value }),
            )
            .await?;
            settle(cx).await;
            Ok(text_output(format!(
                "Set {} to {}.",
                field.description,
                result
                    .get("value")
                    .map(Value::to_string)
                    .unwrap_or_else(|| value.to_string())
            )))
        }
        BrowserCommand::Screenshot { .. } => screenshot(target, tab, &surface, cx).await,
        BrowserCommand::Console {
            only_errors,
            pattern,
            limit,
            clear,
            ..
        } => {
            let value = call(&surface, "console", json!({ "clear": clear })).await?;
            let log: ConsoleLog =
                serde_json::from_value(value).context("The page returned an unreadable log.")?;
            Ok(text_output(format_console(
                &log,
                only_errors,
                pattern.as_deref(),
                limit,
            )))
        }
        BrowserCommand::Network {
            url_pattern,
            limit,
            clear,
            request_id,
            ..
        } => match request_id {
            Some(request_id) => {
                let value = call(&surface, "networkEntry", json!({ "id": request_id })).await?;
                if value.is_null() {
                    bail!(
                        "There is no request with the id `{request_id}`; the list is lost when \
                         the page navigates."
                    );
                }
                let entry: NetworkEntry = serde_json::from_value(value)
                    .context("The page returned an unreadable request.")?;
                Ok(text_output(format_network_entry(&entry)))
            }
            None => {
                let value = call(&surface, "network", json!({ "clear": clear })).await?;
                let log: NetworkLog = serde_json::from_value(value)
                    .context("The page returned an unreadable request list.")?;
                Ok(text_output(format_network(
                    &log,
                    url_pattern.as_deref(),
                    limit,
                )))
            }
        },
        BrowserCommand::Eval { expression, .. } => {
            if !crate::webview::SUPPORTS_SCRIPT_EVAL {
                bail!(EVAL_UNSUPPORTED);
            }
            evaluate(&surface, &expression, cx).await
        }
        BrowserCommand::WaitFor {
            text,
            selector,
            timeout_ms,
            ..
        } => wait_for(&surface, text, selector, timeout_ms, cx).await,
        BrowserCommand::ListTabs
        | BrowserCommand::OpenTab { .. }
        | BrowserCommand::CloseTab { .. }
        | BrowserCommand::SelectTab { .. } => {
            bail!("this command is not about one AI tab")
        }
    }
}

/// Waits until the tab has a browsing surface. A tab that was never shown has none yet (the
/// backends create it when the tab is first painted), so it is brought to the front once.
async fn ready_surface(
    target: &WorkspaceTarget,
    tab: &WeakAiTab,
    cx: &mut AsyncApp,
) -> Result<Rc<dyn AutomationSurface>> {
    let deadline = cx.background_executor().now() + SURFACE_WAIT;
    let mut revealed = false;
    loop {
        let status = tab
            .view
            .update(cx, |view, _| view.automation_status())
            .context(TAB_CLOSED)?;
        match status {
            AutomationStatus::Ready(surface) => {
                return Ok(Rc::new(DeadlineSurface {
                    inner: surface,
                    executor: cx.background_executor().clone(),
                }));
            }
            AutomationStatus::Unavailable(message) => bail!("{message}"),
            AutomationStatus::Waiting { has_surface } => {
                if !has_surface && !revealed {
                    revealed = true;
                    target.update(cx, |workspace, window, cx| {
                        tab.reveal(workspace, window, cx)
                    })??;
                }
            }
        }
        if cx.background_executor().now() >= deadline {
            bail!(
                "The AI tab's browser did not start within {} seconds.",
                SURFACE_WAIT.as_secs()
            );
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

pub(crate) enum AutomationStatus {
    Ready(Rc<dyn AutomationSurface>),
    Waiting { has_surface: bool },
    Unavailable(String),
}

async fn settle(cx: &mut AsyncApp) {
    cx.background_executor().timer(INPUT_SETTLE).await;
}

fn text_output(text: String) -> BrowserOutput {
    BrowserOutput {
        text,
        image_png: None,
    }
}

async fn navigate(
    tab: &WeakAiTab,
    surface: &Rc<dyn AutomationSurface>,
    target: NavigateTarget,
    timeout_ms: u64,
    cx: &mut AsyncApp,
) -> Result<BrowserOutput> {
    let waiter = tab
        .view
        .update(cx, |view, cx| view.ai_navigate(surface, &target, cx))
        .context(TAB_CLOSED)??;
    let timeout = cx
        .background_executor()
        .timer(Duration::from_millis(timeout_ms));
    let outcome = match future::select(waiter, timeout).await {
        Either::Left((outcome, _)) => {
            Some(outcome.map_err(|_| anyhow!("The AI tab's browser went away while loading."))?)
        }
        Either::Right(_) => None,
    };
    let (url, title) = tab
        .view
        .read_with(cx, |view, _| {
            (
                view.current_url().to_string(),
                view.page_title().to_string(),
            )
        })
        .context(TAB_CLOSED)?;
    match outcome {
        Some(NavigationOutcome::Finished { succeeded: true }) => {
            Ok(text_output(if title.trim().is_empty() {
                format!("Loaded {url}.")
            } else {
                format!("Loaded {url} (title: {}).", title.trim())
            }))
        }
        Some(NavigationOutcome::Finished { succeeded: false }) => {
            bail!("Could not load {url}.")
        }
        Some(NavigationOutcome::Blocked { message }) => bail!("{message}"),
        None => Ok(text_output(format!(
            "{url} is still loading after {timeout_ms} ms; the page may be usable already."
        ))),
    }
}

// -- スクリプトの呼び出し -----------------------------------------------------

/// A call of one function of `automation.js`, as a script whose completion value is the
/// function's result envelope.
pub(crate) fn automation_call_script(function: &str, arguments: &Value) -> String {
    format!(
        "(function(){{try{{var a=window[{namespace}];\
         if(!a||typeof a.call!==\"function\")return {{notInjected:true}};\
         return a.call({function},{arguments});}}\
         catch(e){{return {{ok:false,error:String(e&&e.message||e),stack:String(e&&e.stack||\"\")}};}}}})()",
        namespace = json!(NAMESPACE),
        function = json!(function),
    )
}

/// `browser_eval`: the expression is placed in the script itself rather than passed to
/// `eval`, which a page's Content-Security-Policy may forbid.
pub(crate) fn eval_script(expression: &str) -> String {
    let expression = expression.trim().trim_end_matches(';').trim_end();
    format!(
        "(function(){{var a=window[{namespace}];\
         if(!a||typeof a.evalValue!==\"function\")return {{notInjected:true}};\
         var v;try{{v=(\n{expression}\n);}}catch(e){{return a.errorEnvelope(e);}}\
         return a.evalValue(v);}})()",
        namespace = json!(NAMESPACE),
    )
}

#[derive(Debug, PartialEq)]
pub(crate) enum ScriptEnvelope {
    NotInjected,
    Value(Value),
    Error { message: String, stack: String },
}

pub(crate) fn parse_envelope(raw: &str) -> Result<ScriptEnvelope> {
    let value: Value = serde_json::from_str(raw).map_err(|_| anyhow!(NOT_INSTRUMENTED))?;
    let Value::Object(object) = value else {
        // `null` is what the backends report for a script that did not run at all, for
        // instance one with a syntax error.
        bail!(
            "The page did not run the script: it may have a syntax error, or the page is \
             navigating."
        );
    };
    if object.get("notInjected").and_then(Value::as_bool) == Some(true) {
        return Ok(ScriptEnvelope::NotInjected);
    }
    match object.get("ok").and_then(Value::as_bool) {
        Some(true) => Ok(ScriptEnvelope::Value(
            object.get("value").cloned().unwrap_or(Value::Null),
        )),
        Some(false) => Ok(ScriptEnvelope::Error {
            message: object
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_string(),
            stack: object
                .get("stack")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        None => bail!("The page returned an unexpected answer to a browser automation call."),
    }
}

fn script_error(message: &str, stack: &str) -> anyhow::Error {
    let stack = stack
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        // A V8 stack starts with the message itself.
        .skip_while(|line| line.ends_with(message))
        .take(MAX_STACK_LINES)
        .collect::<Vec<_>>();
    if stack.is_empty() {
        anyhow!("{message}")
    } else {
        anyhow!("{message}\n{}", stack.join("\n"))
    }
}

async fn run_script(surface: &Rc<dyn AutomationSurface>, script: &str) -> Result<String> {
    surface
        .execute_script(script)
        .await
        .map_err(|_| anyhow!("The AI tab's browser went away before the script finished."))?
}

/// Runs a script built by [`automation_call_script`] or [`eval_script`]. When the page has no
/// instrumentation (it started loading before the AI tab existed, or a backend could not
/// inject it at document start), it is injected now and the script is run once more.
async fn run_envelope(surface: &Rc<dyn AutomationSurface>, script: &str) -> Result<Value> {
    for attempt in 0..2 {
        match parse_envelope(&run_script(surface, script).await?)? {
            ScriptEnvelope::NotInjected if attempt == 0 => {
                run_script(surface, &format!("{AUTOMATION_SCRIPT}\n;0")).await?;
            }
            ScriptEnvelope::NotInjected => break,
            ScriptEnvelope::Value(value) => return Ok(value),
            ScriptEnvelope::Error { message, stack } => return Err(script_error(&message, &stack)),
        }
    }
    bail!(NOT_INSTRUMENTED)
}

async fn call(
    surface: &Rc<dyn AutomationSurface>,
    function: &str,
    arguments: Value,
) -> Result<Value> {
    run_envelope(surface, &automation_call_script(function, &arguments)).await
}

/// `Ok(false)` when the backend has no trusted input and the caller should synthesise it in
/// the page instead.
async fn dispatch_native(surface: &Rc<dyn AutomationSurface>, input: NativeInput) -> Result<bool> {
    let result = surface
        .dispatch_input(input)
        .await
        .map_err(|_| anyhow!("The AI tab's browser went away before the input was delivered."))?;
    match result {
        Ok(()) => Ok(true),
        Err(error) if error.downcast_ref::<NativeInputUnsupported>().is_some() => Ok(false),
        Err(error) => Err(error),
    }
}

// -- 要素と入力 --------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TargetPoint {
    x: f64,
    y: f64,
    description: String,
    #[serde(default)]
    covered: Option<String>,
    /// The element that takes the click is a file input or a label of one.
    #[serde(default, rename = "opensFileChooser")]
    opens_file_chooser: bool,
}

impl TargetPoint {
    fn covered_note(&self) -> String {
        match &self.covered {
            Some(covering) => {
                format!(" Note: {covering} covers that point, so it received the input.")
            }
            None => String::new(),
        }
    }
}

async fn target_point(
    surface: &Rc<dyn AutomationSurface>,
    target: &ElementTarget,
) -> Result<TargetPoint> {
    let arguments = match target {
        ElementTarget::Ref(element) => json!({ "ref": element }),
        ElementTarget::Point { x, y } => json!({ "x": x, "y": y }),
    };
    let value = call(surface, "targetPoint", arguments).await?;
    serde_json::from_value(value).context("The page returned an unreadable element position.")
}

fn round_coordinate(value: f64) -> f64 {
    (value * 10.).round() / 10.
}

/// What `automation.js` reports about the element that would receive typed text.
#[derive(Debug, Default, Deserialize, PartialEq)]
pub(crate) struct FieldDescriptor {
    #[serde(default)]
    pub(crate) tag: String,
    #[serde(default, rename = "type")]
    pub(crate) input_type: String,
    #[serde(default)]
    pub(crate) autocomplete: String,
    #[serde(default)]
    pub(crate) description: String,
    /// The focus is inside a frame of another origin, whose fields the page cannot see.
    #[serde(default, rename = "crossOriginFrame")]
    pub(crate) cross_origin_frame: bool,
    /// A file input, or a label of one.
    #[serde(default, rename = "opensFileChooser")]
    pub(crate) opens_file_chooser: bool,
}

async fn field_descriptor(
    surface: &Rc<dyn AutomationSurface>,
    element: Option<&str>,
) -> Result<FieldDescriptor> {
    let value = call(surface, "field", json!({ "ref": element })).await?;
    serde_json::from_value(value).context("The page returned an unreadable field description.")
}

/// Password and payment fields are never typed into. This is a guard against mistakes, not a
/// security boundary: the descriptor comes from the page, which can lie about its fields.
pub(crate) fn is_sensitive_field(field: &FieldDescriptor) -> bool {
    let autocomplete = field.autocomplete.to_ascii_lowercase();
    (field.tag.eq_ignore_ascii_case("input") && field.input_type.eq_ignore_ascii_case("password"))
        || autocomplete
            .split_ascii_whitespace()
            .any(|token| token.contains("password") || token.starts_with("cc-"))
}

/// Like [`is_sensitive_field`], the descriptor comes from the page; the backends also keep
/// file dialogs from opening in AI tabs, which is what holds when the page lies.
pub(crate) fn refuse_file_chooser(field: &FieldDescriptor) -> Result<()> {
    let file_input =
        field.tag.eq_ignore_ascii_case("input") && field.input_type.eq_ignore_ascii_case("file");
    if file_input || field.opens_file_chooser {
        bail!(FILE_UPLOAD_REFUSAL);
    }
    Ok(())
}

pub(crate) fn refuse_sensitive_field(field: &FieldDescriptor) -> Result<()> {
    if is_sensitive_field(field) {
        bail!(SENSITIVE_FIELD_REFUSAL);
    }
    // Payment forms are usually exactly such frames (the card fields of a payment provider),
    // and what is focused inside one cannot be looked at.
    if field.cross_origin_frame {
        bail!(CROSS_ORIGIN_FRAME_REFUSAL);
    }
    Ok(())
}

fn mouse_button_name(button: MouseButton) -> &'static str {
    match button {
        MouseButton::Left => "left",
        MouseButton::Middle => "middle",
        MouseButton::Right => "right",
    }
}

fn modifier_names(modifiers: &[KeyModifier]) -> Vec<&'static str> {
    modifiers
        .iter()
        .map(|modifier| match modifier {
            KeyModifier::Shift => "shift",
            KeyModifier::Control => "control",
            KeyModifier::Alt => "alt",
            KeyModifier::Meta => "meta",
        })
        .collect()
}

pub(crate) fn scroll_delta(direction: ScrollDirection, amount: u32) -> (f64, f64) {
    let distance = f64::from(amount) * SCROLL_STEP_PX;
    match direction {
        ScrollDirection::Up => (0., -distance),
        ScrollDirection::Down => (0., distance),
        ScrollDirection::Left => (-distance, 0.),
        ScrollDirection::Right => (distance, 0.),
    }
}

fn scroll_direction_name(direction: ScrollDirection) -> &'static str {
    match direction {
        ScrollDirection::Up => "up",
        ScrollDirection::Down => "down",
        ScrollDirection::Left => "left",
        ScrollDirection::Right => "right",
    }
}

fn format_scroll_position(position: &Value) -> String {
    let number = |key: &str| {
        position
            .get(key)
            .and_then(Value::as_f64)
            .unwrap_or(0.)
            .round()
    };
    format!(
        "The page is scrolled to ({}, {}) of {}x{} CSS pixels.",
        number("scrollX"),
        number("scrollY"),
        number("scrollWidth"),
        number("scrollHeight")
    )
}

/// One key press of `browser_press_key`: a `KeyboardEvent.key` value, the matching
/// `KeyboardEvent.code` and the modifiers held.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct KeyPress {
    pub(crate) key: String,
    pub(crate) code: String,
    pub(crate) modifiers: Vec<KeyModifier>,
}

impl KeyPress {
    fn named(key: &str) -> Self {
        Self {
            key: key.to_string(),
            code: key_code(key),
            modifiers: Vec::new(),
        }
    }

    /// Whether the press would type a character into a text field.
    pub(crate) fn produces_text(&self) -> bool {
        self.key.chars().count() == 1
            && !self.modifiers.iter().any(|modifier| {
                matches!(
                    modifier,
                    KeyModifier::Control | KeyModifier::Alt | KeyModifier::Meta
                )
            })
    }

    /// Enter and Space activate a focused button, link or file input.
    pub(crate) fn activates_the_focused_element(&self) -> bool {
        matches!(self.key.as_str(), "Enter" | " ")
    }

    /// Cut, copy and paste shortcuts. Pressed as trusted input they would reach the user's
    /// clipboard (a paste hands its contents, perhaps a password, to the page and so to the
    /// model), so they are only ever synthesised in the page, where they cannot.
    pub(crate) fn uses_the_clipboard(&self) -> bool {
        let held = |modifier: KeyModifier| self.modifiers.contains(&modifier);
        let command = held(KeyModifier::Control) || held(KeyModifier::Meta);
        match self.key.as_str() {
            "Cut" | "Copy" | "Paste" => true,
            "Insert" => command || held(KeyModifier::Shift),
            "Delete" => held(KeyModifier::Shift),
            key => command && matches!(key.to_ascii_lowercase().as_str(), "c" | "v" | "x"),
        }
    }
}

pub(crate) fn parse_key_sequence(keys: &str) -> Result<Vec<KeyPress>> {
    let presses = keys
        .split_whitespace()
        .map(parse_key_press)
        .collect::<Result<Vec<_>>>()?;
    if presses.is_empty() {
        bail!("`keys` is empty; give at least one key, for example \"Enter\".");
    }
    Ok(presses)
}

fn parse_key_press(chord: &str) -> Result<KeyPress> {
    let (modifier_part, key_part) = if chord == "+" {
        ("", "+")
    } else if let Some(modifiers) = chord.strip_suffix("++") {
        (modifiers, "+")
    } else {
        match chord.rsplit_once('+') {
            Some((modifiers, key)) => (modifiers, key),
            None => ("", chord),
        }
    };
    let mut modifiers = Vec::new();
    for name in modifier_part.split('+').filter(|name| !name.is_empty()) {
        let modifier = match name.to_ascii_lowercase().as_str() {
            "shift" => KeyModifier::Shift,
            "ctrl" | "control" => KeyModifier::Control,
            "alt" | "option" => KeyModifier::Alt,
            "meta" | "cmd" | "command" | "super" | "win" => KeyModifier::Meta,
            _ => bail!("`{name}` in `{chord}` is not a modifier; use shift, ctrl, alt or meta."),
        };
        if !modifiers.contains(&modifier) {
            modifiers.push(modifier);
        }
    }
    if key_part.is_empty() {
        bail!("`{chord}` has no key after its modifiers.");
    }
    let mut key = normalize_key_name(key_part)
        .with_context(|| format!("`{key_part}` is not a key name; use KeyboardEvent.key names such as Enter, Tab, ArrowDown or a."))?;
    if modifiers.contains(&KeyModifier::Shift) && key.len() == 1 {
        key = key.to_uppercase();
    }
    Ok(KeyPress {
        code: key_code(&key),
        key,
        modifiers,
    })
}

fn normalize_key_name(name: &str) -> Option<String> {
    if name.chars().count() == 1 {
        return Some(name.to_string());
    }
    let lower = name.to_ascii_lowercase();
    let named = match lower.as_str() {
        "enter" | "return" => "Enter",
        "esc" | "escape" => "Escape",
        "tab" => "Tab",
        "space" | "spacebar" => " ",
        "backspace" => "Backspace",
        "delete" | "del" => "Delete",
        "insert" | "ins" => "Insert",
        "up" | "arrowup" => "ArrowUp",
        "down" | "arrowdown" => "ArrowDown",
        "left" | "arrowleft" => "ArrowLeft",
        "right" | "arrowright" => "ArrowRight",
        "home" => "Home",
        "end" => "End",
        "pageup" | "pgup" => "PageUp",
        "pagedown" | "pgdn" => "PageDown",
        "plus" => "+",
        "contextmenu" | "menu" => "ContextMenu",
        _ => {
            if let Some(number) = lower.strip_prefix('f')
                && let Ok(number) = number.parse::<u8>()
                && (1..=24).contains(&number)
            {
                return Some(format!("F{number}"));
            }
            // Any other KeyboardEvent.key name ("CapsLock", "MediaPlayPause", ...) as given.
            return name
                .chars()
                .next()
                .is_some_and(|first| first.is_ascii_uppercase())
                .then(|| name.to_string());
        }
    };
    Some(named.to_string())
}

/// The `KeyboardEvent.code` of the key on a US layout; empty when there is no obvious one.
pub(crate) fn key_code(key: &str) -> String {
    let mut characters = key.chars();
    if let (Some(character), None) = (characters.next(), characters.next()) {
        if character.is_ascii_alphabetic() {
            return format!("Key{}", character.to_ascii_uppercase());
        }
        if character.is_ascii_digit() {
            return format!("Digit{character}");
        }
        let code = match character {
            ' ' => "Space",
            '-' | '_' => "Minus",
            '=' | '+' => "Equal",
            '[' | '{' => "BracketLeft",
            ']' | '}' => "BracketRight",
            '\\' | '|' => "Backslash",
            ';' | ':' => "Semicolon",
            '\'' | '"' => "Quote",
            ',' | '<' => "Comma",
            '.' | '>' => "Period",
            '/' | '?' => "Slash",
            '`' | '~' => "Backquote",
            _ => "",
        };
        return code.to_string();
    }
    key.to_string()
}

async fn press_keys(
    surface: &Rc<dyn AutomationSurface>,
    presses: &[KeyPress],
    repeat: u32,
) -> Result<()> {
    let mut native = true;
    // Any other key ("Tab a b c") can move the focus, so the field is looked at again before
    // the first character after it.
    let mut field_checked = false;
    for _ in 0..repeat {
        for press in presses {
            if press.activates_the_focused_element() {
                refuse_file_chooser(&field_descriptor(surface, None).await?)?;
            }
            if press.produces_text() {
                if !field_checked {
                    refuse_sensitive_field(&field_descriptor(surface, None).await?)?;
                    field_checked = true;
                }
            } else {
                field_checked = false;
            }
            let delivered = if native && !press.uses_the_clipboard() {
                native = dispatch_native(
                    surface,
                    NativeInput::Key {
                        key: press.key.clone(),
                        code: press.code.clone(),
                        modifiers: press.modifiers.clone(),
                    },
                )
                .await?;
                native
            } else {
                false
            };
            if !delivered {
                call(
                    surface,
                    "pressKey",
                    json!({
                        "key": press.key,
                        "code": press.code,
                        "modifiers": modifier_names(&press.modifiers),
                    }),
                )
                .await?;
            }
        }
    }
    Ok(())
}

// -- スクリーンショット ------------------------------------------------------

/// The size a screenshot is scaled down to so that its longer edge is at most `max_edge`.
pub(crate) fn downscaled_size(width: u32, height: u32, max_edge: u32) -> (u32, u32) {
    let longer = width.max(height);
    if longer <= max_edge || longer == 0 {
        return (width, height);
    }
    let scale = f64::from(max_edge) / f64::from(longer);
    let scaled = |edge: u32| ((f64::from(edge) * scale).round() as u32).clamp(1, max_edge);
    (scaled(width), scaled(height))
}

pub(crate) fn downscale_png(png: &[u8], max_edge: u32) -> Result<(Vec<u8>, u32, u32)> {
    let image = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .context("The screenshot could not be decoded.")?;
    let (width, height) = downscaled_size(image.width(), image.height(), max_edge);
    if (width, height) == (image.width(), image.height()) {
        return Ok((png.to_vec(), width, height));
    }
    let resized = image.resize_exact(width, height, image::imageops::FilterType::Triangle);
    let mut encoded = Vec::new();
    resized
        .write_to(
            &mut std::io::Cursor::new(&mut encoded),
            image::ImageFormat::Png,
        )
        .context("The screenshot could not be encoded.")?;
    Ok((encoded, width, height))
}

async fn screenshot(
    target: &WorkspaceTarget,
    tab: &WeakAiTab,
    surface: &Rc<dyn AutomationSurface>,
    cx: &mut AsyncApp,
) -> Result<BrowserOutput> {
    // A hidden surface does not paint, so the tab is brought to the front of its pane first.
    target.update(cx, |workspace, window, cx| {
        tab.reveal(workspace, window, cx)
    })??;
    settle(cx).await;
    let viewport = call(surface, "viewport", json!({})).await?;
    let png = surface.capture_png().await.map_err(|_| {
        anyhow!("The AI tab's browser went away before the screenshot was taken.")
    })??;
    let (png, width, height) = cx
        .background_spawn(async move { downscale_png(&png, MAX_SCREENSHOT_EDGE_PX) })
        .await?;
    let url = tab
        .view
        .read_with(cx, |view, _| view.current_url().to_string())
        .context(TAB_CLOSED)?;
    let css = |key: &str| {
        viewport
            .get(key)
            .and_then(Value::as_f64)
            .unwrap_or(0.)
            .round()
    };
    Ok(BrowserOutput {
        text: format!(
            "{width}x{height} screenshot of {url}. The viewport is {}x{} CSS pixels.",
            css("width"),
            css("height")
        ),
        image_png: Some(png),
    })
}

// -- 評価と待機 --------------------------------------------------------------

async fn evaluate(
    surface: &Rc<dyn AutomationSurface>,
    expression: &str,
    cx: &mut AsyncApp,
) -> Result<BrowserOutput> {
    let mut value = run_envelope(surface, &eval_script(expression)).await?;
    let deadline = cx.background_executor().now() + EVAL_TIMEOUT;
    while let Some(id) = value.get("pending").cloned() {
        if cx.background_executor().now() >= deadline {
            bail!(
                "The returned Promise did not settle within {} seconds.",
                EVAL_TIMEOUT.as_secs()
            );
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
        value = call(surface, "evalResult", json!({ "id": id })).await?;
        if value.get("state").and_then(Value::as_str) == Some("pending") {
            value = json!({ "pending": id });
        }
    }
    Ok(text_output(format_eval_value(&value)?))
}

pub(crate) fn format_eval_value(value: &Value) -> Result<String> {
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        let stack = value
            .get("stack")
            .and_then(Value::as_str)
            .unwrap_or_default();
        return Err(script_error(
            &format!("Uncaught (in promise) {error}"),
            stack,
        ));
    }
    let mut text = match value.get("json").and_then(Value::as_str) {
        Some(json) => json.to_string(),
        None => "undefined".to_string(),
    };
    if value.get("truncated").and_then(Value::as_bool) == Some(true) {
        text.push_str("\n[The value was cut off.]");
    }
    Ok(truncate_chars(&text, MAX_CHARS_LIMIT))
}

async fn wait_for(
    surface: &Rc<dyn AutomationSurface>,
    text: Option<String>,
    selector: Option<String>,
    timeout_ms: u64,
    cx: &mut AsyncApp,
) -> Result<BrowserOutput> {
    let deadline = cx.background_executor().now() + Duration::from_millis(timeout_ms);
    let what = match (&text, &selector) {
        (Some(text), Some(selector)) => {
            format!("the text {text:?} and an element matching `{selector}`")
        }
        (Some(text), None) => format!("the text {text:?}"),
        (None, Some(selector)) => format!("an element matching `{selector}`"),
        (None, None) => bail!("Give `text`, `selector` or both."),
    };
    loop {
        match call(
            surface,
            "waitCheck",
            json!({ "text": text, "selector": selector }),
        )
        .await
        {
            Ok(value) => {
                if let Some(error) = value.get("invalidSelector").and_then(Value::as_str) {
                    bail!(
                        "`{}` is not a valid CSS selector: {error}",
                        selector.unwrap_or_default()
                    );
                }
                let holds = |key: &str, wanted: bool| {
                    !wanted || value.get(key).and_then(Value::as_bool) == Some(true)
                };
                if holds("text", text.is_some()) && holds("selector", selector.is_some()) {
                    return Ok(text_output(format!("Found {what}.")));
                }
            }
            // The page may be between documents; keep waiting until the deadline.
            Err(error) => log::debug!("browser_wait_for: {error:#}"),
        }
        if cx.background_executor().now() >= deadline {
            bail!("Timed out after {timeout_ms} ms waiting for {what}.");
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

// -- 出力の整形 --------------------------------------------------------------

pub(crate) fn truncate_chars(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        None => text.to_string(),
        Some((cut, _)) => format!(
            "{}\n[Cut off at {max_chars} of {} characters.]",
            &text[..cut],
            text.chars().count()
        ),
    }
}

fn format_page_text(value: &Value, max_chars: usize) -> String {
    let field = |key: &str| value.get(key).and_then(Value::as_str).unwrap_or_default();
    let title = field("title").trim();
    let header = if title.is_empty() {
        field("url").to_string()
    } else {
        format!("{title} - {}", field("url"))
    };
    let mut body = truncate_chars(field("text"), max_chars);
    if value.get("truncated").and_then(Value::as_bool) == Some(true) {
        body.push_str(&format!("\n[Cut off at {max_chars} characters.]"));
    }
    format!("{header}\n\n{body}")
}

fn format_find(query: &str, value: &Value) -> String {
    let matches = value
        .get("matches")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if matches.is_empty() {
        return format!("No element matches {query:?}.");
    }
    let total = value
        .get("total")
        .and_then(Value::as_u64)
        .unwrap_or(matches.len() as u64);
    let mut text = if total as usize > matches.len() {
        format!(
            "{} of {total} elements matching {query:?}:\n",
            matches.len()
        )
    } else {
        format!(
            "{} element{} matching {query:?}:\n",
            matches.len(),
            if matches.len() == 1 { "" } else { "s" }
        )
    };
    for entry in matches {
        let field = |key: &str| entry.get(key).and_then(Value::as_str).unwrap_or_default();
        text.push_str(&format!("[{}] {}\n", field("ref"), field("description")));
    }
    text
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ConsoleLog {
    #[serde(default)]
    pub(crate) entries: Vec<ConsoleEntry>,
    /// Entries pushed out of the ring buffer since the page loaded.
    #[serde(default)]
    pub(crate) dropped: u64,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct ConsoleEntry {
    #[serde(default)]
    pub(crate) level: String,
    #[serde(default)]
    pub(crate) text: String,
    /// Milliseconds since the page started loading.
    #[serde(default)]
    pub(crate) time: f64,
    #[serde(default)]
    pub(crate) source: Option<String>,
}

impl ConsoleEntry {
    fn is_error(&self) -> bool {
        matches!(self.level.as_str(), "error" | "exception" | "rejection")
    }
}

fn matches_pattern(haystack: &str, pattern: Option<&str>) -> bool {
    pattern.is_none_or(|pattern| haystack.to_lowercase().contains(&pattern.to_lowercase()))
}

/// Oldest first; `limit` keeps the newest.
pub(crate) fn format_console(
    log: &ConsoleLog,
    only_errors: bool,
    pattern: Option<&str>,
    limit: usize,
) -> String {
    let matching = log
        .entries
        .iter()
        .filter(|entry| !only_errors || entry.is_error())
        .filter(|entry| matches_pattern(&entry.text, pattern))
        .collect::<Vec<_>>();
    if matching.is_empty() {
        return "No console messages match.".to_string();
    }
    let shown = &matching[matching.len().saturating_sub(limit)..];
    let mut text = if shown.len() < matching.len() {
        format!(
            "The newest {} of {} matching console messages, oldest first:\n",
            shown.len(),
            matching.len()
        )
    } else {
        format!(
            "{} console message{}, oldest first:\n",
            shown.len(),
            if shown.len() == 1 { "" } else { "s" }
        )
    };
    for entry in shown {
        text.push_str(&format!(
            "[{}] +{} ms {}",
            entry.level,
            entry.time.round(),
            entry.text
        ));
        if let Some(source) = entry.source.as_deref().filter(|source| !source.is_empty()) {
            text.push_str(&format!(" ({source})"));
        }
        text.push('\n');
    }
    if log.dropped > 0 {
        text.push_str(&format!(
            "({} older messages were dropped from the buffer.)\n",
            log.dropped
        ));
    }
    text
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct NetworkLog {
    #[serde(default)]
    pub(crate) entries: Vec<NetworkEntry>,
    #[serde(default)]
    pub(crate) dropped: u64,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NetworkEntry {
    #[serde(default)]
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) method: String,
    #[serde(default)]
    pub(crate) url: String,
    #[serde(default)]
    pub(crate) status: Option<u32>,
    #[serde(default)]
    pub(crate) status_text: String,
    #[serde(default)]
    pub(crate) error: Option<String>,
    #[serde(default)]
    pub(crate) pending: bool,
    #[serde(default)]
    pub(crate) duration: Option<f64>,
    #[serde(default)]
    pub(crate) content_type: String,
    #[serde(default)]
    pub(crate) request_headers: Vec<(String, String)>,
    #[serde(default)]
    pub(crate) request_body: Option<String>,
    #[serde(default)]
    pub(crate) response_headers: Vec<(String, String)>,
    #[serde(default)]
    pub(crate) response_body: Option<String>,
    #[serde(default)]
    pub(crate) response_body_truncated: bool,
    #[serde(default)]
    pub(crate) response_body_note: Option<String>,
}

impl NetworkEntry {
    fn summary(&self) -> String {
        let outcome = match (&self.error, self.status, self.pending) {
            (Some(error), _, _) => format!("failed: {error}"),
            (None, _, true) => "pending".to_string(),
            (None, Some(status), false) => format!("{status} {}", self.status_text)
                .trim_end()
                .to_string(),
            (None, None, false) => "no status".to_string(),
        };
        let mut details = Vec::new();
        if let Some(kind) = self
            .content_type
            .split(';')
            .next()
            .map(str::trim)
            .filter(|kind| !kind.is_empty())
        {
            details.push(kind.to_string());
        }
        if let Some(duration) = self.duration {
            details.push(format!("{} ms", duration.round()));
        }
        details.push(self.kind.clone());
        format!(
            "{} {} {} -> {outcome} ({})",
            self.id,
            self.method,
            self.url,
            details.join(", ")
        )
    }
}

pub(crate) fn format_network(log: &NetworkLog, url_pattern: Option<&str>, limit: usize) -> String {
    let matching = log
        .entries
        .iter()
        .filter(|entry| matches_pattern(&entry.url, url_pattern))
        .collect::<Vec<_>>();
    if matching.is_empty() {
        return "No fetch or XMLHttpRequest requests match.".to_string();
    }
    let shown = &matching[matching.len().saturating_sub(limit)..];
    let mut text = if shown.len() < matching.len() {
        format!(
            "The newest {} of {} matching requests, oldest first:\n",
            shown.len(),
            matching.len()
        )
    } else {
        format!(
            "{} request{}, oldest first:\n",
            shown.len(),
            if shown.len() == 1 { "" } else { "s" }
        )
    };
    for entry in shown {
        text.push_str(&entry.summary());
        text.push('\n');
    }
    if log.dropped > 0 {
        text.push_str(&format!(
            "({} older requests were dropped from the list.)\n",
            log.dropped
        ));
    }
    text.push_str("Pass an id as request_id to see the headers and bodies of one request.\n");
    text
}

pub(crate) fn format_network_entry(entry: &NetworkEntry) -> String {
    let mut text = entry.summary();
    text.push('\n');
    let headers = |text: &mut String, title: &str, headers: &[(String, String)]| {
        if headers.is_empty() {
            return;
        }
        text.push_str(&format!("\n{title}:\n"));
        for (name, value) in headers {
            text.push_str(&format!("  {name}: {value}\n"));
        }
    };
    headers(&mut text, "Request headers", &entry.request_headers);
    if let Some(body) = entry
        .request_body
        .as_deref()
        .filter(|body| !body.is_empty())
    {
        text.push_str(&format!("\nRequest body:\n{body}\n"));
    }
    headers(&mut text, "Response headers", &entry.response_headers);
    match (&entry.response_body, &entry.response_body_note) {
        (Some(body), _) => {
            text.push_str(&format!("\nResponse body:\n{body}\n"));
            if entry.response_body_truncated {
                text.push_str("[The body was cut off.]\n");
            }
        }
        (None, Some(note)) => text.push_str(&format!("\nResponse body: {note}\n")),
        (None, None) => {}
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_client::EgressPolicy;

    fn policy(hosts: &[&str]) -> EgressPolicyHandle {
        let (policy, errors) = EgressPolicy::from_host_strings(hosts.iter().copied());
        assert!(errors.is_empty(), "{errors:?}");
        EgressPolicyHandle::new(policy)
    }

    #[test]
    fn only_web_and_blank_pages_may_load_in_ai_tabs() {
        let policy = policy(&["example.com"]);
        for allowed in [
            "https://example.com/",
            "http://example.com/path?q=1",
            "about:blank",
            "about:blank#top",
            "data:text/html,<p>hi</p>",
        ] {
            assert_eq!(
                ai_navigation_verdict(allowed, &policy),
                AiNavigationVerdict::Allowed,
                "{allowed}"
            );
        }
        for denied in [
            "file:///C:/Users/me/.ssh/id_rsa",
            "file:///etc/passwd",
            "view-source:https://example.com/",
            "edge://settings",
            "chrome://version",
            "about:config",
            "about:srcdoc",
            "javascript:alert(1)",
            "ftp://example.com/",
            "not a url",
        ] {
            assert_eq!(
                ai_navigation_verdict(denied, &policy),
                AiNavigationVerdict::SchemeNotAllowed,
                "{denied}"
            );
        }
    }

    #[test]
    fn data_documents_are_denied_where_page_scripts_are_not_contained() {
        let policy = policy(&[]);
        let page = "data:text/html,<script>new RTCPeerConnection()</script>";
        assert_eq!(
            verdict_for_engine(page, &policy, true),
            AiNavigationVerdict::Allowed
        );
        assert_eq!(
            verdict_for_engine(page, &policy, false),
            AiNavigationVerdict::SchemeNotAllowed
        );
        assert_eq!(
            verdict_for_engine("http://localhost/", &policy, false),
            AiNavigationVerdict::Allowed
        );
    }

    #[test]
    fn tricky_urls_are_judged_by_the_host_the_browser_would_use() {
        let policy = policy(&["allowed.test"]);
        for (url, verdict) in [
            (
                "http://allowed.test@evil.test/",
                AiNavigationVerdict::HostNotAllowed {
                    host: "evil.test".to_string(),
                },
            ),
            (
                "http://evil.test\\@allowed.test/",
                AiNavigationVerdict::HostNotAllowed {
                    host: "evil.test".to_string(),
                },
            ),
            ("HTTPS://ALLOWED.TEST./x", AiNavigationVerdict::Allowed),
            ("  https://allowed.test/  ", AiNavigationVerdict::Allowed),
            ("http://127.1:3000/", AiNavigationVerdict::Allowed),
            ("http://0x7f.1/", AiNavigationVerdict::Allowed),
            ("http://2130706433/", AiNavigationVerdict::Allowed),
            ("http://[::ffff:127.0.0.1]/", AiNavigationVerdict::Allowed),
            ("http://localhost./", AiNavigationVerdict::Allowed),
            (
                "http://app.localhost/",
                AiNavigationVerdict::HostNotAllowed {
                    host: "app.localhost".to_string(),
                },
            ),
            (
                "http://0/",
                AiNavigationVerdict::HostNotAllowed {
                    host: "0.0.0.0".to_string(),
                },
            ),
            (
                "https://bücher.test/",
                AiNavigationVerdict::HostNotAllowed {
                    host: "xn--bcher-kva.test".to_string(),
                },
            ),
            (
                "blob:http://localhost/1234",
                AiNavigationVerdict::SchemeNotAllowed,
            ),
            (
                "filesystem:http://localhost/temporary/a",
                AiNavigationVerdict::SchemeNotAllowed,
            ),
            ("JavaScript:alert(1)", AiNavigationVerdict::SchemeNotAllowed),
            ("about:blank?x#y", AiNavigationVerdict::Allowed),
            ("about:BLANK", AiNavigationVerdict::SchemeNotAllowed),
        ] {
            assert_eq!(ai_navigation_verdict(url, &policy), verdict, "{url}");
        }
    }

    #[test]
    fn hosts_off_the_allowlist_are_denied_but_loopback_is_not() {
        let policy = policy(&["example.com"]);
        assert_eq!(
            ai_navigation_verdict("https://evil.test/", &policy),
            AiNavigationVerdict::HostNotAllowed {
                host: "evil.test".to_string()
            }
        );
        for loopback in [
            "http://localhost:3000/",
            "http://127.0.0.1:8080/api",
            "http://[::1]:5173/",
        ] {
            assert_eq!(
                ai_navigation_verdict(loopback, &policy),
                AiNavigationVerdict::Allowed,
                "{loopback}"
            );
        }
        let reason = ai_navigation_verdict("https://evil.test/", &policy)
            .denial_reason()
            .expect("a denied host has a reason");
        assert!(reason.contains("evil.test"));
        assert!(reason.contains("network allowlist"));
    }

    #[test]
    fn the_filter_reads_the_allowlist_at_decision_time() {
        let handle = policy(&[]);
        let filter = ai_navigation_filter(handle.clone());
        assert!(matches!(
            filter("https://example.com/"),
            NavigationDecision::Deny(_)
        ));
        let (updated, _) = EgressPolicy::from_host_strings(["example.com"]);
        handle.set(updated);
        assert_eq!(filter("https://example.com/"), NavigationDecision::Allow);
        assert!(matches!(
            filter("file:///etc/passwd"),
            NavigationDecision::Deny(_)
        ));
    }

    #[test]
    fn without_an_allowlist_only_loopback_is_allowed() {
        let missing = EgressPolicyHandle::default();
        assert!(matches!(
            ai_navigation_verdict("https://example.com/", &missing),
            AiNavigationVerdict::HostNotAllowed { .. }
        ));
        assert_eq!(
            ai_navigation_verdict("http://localhost/", &missing),
            AiNavigationVerdict::Allowed
        );
    }

    #[test]
    fn an_ai_tab_without_the_egress_proxy_gets_no_surface() {
        assert!(ai_web_view_options(None, policy(&[])).is_none());

        let proxy = Url::parse("http://127.0.0.1:49152/").expect("valid url");
        let options = ai_web_view_options(Some(&proxy), policy(&[])).expect("proxy is running");
        assert_eq!(options.profile, WebViewProfile::Ai);
        assert_eq!(
            options.proxy_endpoint.as_deref(),
            Some("http://127.0.0.1:49152")
        );
        assert_eq!(
            options.user_data_folder,
            profile_data_folder(WebViewProfile::Ai)
        );
        assert!(options.block_downloads);
        assert_eq!(
            options.document_start_script.as_deref(),
            Some(AUTOMATION_SCRIPT)
        );
        let filter = options
            .navigation_filter
            .expect("AI tabs filter navigations");
        assert!(matches!(
            filter("file:///etc/passwd"),
            NavigationDecision::Deny(_)
        ));
        assert!(matches!(
            filter("https://example.com/"),
            NavigationDecision::Deny(_)
        ));
        assert_eq!(filter("http://localhost:3000/"), NavigationDecision::Allow);
    }

    #[test]
    fn profiles_keep_separate_data_folders() {
        let user = profile_data_folder(WebViewProfile::User);
        let ai = profile_data_folder(WebViewProfile::Ai);
        assert_ne!(user, ai);
        assert!(!ai.starts_with(&user), "{ai:?} is inside {user:?}");
        if cfg!(windows) {
            assert!(user.ends_with("webview2"));
        }
    }

    #[test]
    fn tab_ids_stay_unique() {
        let id = |value: &str| TabId(value.to_string());
        assert_eq!(assign_tab_ids(&[None, None]), vec![id("1"), id("2")]);
        assert_eq!(
            assign_tab_ids(&[Some(id("3")), None, Some(id("1"))]),
            vec![id("3"), id("4"), id("1")]
        );
        // Two tabs that bring the same id (one moved in from another window).
        assert_eq!(
            assign_tab_ids(&[Some(id("2")), Some(id("2"))]),
            vec![id("2"), id("3")]
        );
    }

    #[test]
    fn tabs_resolve_to_the_named_or_the_most_recently_used_one() {
        let id = |value: &str| TabId(value.to_string());
        let tabs = vec![(id("1"), 10), (id("2"), 30), (id("3"), 20)];
        assert_eq!(resolve_tab(&tabs, None), Ok(1));
        assert_eq!(resolve_tab(&tabs, Some(&id("3"))), Ok(2));
        assert_eq!(
            resolve_tab(&tabs, Some(&id("9"))),
            Err(TabResolutionError::Unknown(id("9")))
        );
        assert_eq!(resolve_tab(&[], None), Err(TabResolutionError::NoTabs));
    }

    #[test]
    fn password_and_payment_fields_are_refused() {
        let field = |tag: &str, input_type: &str, autocomplete: &str| FieldDescriptor {
            tag: tag.to_string(),
            input_type: input_type.to_string(),
            autocomplete: autocomplete.to_string(),
            description: String::new(),
            cross_origin_frame: false,
            opens_file_chooser: false,
        };
        for sensitive in [
            field("input", "password", ""),
            field("INPUT", "Password", ""),
            field("input", "text", "current-password"),
            field("input", "text", "section-login new-password"),
            field("input", "text", "cc-number"),
            field("input", "tel", "billing cc-csc"),
            field("div", "", "cc-exp"),
        ] {
            assert!(is_sensitive_field(&sensitive), "{sensitive:?}");
            let error = refuse_sensitive_field(&sensitive).expect_err("must be refused");
            assert_eq!(error.to_string(), SENSITIVE_FIELD_REFUSAL);
        }
        for ordinary in [
            field("input", "text", ""),
            field("input", "email", "username"),
            field("textarea", "", "off"),
            field("div", "", ""),
            field("input", "text", "accept"),
        ] {
            assert!(!is_sensitive_field(&ordinary), "{ordinary:?}");
            assert!(refuse_sensitive_field(&ordinary).is_ok());
        }
    }

    #[test]
    fn screenshots_are_scaled_to_the_longest_edge() {
        assert_eq!(downscaled_size(800, 600, 1568), (800, 600));
        assert_eq!(downscaled_size(1568, 900, 1568), (1568, 900));
        assert_eq!(downscaled_size(3136, 1764, 1568), (1568, 882));
        assert_eq!(downscaled_size(1000, 4000, 1568), (392, 1568));
        assert_eq!(downscaled_size(10_000, 1, 1568), (1568, 1));
        assert_eq!(downscaled_size(0, 0, 1568), (0, 0));
    }

    #[test]
    fn a_large_png_is_scaled_down_and_stays_a_png() {
        let image = image::RgbaImage::from_pixel(3200, 1000, image::Rgba([10, 20, 30, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("encoding a test image");
        let (scaled, width, height) = downscale_png(&png, 1568).expect("downscaling");
        assert_eq!((width, height), (1568, 490));
        let decoded = image::load_from_memory_with_format(&scaled, image::ImageFormat::Png)
            .expect("the result is a png");
        assert_eq!((decoded.width(), decoded.height()), (1568, 490));
    }

    #[test]
    fn key_sequences_parse_into_presses() {
        let presses = parse_key_sequence("ctrl+a Backspace shift+tab Enter").expect("parses");
        assert_eq!(
            presses,
            vec![
                KeyPress {
                    key: "a".to_string(),
                    code: "KeyA".to_string(),
                    modifiers: vec![KeyModifier::Control]
                },
                KeyPress {
                    key: "Backspace".to_string(),
                    code: "Backspace".to_string(),
                    modifiers: vec![]
                },
                KeyPress {
                    key: "Tab".to_string(),
                    code: "Tab".to_string(),
                    modifiers: vec![KeyModifier::Shift]
                },
                KeyPress {
                    key: "Enter".to_string(),
                    code: "Enter".to_string(),
                    modifiers: vec![]
                },
            ]
        );
        let plus = parse_key_sequence("ctrl++ + space f5").expect("parses");
        assert_eq!(plus[0].key, "+");
        assert_eq!(plus[0].modifiers, vec![KeyModifier::Control]);
        assert_eq!(plus[1].key, "+");
        assert_eq!(plus[2].key, " ");
        assert_eq!(plus[2].code, "Space");
        assert_eq!(plus[3].key, "F5");
        assert_eq!(parse_key_sequence("shift+a").expect("parses")[0].key, "A");
        assert!(parse_key_sequence("").is_err());
        assert!(parse_key_sequence("hyper+a").is_err());
        assert!(parse_key_sequence("ctrl+").is_err());
        assert!(parse_key_sequence("notakey").is_err());

        assert!(KeyPress::named("a").produces_text());
        assert!(!KeyPress::named("Enter").produces_text());
        assert!(!parse_key_sequence("ctrl+v").expect("parses")[0].produces_text());
    }

    #[test]
    fn call_scripts_target_the_namespace_and_escape_their_arguments() {
        let script = automation_call_script("find", &json!({ "query": "a\"b</script>" }));
        assert!(script.contains("window[\"__forgeKairosAutomation\"]"));
        assert!(script.contains("a.call(\"find\",{\"query\":\"a\\\"b</script>\"})"));
        assert!(AUTOMATION_SCRIPT.contains(NAMESPACE));
    }

    #[test]
    fn eval_scripts_wrap_the_expression_without_eval() {
        let script = eval_script("  document.title;  ");
        assert!(script.contains("v=(\ndocument.title\n);"));
        assert!(!script.contains("eval("));
    }

    #[test]
    fn envelopes_are_parsed() {
        assert_eq!(
            parse_envelope(r#"{"notInjected":true}"#).expect("parses"),
            ScriptEnvelope::NotInjected
        );
        assert_eq!(
            parse_envelope(r#"{"ok":true,"value":{"a":1}}"#).expect("parses"),
            ScriptEnvelope::Value(json!({ "a": 1 }))
        );
        assert_eq!(
            parse_envelope(r#"{"ok":false,"error":"boom","stack":"Error: boom\n at x"}"#)
                .expect("parses"),
            ScriptEnvelope::Error {
                message: "boom".to_string(),
                stack: "Error: boom\n at x".to_string()
            }
        );
        assert!(parse_envelope("null").is_err());
        assert!(parse_envelope("").is_err());
    }

    #[test]
    fn script_errors_keep_the_head_of_the_stack() {
        let stack = (0..20)
            .map(|line| format!("    at frame{line} (app.js:{line}:1)"))
            .collect::<Vec<_>>()
            .join("\n");
        let error = script_error(
            "TypeError: x is undefined",
            &format!("TypeError: x is undefined\n{stack}"),
        );
        let text = error.to_string();
        assert!(text.starts_with("TypeError: x is undefined\nat frame0"));
        assert_eq!(text.lines().count(), 1 + MAX_STACK_LINES);
    }

    #[test]
    fn console_logs_keep_the_newest_matching_entries() {
        let entry = |level: &str, text: &str, time: f64| ConsoleEntry {
            level: level.to_string(),
            text: text.to_string(),
            time,
            source: None,
        };
        let log = ConsoleLog {
            entries: vec![
                entry("log", "starting", 1.),
                entry("error", "Failed to fetch", 2.),
                entry("warn", "deprecated", 3.),
                entry("exception", "Uncaught TypeError", 4.),
                entry("log", "FETCH done", 5.),
            ],
            dropped: 0,
        };
        let errors = format_console(&log, true, None, 50);
        assert!(errors.contains("Failed to fetch"));
        assert!(errors.contains("Uncaught TypeError"));
        assert!(!errors.contains("starting"));

        let newest = format_console(&log, false, Some("fetch"), 1);
        assert!(newest.contains("FETCH done"));
        assert!(!newest.contains("Failed to fetch"));
        assert!(newest.starts_with("The newest 1 of 2"));

        let oldest_first = format_console(&log, false, None, 50);
        let starting = oldest_first.find("starting").expect("listed");
        let done = oldest_first.find("FETCH done").expect("listed");
        assert!(starting < done);
    }

    #[test]
    fn network_lists_keep_the_newest_matching_requests() {
        let request = |id: &str, url: &str| NetworkEntry {
            id: id.to_string(),
            kind: "fetch".to_string(),
            method: "GET".to_string(),
            url: url.to_string(),
            status: Some(200),
            status_text: "OK".to_string(),
            ..NetworkEntry::default()
        };
        let log = NetworkLog {
            entries: vec![
                request("r1", "http://localhost/API/users"),
                request("r2", "http://localhost/static/app.js"),
                request("r3", "http://localhost/api/orders"),
            ],
            dropped: 2,
        };
        let text = format_network(&log, Some("api"), 50);
        assert!(text.contains("r1 GET http://localhost/API/users -> 200 OK"));
        assert!(text.contains("r3"));
        assert!(!text.contains("r2"));
        assert!(text.contains("2 older requests"));
        let newest = format_network(&log, None, 1);
        assert!(newest.contains("r3"));
        assert!(!newest.contains("r1"));
    }

    #[test]
    fn eval_values_are_shown_as_json() {
        assert_eq!(
            format_eval_value(&json!({ "json": "{\"a\":1}", "type": "object" })).expect("ok"),
            "{\"a\":1}"
        );
        assert_eq!(
            format_eval_value(&json!({ "type": "undefined" })).expect("ok"),
            "undefined"
        );
        let error = format_eval_value(&json!({ "error": "nope", "stack": "" }))
            .expect_err("a rejection is an error");
        assert!(error.to_string().contains("nope"));
        let long = "x".repeat(MAX_CHARS_LIMIT + 10);
        let shown = format_eval_value(&json!({ "json": long })).expect("ok");
        assert!(shown.contains("[Cut off at"));
    }

    #[test]
    fn stopping_cancels_running_commands_and_refuses_new_ones() {
        let mut state = AiTabState::new();
        let mut ticket = state.begin_command().expect("not paused");
        assert!(state.is_running());
        state.stop();
        assert!(!state.is_running());
        assert_eq!(ticket.cancelled.try_recv(), Ok(Some(())));
        let error = state.begin_command().err().expect("paused");
        assert_eq!(error.to_string(), STOPPED_BY_USER);
        state.paused = false;
        assert!(state.begin_command().is_ok());
    }

    #[test]
    fn only_the_requested_navigation_fails_when_blocked() {
        let mut state = AiTabState::new();
        state.requested_navigation = Some("http://localhost:3000/".to_string());
        let mut waiter = state.expect_navigation();
        // An iframe of the page (WebKitGTK reports those too).
        state.navigation_blocked("https://ads.test/frame", "`ads.test` is not allowed");
        assert_eq!(waiter.try_recv(), Ok(None));
        assert!(state.blocked.is_empty());
        state.navigation_blocked("http://localhost:3000", "test");
        assert!(matches!(
            waiter.try_recv(),
            Ok(Some(NavigationOutcome::Blocked { .. }))
        ));
        assert_eq!(state.blocked.len(), 1);

        // Back, forward and reload have no URL to compare with.
        let mut waiter = state.expect_navigation();
        state.navigation_blocked("https://evil.test/", "test");
        assert!(matches!(
            waiter.try_recv(),
            Ok(Some(NavigationOutcome::Blocked { .. }))
        ));
    }

    #[test]
    fn clipboard_shortcuts_are_recognised() {
        for chord in [
            "ctrl+v",
            "ctrl+shift+v",
            "meta+v",
            "ctrl+c",
            "cmd+x",
            "shift+Insert",
            "ctrl+Insert",
            "shift+Delete",
            "Paste",
        ] {
            let press = parse_key_sequence(chord).expect("parses").remove(0);
            assert!(press.uses_the_clipboard(), "{chord}");
        }
        for chord in [
            "v", "shift+v", "alt+v", "ctrl+a", "Delete", "Insert", "Enter",
        ] {
            let press = parse_key_sequence(chord).expect("parses").remove(0);
            assert!(!press.uses_the_clipboard(), "{chord}");
        }
    }

    #[test]
    fn ai_tabs_are_not_created_without_the_egress_proxy() {
        let error = ensure_egress_proxy(None).expect_err("no proxy, no AI tab");
        assert!(
            error.to_string().contains("never connect directly"),
            "{error}"
        );
        let proxy = Url::parse("http://127.0.0.1:49152/").expect("valid url");
        assert!(ensure_egress_proxy(Some(&proxy)).is_ok());
    }

    #[test]
    fn a_redirect_to_a_blocked_host_is_named_when_the_load_fails() {
        let mut state = AiTabState::new();
        let mut waiter = state.expect_navigation();
        state.requested_navigation = Some("http://localhost:3000/login".to_string());
        state.navigation_blocked(
            "https://sso.evil.test/",
            "`sso.evil.test` is not on the network allowlist",
        );
        assert_eq!(waiter.try_recv(), Ok(None), "the load is still going");
        state.navigation_ended(NavigationOutcome::Finished { succeeded: false });
        match waiter.try_recv() {
            Ok(Some(NavigationOutcome::Blocked { message })) => {
                assert!(message.contains("sso.evil.test"), "{message}")
            }
            other => panic!("expected the blocked redirect, got {other:?}"),
        }

        // A blocked frame of a page that loads anyway is not an error, and is forgotten.
        let mut waiter = state.expect_navigation();
        state.requested_navigation = Some("http://localhost:3000/".to_string());
        state.navigation_blocked("https://ads.test/frame", "test");
        state.navigation_ended(NavigationOutcome::Finished { succeeded: true });
        assert_eq!(
            waiter.try_recv(),
            Ok(Some(NavigationOutcome::Finished { succeeded: true }))
        );
        let mut waiter = state.expect_navigation();
        state.navigation_ended(NavigationOutcome::Finished { succeeded: false });
        assert_eq!(
            waiter.try_recv(),
            Ok(Some(NavigationOutcome::Finished { succeeded: false }))
        );
    }

    #[test]
    fn blocked_events_are_reported_to_the_command_that_caused_them() {
        let mut state = AiTabState::new();
        state.record_blocked("old".to_string());
        let ticket = state.begin_command().expect("not paused");
        state.record_blocked(blocked_navigation_message(
            "https://evil.test/",
            "`evil.test` is not on the network allowlist",
        ));
        let blocked = state.blocked_since(ticket.blocked_before);
        assert_eq!(blocked.len(), 1);
        assert!(blocked[0].contains("evil.test"));
    }

    // -- ワークスペースを通した経路 (偽のページで) ----------------------------

    use crate::webview::{self, WebViewEvent, completed};
    use crate::{ResumeAiControl, StopAiControl};
    use futures::FutureExt as _;
    use gpui::BorrowAppContext as _;
    use gpui::{TestAppContext, VisualTestContext};
    use project::FakeFs;
    use settings::SettingsStore;
    use std::{
        cell::{Cell, RefCell},
        collections::{HashMap, VecDeque},
    };

    /// A page that answers each automation function with queued responses (the last one
    /// repeats) and records what it was asked to do.
    #[derive(Default)]
    struct FakeSurface {
        responses: RefCell<HashMap<String, VecDeque<String>>>,
        scripts: RefCell<Vec<String>>,
        navigations: RefCell<Vec<String>>,
        native_input: Cell<bool>,
        inputs: RefCell<Vec<NativeInput>>,
        /// Scripts are taken but never answered (a hidden WKWebView page).
        unresponsive: Cell<bool>,
        unanswered: RefCell<Vec<oneshot::Sender<Result<String>>>>,
    }

    impl FakeSurface {
        fn respond(&self, function: &str, values: &[Value]) {
            self.responses.borrow_mut().insert(
                function.to_string(),
                values
                    .iter()
                    .map(|value| json!({ "ok": true, "value": value }).to_string())
                    .collect(),
            );
        }

        fn respond_raw(&self, function: &str, raw: &[&str]) {
            self.responses.borrow_mut().insert(
                function.to_string(),
                raw.iter().map(|raw| raw.to_string()).collect(),
            );
        }

        fn calls(&self, function: &str) -> usize {
            self.scripts
                .borrow()
                .iter()
                .filter(|script| function_of(script) == function)
                .count()
        }
    }

    fn function_of(script: &str) -> String {
        if script.contains("a.evalValue(v)") {
            return "eval".to_string();
        }
        match script.split_once("a.call(\"") {
            Some((_, rest)) => rest.split('"').next().unwrap_or_default().to_string(),
            None => "inject".to_string(),
        }
    }

    impl AutomationSurface for FakeSurface {
        fn navigate(&self, url: &str) {
            self.navigations.borrow_mut().push(url.to_string());
        }

        fn go_back(&self) {
            self.navigations.borrow_mut().push("back".to_string());
        }

        fn go_forward(&self) {
            self.navigations.borrow_mut().push("forward".to_string());
        }

        fn reload(&self) {
            self.navigations.borrow_mut().push("reload".to_string());
        }

        fn execute_script(&self, script: &str) -> oneshot::Receiver<Result<String>> {
            self.scripts.borrow_mut().push(script.to_string());
            if self.unresponsive.get() {
                let (sender, receiver) = oneshot::channel();
                self.unanswered.borrow_mut().push(sender);
                return receiver;
            }
            let function = function_of(script);
            if function == "inject" {
                return completed(Ok("0".to_string()));
            }
            let mut responses = self.responses.borrow_mut();
            let response = match responses.get_mut(&function) {
                Some(queue) if queue.len() > 1 => queue.pop_front(),
                Some(queue) => queue.front().cloned(),
                None => None,
            };
            completed(Ok(response.unwrap_or_else(|| {
                json!({ "ok": true, "value": null }).to_string()
            })))
        }

        fn capture_png(&self) -> oneshot::Receiver<Result<Vec<u8>>> {
            completed(Err(anyhow!("no screenshots in tests")))
        }

        fn dispatch_input(&self, input: NativeInput) -> oneshot::Receiver<Result<()>> {
            if !self.native_input.get() {
                return completed(Err(NativeInputUnsupported.into()));
            }
            self.inputs.borrow_mut().push(input);
            completed(Ok(()))
        }
    }

    struct Fixture {
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        panel: Entity<BrowserPanel>,
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
        });
    }

    fn set_ai_control(enabled: bool, cx: &mut VisualTestContext) {
        cx.update(|_, cx| {
            cx.update_global(|store: &mut SettingsStore, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.browser.get_or_insert_default().ai_control = Some(enabled);
                });
            });
        });
    }

    async fn fixture(cx: &mut TestAppContext) -> (Fixture, VisualTestContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle = cx.add_window({
            let project = project.clone();
            |window, cx| MultiWorkspace::test_new(project, window, cx)
        });
        let workspace = window_handle
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("the window holds a workspace");
        let mut visual = VisualTestContext::from_window(window_handle.into(), cx);
        let panel = workspace.update_in(&mut visual, |workspace, window, cx| {
            let panel = cx.new(|cx| BrowserPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        set_ai_control(true, &mut visual);
        visual.run_until_parked();
        (
            Fixture {
                project,
                workspace,
                panel,
            },
            visual,
        )
    }

    fn run(
        fixture: &Fixture,
        command: BrowserCommand,
        cx: &mut VisualTestContext,
    ) -> Task<Result<BrowserOutput>> {
        let project = fixture.project.clone();
        let app: &mut TestAppContext = cx;
        app.update(|cx| {
            browser_control::browser_control(cx)
                .expect("browser_panel::init registers the provider")
                .run(&project, command, cx)
        })
    }

    fn finished(
        task: Task<Result<BrowserOutput>>,
        cx: &mut VisualTestContext,
    ) -> Result<BrowserOutput> {
        cx.run_until_parked();
        task.now_or_never()
            .expect("the command should have finished")
    }

    fn advance(milliseconds: u64, cx: &mut VisualTestContext) {
        cx.executor()
            .advance_clock(Duration::from_millis(milliseconds));
        cx.run_until_parked();
    }

    fn ai_tabs_of(fixture: &Fixture, cx: &mut VisualTestContext) -> Vec<AiTab> {
        fixture
            .workspace
            .update(cx, |workspace, cx| ai_tabs(workspace, cx))
    }

    fn open_tab_with_fake(
        fixture: &Fixture,
        cx: &mut VisualTestContext,
    ) -> (AiTab, Rc<FakeSurface>) {
        fixture.panel.update_in(cx, |panel, window, cx| {
            panel.open_ai_tab(
                Some("http://localhost:3000/".to_string()),
                NewTabPlacement::Foreground,
                window,
                cx,
            );
        });
        cx.run_until_parked();
        let tab = ai_tabs_of(fixture, cx)
            .pop()
            .expect("the AI tab is in the panel");
        let fake = Rc::new(FakeSurface::default());
        let surface: Rc<dyn AutomationSurface> = fake.clone();
        tab.view.update(cx, |view, cx| {
            view.set_automation_surface_for_test(surface, cx)
        });
        (tab, fake)
    }

    fn error_text(result: Result<BrowserOutput>) -> String {
        match result {
            Ok(output) => panic!("expected an error, got {:?}", output.text),
            Err(error) => format!("{error:#}"),
        }
    }

    #[gpui::test]
    async fn commands_are_refused_while_ai_control_is_off(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        let (_tab, fake) = open_tab_with_fake(&fixture, cx);
        set_ai_control(false, cx);
        cx.update(|_, cx| {
            let control = browser_control::browser_control(cx).expect("registered");
            assert!(!control.is_enabled(cx));
        });
        for command in [
            BrowserCommand::ListTabs,
            BrowserCommand::PageText {
                tab: None,
                max_chars: 100,
            },
        ] {
            let error = error_text(finished(run(&fixture, command, cx), cx));
            assert_eq!(error, browser_control::AI_CONTROL_OFF_MESSAGE);
        }
        assert!(fake.scripts.borrow().is_empty(), "nothing reached the page");

        set_ai_control(true, cx);
        let output = finished(run(&fixture, BrowserCommand::ListTabs, cx), cx).expect("enabled");
        assert!(
            output.text.contains("http://localhost:3000/"),
            "{}",
            output.text
        );
    }

    #[gpui::test]
    async fn the_user_s_own_tabs_are_never_listed(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        fixture.panel.update_in(cx, |panel, window, cx| {
            panel.open_tab(
                Some("https://outlook.office.com/".to_string()),
                NewTabPlacement::Foreground,
                window,
                cx,
            );
        });
        cx.run_until_parked();
        let output = finished(run(&fixture, BrowserCommand::ListTabs, cx), cx).expect("listing");
        assert!(!output.text.contains("outlook"), "{}", output.text);
        let error = error_text(finished(
            run(
                &fixture,
                BrowserCommand::PageText {
                    tab: None,
                    max_chars: 100,
                },
                cx,
            ),
            cx,
        ));
        assert_eq!(error, NO_AI_TAB);
    }

    #[gpui::test]
    async fn tabs_resolve_to_the_named_or_the_current_one(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        let (first, first_fake) = open_tab_with_fake(&fixture, cx);
        let (second, second_fake) = open_tab_with_fake(&fixture, cx);
        assert_eq!(first.id, TabId("1".to_string()));
        assert_eq!(second.id, TabId("2".to_string()));
        for fake in [&first_fake, &second_fake] {
            fake.respond("viewport", &[json!({ "width": 800, "height": 600 })]);
        }

        let listing = finished(run(&fixture, BrowserCommand::ListTabs, cx), cx).expect("listing");
        assert!(listing.text.contains("* 2:"), "{}", listing.text);

        // `tab` omitted = the most recently used tab.
        finished(
            run(
                &fixture,
                BrowserCommand::Eval {
                    tab: None,
                    expression: "1".to_string(),
                },
                cx,
            ),
            cx,
        )
        .expect("eval");
        assert_eq!(second_fake.calls("eval"), 1);
        assert_eq!(first_fake.calls("eval"), 0);

        // Naming a tab uses it and makes it the current one.
        finished(
            run(
                &fixture,
                BrowserCommand::Eval {
                    tab: Some(first.id),
                    expression: "1".to_string(),
                },
                cx,
            ),
            cx,
        )
        .expect("eval");
        assert_eq!(first_fake.calls("eval"), 1);
        finished(
            run(
                &fixture,
                BrowserCommand::Eval {
                    tab: None,
                    expression: "1".to_string(),
                },
                cx,
            ),
            cx,
        )
        .expect("eval");
        assert_eq!(first_fake.calls("eval"), 2);

        let error = error_text(finished(
            run(
                &fixture,
                BrowserCommand::Eval {
                    tab: Some(TabId("9".to_string())),
                    expression: "1".to_string(),
                },
                cx,
            ),
            cx,
        ));
        assert!(error.contains("`9`"), "{error}");

        // Selecting a tab makes it current again.
        finished(
            run(&fixture, BrowserCommand::SelectTab { tab: second.id }, cx),
            cx,
        )
        .expect("select");
        finished(
            run(
                &fixture,
                BrowserCommand::Eval {
                    tab: None,
                    expression: "1".to_string(),
                },
                cx,
            ),
            cx,
        )
        .expect("eval");
        assert_eq!(second_fake.calls("eval"), 2);
    }

    #[gpui::test]
    async fn a_stopped_tab_cancels_its_command_and_refuses_more(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        let (tab, fake) = open_tab_with_fake(&fixture, cx);
        fake.respond("waitCheck", &[json!({ "text": false })]);

        let waiting = run(
            &fixture,
            BrowserCommand::WaitFor {
                tab: None,
                text: Some("Done".to_string()),
                selector: None,
                timeout_ms: 60_000,
            },
            cx,
        );
        cx.run_until_parked();
        assert!(tab.view.read_with(cx, |view, _| view.ai_is_running()));

        tab.view.update_in(cx, |view, window, cx| {
            view.stop_ai_control(&StopAiControl, window, cx)
        });
        let error = error_text(finished(waiting, cx));
        assert_eq!(error, STOPPED_BY_USER);
        assert!(!tab.view.read_with(cx, |view, _| view.ai_is_running()));
        assert!(tab.view.read_with(cx, |view, _| view.ai_is_paused()));

        let scripts_before = fake.scripts.borrow().len();
        let error = error_text(finished(
            run(
                &fixture,
                BrowserCommand::Eval {
                    tab: None,
                    expression: "1".to_string(),
                },
                cx,
            ),
            cx,
        ));
        assert_eq!(error, STOPPED_BY_USER);
        assert_eq!(fake.scripts.borrow().len(), scripts_before, "nothing ran");

        tab.view.update_in(cx, |view, window, cx| {
            view.resume_ai_control(&ResumeAiControl, window, cx)
        });
        finished(
            run(
                &fixture,
                BrowserCommand::Eval {
                    tab: None,
                    expression: "1".to_string(),
                },
                cx,
            ),
            cx,
        )
        .expect("resumed");
    }

    #[gpui::test]
    async fn a_tab_without_a_browser_reports_why(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        let (tab, _fake) = open_tab_with_fake(&fixture, cx);
        let message = webview::runtime_missing_message();
        tab.view.update(cx, |view, cx| {
            view.set_surface_unavailable_for_test(message.clone(), cx)
        });
        let error = error_text(finished(
            run(
                &fixture,
                BrowserCommand::PageText {
                    tab: None,
                    max_chars: 100,
                },
                cx,
            ),
            cx,
        ));
        assert_eq!(error, message.to_string());
    }

    #[gpui::test]
    async fn navigation_is_checked_against_the_allowlist_first(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        let navigate = |url: &str| BrowserCommand::Navigate {
            tab: None,
            target: NavigateTarget::Url(url.to_string()),
            timeout_ms: 5_000,
        };

        // No allowlist entry and no AI tab yet: refused before a tab is opened.
        for denied in ["https://evil.test/", "file:///etc/passwd"] {
            let error = error_text(finished(run(&fixture, navigate(denied), cx), cx));
            assert!(error.contains(denied), "{error}");
            assert!(
                ai_tabs_of(&fixture, cx).is_empty(),
                "no tab was opened for {denied}"
            );
        }
        let error = error_text(finished(
            run(&fixture, navigate("https://evil.test/"), cx),
            cx,
        ));
        assert!(error.contains("network allowlist"), "{error}");

        // An allowlist edit applies to the very next command.
        let policy = EgressPolicyHandle::default();
        cx.update(|_, cx| cx.set_global(EgressAllowlistGlobal(policy.clone())));
        let (tab, fake) = open_tab_with_fake(&fixture, cx);
        let (allowed, _) = http_client::EgressPolicy::from_host_strings(["evil.test"]);
        policy.set(allowed);

        let loading = run(&fixture, navigate("https://evil.test/"), cx);
        cx.run_until_parked();
        assert_eq!(fake.navigations.borrow().as_slice(), ["https://evil.test/"]);
        tab.view.update_in(cx, |view, window, cx| {
            view.handle_webview_event_for_test(
                WebViewEvent::UrlChanged("https://evil.test/".to_string()),
                window,
                cx,
            );
            view.handle_webview_event_for_test(
                WebViewEvent::TitleChanged("Evil".to_string()),
                window,
                cx,
            );
            view.handle_webview_event_for_test(
                WebViewEvent::LoadFinished { succeeded: true },
                window,
                cx,
            );
        });
        let output = finished(loading, cx).expect("loaded");
        assert_eq!(output.text, "Loaded https://evil.test/ (title: Evil).");
    }

    #[gpui::test]
    async fn navigating_without_an_ai_tab_opens_one(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        // Process-wide; no test expects it to be absent (the refusal is tested on
        // `ensure_egress_proxy` itself).
        http_client::set_child_process_proxy(Some(
            Url::parse("http://127.0.0.1:49152/").expect("valid url"),
        ));
        let _loading = run(
            &fixture,
            BrowserCommand::Navigate {
                tab: None,
                target: NavigateTarget::Url("http://localhost:5173/".to_string()),
                timeout_ms: 1_000,
            },
            cx,
        );
        cx.run_until_parked();
        let tabs = ai_tabs_of(&fixture, cx);
        assert_eq!(tabs.len(), 1);
        let in_panel = fixture.panel.read_with(cx, |panel, cx| {
            panel.center.panes().iter().any(|pane| {
                pane.read(cx)
                    .items()
                    .any(|item| item.item_id() == tabs[0].item.entity_id())
            })
        });
        assert!(in_panel, "the AI tab opens in the browser panel");
    }

    #[gpui::test]
    async fn a_blocked_navigation_fails_the_command_that_caused_it(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        let (tab, fake) = open_tab_with_fake(&fixture, cx);
        fake.respond(
            "targetPoint",
            &[json!({ "x": 10.0, "y": 20.0, "description": "link \"Out\" [e1]" })],
        );
        fake.respond("click", &[json!({ "description": "link \"Out\"" })]);

        let clicking = run(
            &fixture,
            BrowserCommand::Click {
                tab: None,
                target: ElementTarget::Ref("e1".to_string()),
                button: MouseButton::Left,
                click_count: 1,
                modifiers: Vec::new(),
            },
            cx,
        );
        cx.run_until_parked();
        // No trusted input in this backend: the click was synthesised in the page.
        assert_eq!(fake.calls("click"), 1);
        tab.view.update_in(cx, |view, window, cx| {
            view.handle_webview_event_for_test(
                WebViewEvent::NavigationBlocked {
                    url: "https://evil.test/".to_string(),
                    reason: "`evil.test` is not on the network allowlist".to_string(),
                },
                window,
                cx,
            );
        });
        advance(500, cx);
        let error = error_text(finished(clicking, cx));
        assert!(error.starts_with("Clicked link"), "{error}");
        assert!(error.contains("https://evil.test/"), "{error}");
        assert!(error.contains("not on the network allowlist"), "{error}");
    }

    #[gpui::test]
    async fn native_input_is_preferred_when_the_backend_has_it(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        let (_tab, fake) = open_tab_with_fake(&fixture, cx);
        fake.native_input.set(true);
        fake.respond(
            "targetPoint",
            &[json!({ "x": 10.0, "y": 20.0, "description": "button \"Go\" [e1]" })],
        );
        let clicking = run(
            &fixture,
            BrowserCommand::Click {
                tab: None,
                target: ElementTarget::Ref("e1".to_string()),
                button: MouseButton::Left,
                click_count: 2,
                modifiers: vec![KeyModifier::Shift],
            },
            cx,
        );
        advance(500, cx);
        finished(clicking, cx).expect("clicked");
        assert_eq!(
            fake.calls("click"),
            0,
            "nothing was synthesised in the page"
        );
        assert_eq!(
            fake.inputs.borrow().as_slice(),
            [NativeInput::MouseClick {
                x: 10.0,
                y: 20.0,
                button: MouseButton::Left,
                click_count: 2,
                modifiers: vec![KeyModifier::Shift],
            }]
        );
    }

    #[gpui::test]
    async fn password_fields_are_never_typed_into(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        let (_tab, fake) = open_tab_with_fake(&fixture, cx);
        fake.native_input.set(true);
        fake.respond("focus", &[json!({ "description": "textbox \"Password\"" })]);
        fake.respond(
            "field",
            &[json!({ "tag": "input", "type": "password", "autocomplete": "", "description": "textbox \"Password\"" })],
        );

        let typing = run(
            &fixture,
            BrowserCommand::Type {
                tab: None,
                element: Some("e3".to_string()),
                text: "hunter2".to_string(),
                submit: true,
            },
            cx,
        );
        let error = error_text(finished(typing, cx));
        assert_eq!(error, SENSITIVE_FIELD_REFUSAL);
        let pressing = run(
            &fixture,
            BrowserCommand::PressKey {
                tab: None,
                keys: "a b".to_string(),
                repeat: 1,
            },
            cx,
        );
        assert_eq!(error_text(finished(pressing, cx)), SENSITIVE_FIELD_REFUSAL);
        let setting = run(
            &fixture,
            BrowserCommand::SetValue {
                tab: None,
                element: "e3".to_string(),
                value: json!("hunter2"),
            },
            cx,
        );
        assert_eq!(error_text(finished(setting, cx)), SENSITIVE_FIELD_REFUSAL);

        assert!(fake.inputs.borrow().is_empty(), "no key reached the page");
        assert_eq!(fake.calls("typeText"), 0);
        assert_eq!(fake.calls("pressKey"), 0);
        assert_eq!(fake.calls("setValue"), 0);

        // Enter alone (submitting a form the user filled in) is not text and goes through.
        let submitting = run(
            &fixture,
            BrowserCommand::PressKey {
                tab: None,
                keys: "Enter".to_string(),
                repeat: 1,
            },
            cx,
        );
        advance(500, cx);
        finished(submitting, cx).expect("Enter is allowed");
    }

    #[gpui::test]
    async fn file_inputs_are_never_clicked_set_or_activated(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        let (_tab, fake) = open_tab_with_fake(&fixture, cx);
        fake.native_input.set(true);
        fake.respond(
            "targetPoint",
            &[json!({
                "x": 10.0,
                "y": 20.0,
                "description": "label \"Upload\" [e4]",
                "opensFileChooser": true,
            })],
        );
        let clicking = run(
            &fixture,
            BrowserCommand::Click {
                tab: None,
                target: ElementTarget::Ref("e4".to_string()),
                button: MouseButton::Left,
                click_count: 1,
                modifiers: Vec::new(),
            },
            cx,
        );
        assert_eq!(error_text(finished(clicking, cx)), FILE_UPLOAD_REFUSAL);

        fake.respond(
            "field",
            &[json!({ "tag": "input", "type": "file", "description": "button \"Choose File\"" })],
        );
        let setting = run(
            &fixture,
            BrowserCommand::SetValue {
                tab: None,
                element: "e5".to_string(),
                value: json!("secrets.txt"),
            },
            cx,
        );
        assert_eq!(error_text(finished(setting, cx)), FILE_UPLOAD_REFUSAL);
        for keys in ["Enter", "Space", "Tab Enter"] {
            let pressing = run(
                &fixture,
                BrowserCommand::PressKey {
                    tab: None,
                    keys: keys.to_string(),
                    repeat: 1,
                },
                cx,
            );
            assert_eq!(
                error_text(finished(pressing, cx)),
                FILE_UPLOAD_REFUSAL,
                "{keys}"
            );
        }

        assert_eq!(
            fake.inputs.borrow().as_slice(),
            [NativeInput::Key {
                key: "Tab".to_string(),
                code: "Tab".to_string(),
                modifiers: Vec::new(),
            }],
            "only the Tab before the refused Enter reached the page"
        );
        assert_eq!(fake.calls("click"), 0);
        assert_eq!(fake.calls("setValue"), 0);
        assert_eq!(fake.calls("pressKey"), 0);

        // Other keys on a file input do nothing that needs refusing.
        let pressing = run(
            &fixture,
            BrowserCommand::PressKey {
                tab: None,
                keys: "Escape".to_string(),
                repeat: 1,
            },
            cx,
        );
        advance(500, cx);
        finished(pressing, cx).expect("Escape is allowed");
    }

    #[gpui::test]
    async fn clipboard_shortcuts_never_use_trusted_input(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        let (_tab, fake) = open_tab_with_fake(&fixture, cx);
        fake.native_input.set(true);
        let pressing = run(
            &fixture,
            BrowserCommand::PressKey {
                tab: None,
                keys: "ctrl+a ctrl+c shift+Insert ctrl+v".to_string(),
                repeat: 1,
            },
            cx,
        );
        advance(500, cx);
        finished(pressing, cx).expect("pressed");
        assert_eq!(
            fake.inputs.borrow().as_slice(),
            [NativeInput::Key {
                key: "a".to_string(),
                code: "KeyA".to_string(),
                modifiers: vec![KeyModifier::Control],
            }],
            "only the select-all went through the trusted input"
        );
        assert_eq!(fake.calls("pressKey"), 3, "the others were synthesised");
    }

    #[gpui::test]
    async fn the_field_is_checked_again_after_a_key_that_moves_the_focus(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        let (_tab, fake) = open_tab_with_fake(&fixture, cx);
        fake.native_input.set(true);
        fake.respond(
            "field",
            &[
                json!({ "tag": "input", "type": "email", "description": "textbox \"Email\"" }),
                json!({ "tag": "input", "type": "password", "description": "textbox \"Password\"" }),
            ],
        );
        let pressing = run(
            &fixture,
            BrowserCommand::PressKey {
                tab: None,
                keys: "a Tab b".to_string(),
                repeat: 1,
            },
            cx,
        );
        assert_eq!(error_text(finished(pressing, cx)), SENSITIVE_FIELD_REFUSAL);
        let keys = fake
            .inputs
            .borrow()
            .iter()
            .map(|input| match input {
                NativeInput::Key { key, .. } => key.clone(),
                other => panic!("unexpected input {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            keys,
            ["a", "Tab"],
            "nothing was typed after the focus moved"
        );
    }

    #[gpui::test]
    async fn typing_checks_the_focused_field_and_refuses_foreign_frames(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        let (_tab, fake) = open_tab_with_fake(&fixture, cx);
        fake.native_input.set(true);
        fake.respond("focus", &[json!({ "description": "group \"Card\"" })]);
        fake.respond(
            "field",
            &[json!({ "tag": "iframe", "description": "iframe", "crossOriginFrame": true })],
        );
        let typing = run(
            &fixture,
            BrowserCommand::Type {
                tab: None,
                element: Some("e4".to_string()),
                text: "4242424242424242".to_string(),
                submit: false,
            },
            cx,
        );
        assert_eq!(error_text(finished(typing, cx)), CROSS_ORIGIN_FRAME_REFUSAL);
        assert!(fake.inputs.borrow().is_empty(), "nothing was typed");
        let field_script = fake
            .scripts
            .borrow()
            .iter()
            .find(|script| function_of(script) == "field")
            .cloned()
            .expect("the field was looked at");
        assert!(
            field_script.contains("{\"ref\":null}"),
            "the focused element is checked, not the named one: {field_script}"
        );
    }

    #[gpui::test]
    async fn a_page_that_never_answers_fails_the_command(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        let (_tab, fake) = open_tab_with_fake(&fixture, cx);
        fake.unresponsive.set(true);
        let mut reading = run(
            &fixture,
            BrowserCommand::PageText {
                tab: None,
                max_chars: 100,
            },
            cx,
        );
        advance(BACKEND_ANSWER_TIMEOUT.as_millis() as u64 - 1_000, cx);
        assert!(
            (&mut reading).now_or_never().is_none(),
            "still waiting before the deadline"
        );
        advance(2_000, cx);
        let error = error_text(finished(reading, cx));
        assert!(error.contains("did not answer"), "{error}");
        assert_eq!(fake.unanswered.borrow().len(), 1);
    }

    #[gpui::test]
    async fn a_backend_that_drops_its_answer_fails_the_command(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        let (_tab, fake) = open_tab_with_fake(&fixture, cx);
        fake.unresponsive.set(true);
        let reading = run(
            &fixture,
            BrowserCommand::PageText {
                tab: None,
                max_chars: 100,
            },
            cx,
        );
        cx.run_until_parked();
        assert_eq!(fake.unanswered.borrow().len(), 1);
        // What a backend does when its page goes away with a script in flight.
        fake.unanswered.borrow_mut().clear();
        cx.run_until_parked();
        let error = error_text(finished(reading, cx));
        assert!(error.contains("went away"), "{error}");
    }

    #[gpui::test]
    async fn closing_the_tab_ends_its_command(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        let (tab, fake) = open_tab_with_fake(&fixture, cx);
        fake.respond("waitCheck", &[json!({ "text": false })]);
        let waiting = run(
            &fixture,
            BrowserCommand::WaitFor {
                tab: None,
                text: Some("Done".to_string()),
                selector: None,
                timeout_ms: 60_000,
            },
            cx,
        );
        cx.run_until_parked();
        let view = tab.view.downgrade();
        let (pane, item_id) = (tab.pane.clone(), tab.item.entity_id());
        drop(tab);
        pane.update_in(cx, |pane, window, cx| {
            pane.close_item_by_id(item_id, workspace::SaveIntent::Skip, window, cx)
        })
        .detach();
        advance(POLL_INTERVAL.as_millis() as u64, cx);
        assert_eq!(error_text(finished(waiting, cx)), TAB_CLOSED);
        assert!(
            view.upgrade().is_none(),
            "the closed tab's view was dropped"
        );
    }

    #[gpui::test]
    async fn an_ai_tab_that_waited_for_the_proxy_starts_once_it_runs(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        let (tab, _fake) = open_tab_with_fake(&fixture, cx);
        // Process-wide; no test expects it to be absent (see `ensure_egress_proxy`).
        http_client::set_child_process_proxy(Some(
            Url::parse("http://127.0.0.1:49152/").expect("valid url"),
        ));
        tab.view.update(cx, |view, cx| {
            view.set_surface_unavailable_for_test("no proxy yet".into(), cx);
            view.wait_for_egress_proxy(cx);
        });
        let waiting = |cx: &mut VisualTestContext| {
            tab.view.update(cx, |view, _| {
                matches!(view.automation_status(), AutomationStatus::Waiting { .. })
            })
        };
        assert!(!waiting(cx), "still unavailable before the next look");
        advance(1_500, cx);
        assert!(waiting(cx), "the tab tries to start its browser again");
    }

    #[gpui::test]
    async fn a_stopped_tab_cannot_be_closed_or_selected_by_the_ai(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        let (tab, _fake) = open_tab_with_fake(&fixture, cx);
        tab.view.update_in(cx, |view, window, cx| {
            view.stop_ai_control(&StopAiControl, window, cx)
        });
        for command in [
            BrowserCommand::SelectTab {
                tab: tab.id.clone(),
            },
            BrowserCommand::CloseTab {
                tab: tab.id.clone(),
            },
        ] {
            assert_eq!(
                error_text(finished(run(&fixture, command, cx), cx)),
                STOPPED_BY_USER
            );
        }
        assert_eq!(ai_tabs_of(&fixture, cx).len(), 1, "the tab is still open");
    }

    #[gpui::test]
    async fn a_returned_promise_is_polled_until_it_settles(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        let (_tab, fake) = open_tab_with_fake(&fixture, cx);
        fake.respond("eval", &[json!({ "pending": 7 })]);
        fake.respond(
            "evalResult",
            &[
                json!({ "state": "pending" }),
                json!({ "state": "pending" }),
                json!({ "type": "number", "json": "42" }),
            ],
        );
        let evaluating = run(
            &fixture,
            BrowserCommand::Eval {
                tab: None,
                expression: "fetch('/api').then(r => r.json())".to_string(),
            },
            cx,
        );
        for _ in 0..3 {
            advance(POLL_INTERVAL.as_millis() as u64, cx);
        }
        let output = finished(evaluating, cx).expect("settled");
        assert_eq!(output.text, "42");
        assert_eq!(fake.calls("evalResult"), 3);
        assert!(
            fake.scripts.borrow()[0].contains("fetch('/api')"),
            "the expression is part of the script"
        );
    }

    #[gpui::test]
    async fn a_promise_that_never_settles_times_out(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        let (_tab, fake) = open_tab_with_fake(&fixture, cx);
        fake.respond("eval", &[json!({ "pending": 1 })]);
        fake.respond("evalResult", &[json!({ "state": "pending" })]);
        let evaluating = run(
            &fixture,
            BrowserCommand::Eval {
                tab: None,
                expression: "new Promise(() => {})".to_string(),
            },
            cx,
        );
        advance(EVAL_TIMEOUT.as_millis() as u64 + 1_000, cx);
        let error = error_text(finished(evaluating, cx));
        assert!(error.contains("did not settle"), "{error}");
    }

    #[gpui::test]
    async fn waiting_polls_until_the_text_shows_up_or_time_runs_out(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        let (_tab, fake) = open_tab_with_fake(&fixture, cx);
        fake.respond(
            "waitCheck",
            &[
                json!({ "text": false, "selector": true }),
                json!({ "text": false, "selector": true }),
                json!({ "text": true, "selector": true }),
            ],
        );
        let waiting = run(
            &fixture,
            BrowserCommand::WaitFor {
                tab: None,
                text: Some("Saved".to_string()),
                selector: Some("#toast".to_string()),
                timeout_ms: 5_000,
            },
            cx,
        );
        for _ in 0..3 {
            advance(POLL_INTERVAL.as_millis() as u64, cx);
        }
        let output = finished(waiting, cx).expect("found");
        assert!(output.text.starts_with("Found"), "{}", output.text);
        assert_eq!(fake.calls("waitCheck"), 3);

        fake.respond("waitCheck", &[json!({ "text": false })]);
        let waiting = run(
            &fixture,
            BrowserCommand::WaitFor {
                tab: None,
                text: Some("Never".to_string()),
                selector: None,
                timeout_ms: 1_000,
            },
            cx,
        );
        advance(1_500, cx);
        let error = error_text(finished(waiting, cx));
        assert!(error.starts_with("Timed out after 1000 ms"), "{error}");
    }

    #[gpui::test]
    async fn an_uninstrumented_page_is_instrumented_and_asked_again(cx: &mut TestAppContext) {
        let (fixture, mut cx) = fixture(cx).await;
        let cx = &mut cx;
        let (_tab, fake) = open_tab_with_fake(&fixture, cx);
        fake.respond_raw(
            "pageText",
            &[
                r#"{"notInjected":true}"#,
                r#"{"ok":true,"value":{"title":"T","url":"http://localhost:3000/","text":"Hello","truncated":false}}"#,
            ],
        );
        let output = finished(
            run(
                &fixture,
                BrowserCommand::PageText {
                    tab: None,
                    max_chars: 100,
                },
                cx,
            ),
            cx,
        )
        .expect("read");
        assert_eq!(output.text, "T - http://localhost:3000/\n\nHello");
        let scripts = fake.scripts.borrow();
        assert_eq!(scripts.len(), 3);
        assert!(
            scripts[1].contains(NAMESPACE) && scripts[1].len() > 10_000,
            "the instrumentation was injected"
        );
    }
}
