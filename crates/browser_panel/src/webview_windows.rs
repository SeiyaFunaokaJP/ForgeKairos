//! WebView2 host for [`crate::BrowserView`] (ADR-F19).
//!
//! The controller is created as a child of the GPUI top-level HWND, so every
//! COM object here lives on the UI thread and is deliberately `!Send`. WebView2
//! delivers its callbacks on that same thread, but without a GPUI context, so
//! everything the view needs to react to is forwarded through an unbounded
//! channel that the view drains from a GPUI task.
//!
//! Several browsing surfaces can exist at once (the dock panel plus any number
//! of editor tabs), and they all share one [`ICoreWebView2Environment`]: a
//! user data folder can only be opened by one environment, so a second one over
//! the same folder fails outright.

use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    ffi::c_void,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow, bail};
use futures::channel::{mpsc::UnboundedSender, oneshot};
use gpui::{SharedString, Window};
use util::ResultExt as _;
use webview2_com::{
    AcceleratorKeyPressedEventHandler, CoreWebView2EnvironmentOptions,
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    DocumentTitleChangedEventHandler, FocusChangedEventHandler, HistoryChangedEventHandler,
    Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_COLOR, COREWEBVIEW2_KEY_EVENT_KIND, COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN,
        COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC, COREWEBVIEW2_PREFERRED_COLOR_SCHEME,
        COREWEBVIEW2_PREFERRED_COLOR_SCHEME_DARK, COREWEBVIEW2_PREFERRED_COLOR_SCHEME_LIGHT,
        CreateCoreWebView2EnvironmentWithOptions, ICoreWebView2, ICoreWebView2_13,
        ICoreWebView2Controller, ICoreWebView2Controller2, ICoreWebView2Controller3,
        ICoreWebView2Environment, ICoreWebView2EnvironmentOptions,
    },
    NavigationCompletedEventHandler, NewWindowRequestedEventHandler, SourceChangedEventHandler,
    take_pwstr,
};
use windows::{
    Win32::{
        Foundation::{E_POINTER, HWND, LPARAM, POINT, RECT},
        Graphics::Gdi::{
            CombineRgn, CreateRectRgn, CreateRoundRectRgn, DeleteObject, RGN_DIFF, RGN_ERROR,
            ScreenToClient, SetWindowRgn,
        },
        UI::{
            Input::KeyboardAndMouse::{
                EnableWindow, GetFocus, IsWindowEnabled, SetFocus, VK_ESCAPE,
            },
            WindowsAndMessaging::{
                EnumChildWindows, GWL_EXSTYLE, GetClassNameW, GetParent, GetWindowLongPtrW,
                GetWindowRect, HWND_TOP, IsChild, IsIconic, IsWindowVisible, SW_SHOWNA,
                SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowPos, ShowWindow,
                WS_EX_NOREDIRECTIONBITMAP,
            },
        },
    },
    core::{BOOL, HSTRING, Interface as _, PCWSTR},
};

pub(crate) use crate::webview_types::*;

/// Corner radius of the holes cut for gpui's overlays, in logical pixels.
///
/// Tooltips, context menus and popovers are all drawn by `ui::elevation_2`,
/// which rounds its corners by `rounded_lg` (0.5rem, 8px at the default rem
/// size). The four corners of the bounding box are not covered by the overlay,
/// so a square hole would cut away page the user is still meant to see through
/// gpui's punch-through (and, without DirectComposition, expose the panel's own
/// dark background instead).
const OVERLAY_CORNER_RADIUS: f64 = 8.;

/// How deep, in logical pixels, an overlay may reach into the surface across
/// one of its edges and still not be cut out, as long as it carries on past
/// that edge.
///
/// This is for a dock's resize handle: `workspace::dock` draws it as a
/// deferred, occluding 6px strip (`RESIZE_HANDLE_SIZE`) across the dock's 1px
/// border, placed from inside that border, so it reaches 3px into the dock's
/// own content (a browser panel in that dock) and 2px into its neighbour (a
/// browser tab beside the dock). gpui reports it as an overlay, but it paints
/// nothing: cutting it out only uncovers the strip, and under
/// DirectComposition gpui's punch-through there is fully transparent, so the
/// desktop behind Zed shows as a gap along the whole edge. The deeper 3px plus
/// room for rounding at fractional scale factors. Real popovers, menus and
/// tooltips are never this thin; one that merely grazes the edge only loses
/// the clickability of that sliver.
///
/// Accepted trade-off: the handle keeps the mouse only where it lies over gpui
/// content (the border, a neighbour that is not a page, the tab and toolbar
/// rows), so with a page on both sides of a dock border only the border and
/// the rows above the pages remain grabbable.
const EDGE_SLIVER_DEPTH: f32 = 4.;

/// Whether the host window was created with `WS_EX_NOREDIRECTIONBITMAP`, which
/// `gpui_windows` does unless `GPUI_DISABLE_DIRECT_COMPOSITION` is set.
///
/// Both cases work but differ in how the page ends up on screen, so the panel
/// reads it to decide what it has to hide. With DirectComposition gpui binds
/// its visual tree with
/// `CreateTargetForHwnd(hwnd, topmost = true)`, so its swap chain composites
/// *above* this child window and the page is only visible because gpui clears
/// the rectangle to transparent (`Window::paint_punch_through`). Without it the
/// swap chain is bound straight to the HWND and the child is in front of
/// everything gpui draws, which is why the overlay holes below exist.
pub(crate) fn host_uses_direct_composition(host_hwnd: isize) -> bool {
    let hwnd = HWND(host_hwnd as *mut c_void);
    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    (ex_style as u32) & WS_EX_NOREDIRECTIONBITMAP.0 != 0
}

pub(crate) fn host_window_handle(window: &Window) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    // `gpui::Window` has its own inherent `window_handle`, so the trait method
    // has to be named explicitly.
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
        _ => None,
    }
}

pub(crate) fn runtime_missing_message() -> SharedString {
    i18n::t!("The WebView2 Runtime wasn't found. Install the Microsoft Edge WebView2 Runtime.")
        .into()
}

/// Handed the shared environment once it exists, or the reason it does not.
/// The error is a message rather than a `windows::core::Error` so that one
/// failure can be reported to every queued surface.
type EnvironmentWaiter = Box<dyn FnOnce(Result<ICoreWebView2Environment, String>)>;

enum EnvironmentSlot {
    /// Nothing has asked for an environment yet, or the last attempt failed.
    /// A failed attempt resets to this so a retry is possible once the user
    /// installs the runtime.
    Idle,
    Creating(Vec<EnvironmentWaiter>),
    Ready(ICoreWebView2Environment),
}

thread_local! {
    /// WebView2's COM objects are UI-thread affine, so the shared environment
    /// is thread local rather than a process-wide global.
    static ENVIRONMENT: RefCell<EnvironmentSlot> = const { RefCell::new(EnvironmentSlot::Idle) };
    /// What the shared environment was created with. Kept only to warn when a
    /// later surface asks for something else: WebView2 has no way to change the
    /// user data folder or the browser arguments after creation.
    static ENVIRONMENT_OPTIONS: RefCell<Option<(PathBuf, Option<String>)>> =
        const { RefCell::new(None) };
}

/// Outcome of queueing a waiter; `Ready` hands the waiter back so it can be
/// invoked outside the `RefCell` borrow.
enum EnvironmentRequest {
    Ready(ICoreWebView2Environment, EnvironmentWaiter),
    Pending,
    Start,
}

fn queue_environment_waiter(waiter: EnvironmentWaiter) -> EnvironmentRequest {
    ENVIRONMENT.with(|slot| {
        let mut slot = slot.borrow_mut();
        match &mut *slot {
            EnvironmentSlot::Ready(environment) => {
                EnvironmentRequest::Ready(environment.clone(), waiter)
            }
            EnvironmentSlot::Creating(waiters) => {
                waiters.push(waiter);
                EnvironmentRequest::Pending
            }
            EnvironmentSlot::Idle => {
                *slot = EnvironmentSlot::Creating(vec![waiter]);
                EnvironmentRequest::Start
            }
        }
    })
}

/// Publishes the result of the one in-flight creation to every queued waiter.
/// On failure the slot goes back to `Idle` so a later surface can try again.
fn finish_environment(outcome: Result<ICoreWebView2Environment, String>) {
    let waiters = ENVIRONMENT.with(|slot| {
        let mut slot = slot.borrow_mut();
        let waiters = match std::mem::replace(&mut *slot, EnvironmentSlot::Idle) {
            EnvironmentSlot::Creating(waiters) => waiters,
            other => {
                *slot = other;
                Vec::new()
            }
        };
        if let Ok(environment) = &outcome {
            *slot = EnvironmentSlot::Ready(environment.clone());
        }
        waiters
    });
    for waiter in waiters {
        waiter(outcome.clone());
    }
}

fn warn_on_environment_option_mismatch(user_data_folder: &Path, proxy_endpoint: Option<&str>) {
    ENVIRONMENT_OPTIONS.with(|options| {
        let options = options.borrow();
        let Some((created_folder, created_proxy)) = options.as_ref() else {
            return;
        };
        if created_folder != user_data_folder {
            log::warn!(
                "WebView2: the shared environment already uses {}; ignoring the request for {}",
                created_folder.display(),
                user_data_folder.display()
            );
        }
        if created_proxy.as_deref() != proxy_endpoint {
            log::warn!(
                "WebView2: the shared environment was created with proxy {:?}; \
                 restart Zed for browser.use_egress_proxy changes to take effect",
                created_proxy
            );
        }
    });
}

/// Hands `waiter` the process-wide WebView2 environment, creating it on first
/// use. Errors returned here are synchronous rejections of the request; a
/// failure that arrives later is delivered to the waiter instead.
fn with_shared_environment(
    user_data_folder: &Path,
    proxy_endpoint: Option<&str>,
    waiter: EnvironmentWaiter,
) -> Result<()> {
    match queue_environment_waiter(waiter) {
        EnvironmentRequest::Ready(environment, waiter) => {
            warn_on_environment_option_mismatch(user_data_folder, proxy_endpoint);
            waiter(Ok(environment));
            Ok(())
        }
        EnvironmentRequest::Pending => {
            warn_on_environment_option_mismatch(user_data_folder, proxy_endpoint);
            Ok(())
        }
        EnvironmentRequest::Start => {
            ENVIRONMENT_OPTIONS.with(|options| {
                *options.borrow_mut() = Some((
                    user_data_folder.to_path_buf(),
                    proxy_endpoint.map(str::to_string),
                ));
            });
            let result = create_shared_environment(user_data_folder, proxy_endpoint);
            if let Err(error) = &result {
                finish_environment(Err(format!("{error:#}")));
            }
            result
        }
    }
}

fn create_shared_environment(user_data_folder: &Path, proxy_endpoint: Option<&str>) -> Result<()> {
    let options = CoreWebView2EnvironmentOptions::default();
    if let Some(proxy_endpoint) = proxy_endpoint {
        // Chromium reads `--proxy-server` once at browser-process start, so it
        // has to be set before the environment exists; there is no runtime
        // equivalent in the WebView2 API.
        unsafe {
            options.set_additional_browser_arguments(format!("--proxy-server={proxy_endpoint}"));
        }
    }
    let options: ICoreWebView2EnvironmentOptions = options.into();
    log::info!(
        "WebView2: requesting the shared environment (user data folder {}, proxy {})",
        user_data_folder.display(),
        proxy_endpoint.unwrap_or("none"),
    );
    let user_data_folder = HSTRING::from(user_data_folder.as_os_str());

    let handler =
        CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(|result, environment| {
            match result.and_then(|()| environment.ok_or_else(|| E_POINTER.into())) {
                Ok(environment) => {
                    log::info!("WebView2: the shared environment is ready");
                    finish_environment(Ok(environment));
                }
                Err(error) => {
                    log::error!("WebView2 environment creation failed: {error:?}");
                    finish_environment(Err(format!("{error}")));
                }
            }
            Ok(())
        }));

    unsafe {
        CreateCoreWebView2EnvironmentWithOptions(
            PCWSTR::null(),
            PCWSTR(user_data_folder.as_ptr()),
            &options,
            &handler,
        )
    }
    .context("failed to request a WebView2 environment")
}

/// How long a controller creation may run before the next request stops
/// waiting for it. WebView2 takes tens of milliseconds; one that neither
/// completes nor releases its handler (the host window closed mid-creation,
/// the browser process wedged) would otherwise leave every later surface, in
/// every window, on its loading message for good.
const CREATION_TIMEOUT: Duration = Duration::from_secs(30);

/// Given the turn, starts a controller creation and keeps the turn until the
/// creation ends, or hands it straight back when there is nothing to wait
/// for: the surface is gone, or the creation failed before it began.
type CreationStarter = Box<dyn FnOnce(CreationTurn) -> Option<CreationTurn>>;

/// Runs controller creations one at a time, in the order they were asked for.
///
/// A surface tells its controller's windows apart from everyone else's by
/// listing the host's children right before its creation starts and again
/// once it has attached. With two creations in flight at once - two surfaces
/// painted in the same frame, such as a restored panel beside a centre tab -
/// each would see the other's window appear too, claim it, and later clip or
/// disable it along with its own.
#[derive(Default)]
struct CreationQueue {
    state: RefCell<CreationQueueState>,
}

#[derive(Default)]
struct CreationQueueState {
    /// Bumped for every creation started, so that the turn of one the queue
    /// gave up on can be told apart from the turn in flight.
    generation: u64,
    in_flight: Option<InFlightCreation>,
    /// Each request's host window, for the logs, and its starter.
    waiting: VecDeque<(isize, CreationStarter)>,
}

struct InFlightCreation {
    generation: u64,
    host_hwnd: isize,
    started_at: Instant,
}

impl CreationQueue {
    /// Starts `start` right away when nothing is in flight, and otherwise
    /// queues it behind the creation in flight - unless that one has run for
    /// longer than [`CREATION_TIMEOUT`], in which case the queue gives up on
    /// it and moves on.
    fn request(self: &Rc<Self>, host_hwnd: isize, start: CreationStarter, now: Instant) {
        {
            let mut state = self.state.borrow_mut();
            state.waiting.push_back((host_hwnd, start));
            if let Some(in_flight) = &state.in_flight {
                let running_for = now.saturating_duration_since(in_flight.started_at);
                if running_for < CREATION_TIMEOUT {
                    log::info!(
                        "WebView2: the controller for host hwnd {host_hwnd:#x} is waiting behind a creation started {} ms ago",
                        running_for.as_millis()
                    );
                    return;
                }
                log::warn!(
                    "WebView2: the controller creation on host hwnd {:#x} has not ended after {} ms; giving up on it and starting the next one",
                    in_flight.host_hwnd,
                    running_for.as_millis()
                );
                state.in_flight = None;
            }
        }
        self.start_next(now);
    }

    /// Ends the creation `generation` belongs to and starts the next one,
    /// unless the queue has already given up on that creation and moved on.
    fn finish(self: &Rc<Self>, generation: u64, now: Instant) {
        {
            let mut state = self.state.borrow_mut();
            let in_flight = state
                .in_flight
                .as_ref()
                .map(|in_flight| in_flight.generation);
            if in_flight != Some(generation) {
                return;
            }
            state.in_flight = None;
        }
        self.start_next(now);
    }

    /// Starts waiting creations until one keeps its turn or none is left.
    ///
    /// A loop rather than recursion through the turn's `Drop`, so that any
    /// number of requests handed straight back is skipped in constant stack.
    fn start_next(self: &Rc<Self>, now: Instant) {
        loop {
            let (turn, start) = {
                let mut state = self.state.borrow_mut();
                let Some((host_hwnd, start)) = state.waiting.pop_front() else {
                    state.in_flight = None;
                    return;
                };
                state.generation += 1;
                let generation = state.generation;
                state.in_flight = Some(InFlightCreation {
                    generation,
                    host_hwnd,
                    started_at: now,
                });
                let turn = CreationTurn {
                    queue: Some(self.clone()),
                    generation,
                };
                (turn, start)
            };
            match start(turn) {
                None => return,
                // The loop starts the next one itself, so the turn must not do
                // it again when it is dropped.
                Some(mut turn) => turn.queue = None,
            }
        }
    }
}

/// Held by the creation in flight; dropping it ends that creation and starts
/// the next one, whichever way the creation ends - attached, failed, or its
/// completion handler released without ever being called.
struct CreationTurn {
    queue: Option<Rc<CreationQueue>>,
    generation: u64,
}

impl Drop for CreationTurn {
    fn drop(&mut self) {
        if let Some(queue) = self.queue.take() {
            queue.finish(self.generation, Instant::now());
        }
    }
}

thread_local! {
    static CONTROLLER_QUEUE: Rc<CreationQueue> = Rc::new(CreationQueue::default());
}

fn request_controller(surface: &Rc<Inner>, environment: ICoreWebView2Environment) {
    let host_hwnd = surface.host_hwnd.0 as isize;
    // Weak, so that a surface dropped while it waits is skipped.
    let surface = Rc::downgrade(surface);
    let start: CreationStarter = Box::new(move |turn| match surface.upgrade() {
        Some(surface) => surface.create_controller(environment, turn),
        None => {
            log::info!(
                "WebView2: skipping the controller of a surface that was closed while it waited"
            );
            Some(turn)
        }
    });
    CONTROLLER_QUEUE
        .with(Rc::clone)
        .request(host_hwnd, start, Instant::now());
}

pub(crate) struct WebView {
    inner: Rc<Inner>,
}

struct Inner {
    host_hwnd: HWND,
    /// Whether gpui composites its swap chain in front of this child window;
    /// see [`host_uses_direct_composition`]. Read once, because the flag comes
    /// from an extended window style that is fixed at window creation.
    composited: bool,
    events: UnboundedSender<WebViewEvent>,
    state: RefCell<State>,
}

#[derive(Default)]
struct State {
    controller: Option<ICoreWebView2Controller>,
    webview: Option<ICoreWebView2>,
    /// Requests issued before the controller finished creating are replayed
    /// once it exists; WebView2 creation takes tens of milliseconds and the
    /// panel is interactive the whole time.
    pending_url: Option<String>,
    pending_dark_mode: Option<bool>,
    pending_background_color: Option<COREWEBVIEW2_COLOR>,
    bounds: RECT,
    /// Parts of `bounds` that gpui paints over this frame, in the same
    /// coordinates. Cut out of the child windows so the overlay stays visible.
    holes: Vec<RECT>,
    scale: f64,
    visible: bool,
    applied_bounds: Option<RECT>,
    /// The `(bounds, holes)` the current window regions were built from. The
    /// holes are relative to the host window, so a move of the surface
    /// invalidates them just as much as a change of the overlays does.
    applied_clip: Option<(RECT, Vec<RECT>)>,
    applied_scale: Option<f64>,
    applied_visible: Option<bool>,
    input_blocked: bool,
    applied_input_blocked: Option<bool>,
    page_has_focus: bool,
    /// A `focus_page` that came while the page's windows were disabled, which
    /// Windows refuses; moved into the page once they are enabled again, if
    /// the view still holds gpui's focus. Dropped whenever the surface hides.
    pending_focus: bool,
    /// Whether the view held gpui's focus at the last paint.
    view_focused: bool,
    /// Set by `WebView`'s `Drop`. A controller that attaches while that drop
    /// runs re-entrantly would otherwise never be closed.
    closed: bool,
    hidden_reason: Option<&'static str>,
    /// The render widget is nudged to the top of the sibling order once, the
    /// first time the surface is made visible; it is a workaround, not
    /// something to repeat per frame.
    nudged_child_windows: bool,
    /// Child HWNDs the host window had before this controller was created, and
    /// the ones it created: its new direct children of the host and whatever
    /// is nested inside them ([`windows_of_new_controller`]). Controllers are
    /// created one at a time ([`CreationQueue`]), so no other surface's direct
    /// child can appear meanwhile. Several surfaces share the host window, so
    /// the nudge must never touch a sibling that is deliberately hidden -
    /// showing it again would leave an invisible window eating clicks.
    children_before_controller: Vec<isize>,
    owned_children: Vec<isize>,
}

impl WebView {
    /// Starts asynchronous creation of a WebView2 environment and controller.
    /// Returning `Ok` only means the request was accepted; readiness (or
    /// failure) arrives as a [`WebViewEvent`].
    pub(crate) fn new(
        host_hwnd: isize,
        options: WebViewOptions,
        events: UnboundedSender<WebViewEvent>,
    ) -> Result<Self> {
        // TEMPORARY STUB (win): the AI profile, the document start script, the
        // navigation filter and the download block are not applied yet, so
        // asking for any of them fails rather than opening an unrestricted page.
        if options.uses_ai_tab_features() {
            bail!("AI tabs are not supported by the WebView2 backend yet");
        }

        let composited = host_uses_direct_composition(host_hwnd);
        let inner = Rc::new(Inner {
            host_hwnd: HWND(host_hwnd as *mut c_void),
            composited,
            events,
            state: RefCell::new(State {
                scale: 1.0,
                hidden_reason: Some("the panel has not been painted yet"),
                ..Default::default()
            }),
        });

        log::info!(
            "WebView2: requesting a surface (host hwnd {host_hwnd:#x}, direct composition on the host window: {composited})",
        );

        // Weak, so that a surface dropped before the environment is ready never
        // asks for a controller.
        let waiter = {
            let surface = Rc::downgrade(&inner);
            Box::new(move |outcome: Result<ICoreWebView2Environment, String>| {
                let Some(surface) = surface.upgrade() else {
                    return;
                };
                match outcome {
                    Ok(environment) => request_controller(&surface, environment),
                    Err(message) => surface.report_failure_message(&message),
                }
            }) as EnvironmentWaiter
        };
        with_shared_environment(
            &options.user_data_folder,
            options.proxy_endpoint.as_deref(),
            waiter,
        )?;

        Ok(Self { inner })
    }

    /// Places the browsing surface under the panel's content area.
    /// `ICoreWebView2Controller::SetBounds` wants client-area coordinates of the
    /// parent HWND in physical pixels, so GPUI's logical bounds are multiplied
    /// by the window's scale factor here (and nowhere else: WebView2 is told not
    /// to follow the monitor DPI on its own).
    ///
    /// The overlays gpui reported for this frame are turned into holes in the
    /// child window's region (ADR-F19). Under DirectComposition gpui already
    /// draws the overlay in front of the page, so the hole is what lets the
    /// mouse reach it: a child HWND hit-tests its whole rectangle, and a
    /// popover or menu over the surface would otherwise be unclickable. Without
    /// DirectComposition the child is in front of gpui's swap chain and the
    /// hole is what makes the overlay visible at all.
    ///
    /// Returns `true` when cutting those holes was held back: this runs while
    /// gpui paints, so the frame that draws the overlay has not been presented
    /// yet, and opening the hole now uncovers - for one frame - whatever is
    /// behind the child window, which is the host window's black class brush or,
    /// under DirectComposition, gpui's punch-through and with it the desktop.
    /// The caller applies it from a next-frame callback with
    /// [`WebView::apply_deferred_clip`] instead. Removing holes is never
    /// deferred - it only ever covers more of the surface back up.
    ///
    /// `view_focused` says whether the view holds gpui's focus: a
    /// [`WebView::focus_page`] refused while the page took no input is only
    /// moved into the page later while it does.
    #[must_use]
    pub(crate) fn sync_viewport(&self, viewport: Viewport, view_focused: bool) -> bool {
        let scale = viewport.scale_factor.max(0.1) as f64;
        let (bounds, holes) = viewport_geometry(&viewport);

        let mut hidden_reason = viewport.hidden_reason;
        if hidden_reason.is_none() && (bounds.right <= bounds.left || bounds.bottom <= bounds.top) {
            hidden_reason = Some("the content rect is empty");
        }
        if hidden_reason.is_none() && unsafe { IsIconic(self.inner.host_hwnd) }.as_bool() {
            hidden_reason = Some("the window is minimized");
        }

        {
            let mut state = self.inner.state.borrow_mut();
            state.bounds = bounds;
            state.holes = holes;
            state.scale = scale;
            state.visible = hidden_reason.is_none();
            state.hidden_reason = hidden_reason;
            state.input_blocked = viewport.input_blocked;
            state.view_focused = view_focused;
            if hidden_reason.is_some() {
                // A page taken off screen must not grab the keyboard when it
                // comes back.
                state.pending_focus = false;
            }
        }
        self.inner.apply_viewport(true)
    }

    /// Cuts the holes [`WebView::sync_viewport`] held back. Meant to be called
    /// once the frame that painted the overlays has been presented.
    pub(crate) fn apply_deferred_clip(&self) {
        self.inner.apply_clip(false);
    }

    /// Hides the surface without touching the cached geometry, so the next
    /// paint can show it again in place.
    pub(crate) fn hide(&self, reason: &'static str) {
        {
            let mut state = self.inner.state.borrow_mut();
            state.visible = false;
            state.hidden_reason = Some(reason);
            state.pending_focus = false;
        }
        self.inner.apply_viewport(false);
    }

    pub(crate) fn navigate(&self, url: &str) {
        match self.inner.webview() {
            Some(webview) => {
                log::info!("WebView2: navigating to {url}");
                let wide = HSTRING::from(url);
                unsafe { webview.Navigate(PCWSTR(wide.as_ptr())) }.log_err();
            }
            None => {
                log::info!("WebView2: queueing a navigation to {url} until the controller exists");
                self.inner.state.borrow_mut().pending_url = Some(url.to_string());
            }
        }
    }

    pub(crate) fn go_back(&self) {
        if let Some(webview) = self.inner.webview() {
            unsafe { webview.GoBack() }.log_err();
        }
    }

    pub(crate) fn go_forward(&self) {
        if let Some(webview) = self.inner.webview() {
            unsafe { webview.GoForward() }.log_err();
        }
    }

    pub(crate) fn reload(&self) {
        if let Some(webview) = self.inner.webview() {
            unsafe { webview.Reload() }.log_err();
        }
    }

    pub(crate) fn set_dark_mode(&self, dark: bool) {
        match self.inner.webview() {
            Some(webview) => apply_dark_mode(&webview, dark),
            None => self.inner.state.borrow_mut().pending_dark_mode = Some(dark),
        }
    }

    /// The colour WebView2 paints where the page has not drawn yet: before the
    /// first render of a document, and for the frames between `SetIsVisible`
    /// and Chromium's next present. Without it that is WebView2's own white,
    /// which reads as a flash against a dark theme (ADR-F32).
    ///
    /// WebView2 only accepts fully opaque or fully transparent, so the alpha is
    /// forced; a transparent default would show the host window's black class
    /// brush instead of the theme colour.
    pub(crate) fn set_background_color(&self, red: u8, green: u8, blue: u8) {
        let color = COREWEBVIEW2_COLOR {
            A: u8::MAX,
            R: red,
            G: green,
            B: blue,
        };
        match self.inner.controller() {
            Some(controller) => apply_background_color(&controller, color),
            None => self.inner.state.borrow_mut().pending_background_color = Some(color),
        }
    }

    /// Hands the Win32 keyboard focus back to the GPUI window when the page
    /// currently owns it. Windows leaves focus on the child HWND after a click
    /// lands on the parent's client area, so GPUI would stay key-dead until the
    /// next `Esc`; the panel calls this from a window-wide mouse-down listener.
    pub(crate) fn release_keyboard_focus(&self) {
        let page_has_focus = {
            let mut state = self.inner.state.borrow_mut();
            // The keyboard is wanted elsewhere now, so a focus request still
            // waiting for the page to take input again is stale.
            state.pending_focus = false;
            state.page_has_focus
        };
        if page_has_focus {
            self.inner.return_keyboard_focus();
        }
    }

    /// Moves keyboard focus into the page. GPUI keeps its own focus state, so
    /// the caller is responsible for parking the logical focus on the panel.
    pub(crate) fn focus_page(&self) {
        // Windows refuses the focus to a window inside a disabled one, so while
        // the page takes no input the request waits until it does again.
        let input_blocked = self.inner.state.borrow().applied_input_blocked == Some(true);
        if input_blocked {
            self.inner.state.borrow_mut().pending_focus = true;
            return;
        }
        self.inner.move_focus_into_page();
    }
}

// TEMPORARY STUB (win): filled in by the WebView2 automation work. Nothing
// calls these until the browser automation (core) lands.
#[allow(dead_code)]
impl WebView {
    pub(crate) fn execute_script(&self, _script: &str) -> oneshot::Receiver<Result<String>> {
        completed(Err(anyhow!(
            "running scripts is not supported by the WebView2 backend yet"
        )))
    }

    pub(crate) fn capture_png(&self) -> oneshot::Receiver<Result<Vec<u8>>> {
        completed(Err(anyhow!(
            "screenshots are not supported by the WebView2 backend yet"
        )))
    }

    pub(crate) fn dispatch_input(&self, _input: NativeInput) -> oneshot::Receiver<Result<()>> {
        completed(Err(NativeInputUnsupported.into()))
    }

    pub(crate) fn open_developer_tools(&self) {}

    pub(crate) fn is_detached(&self) -> bool {
        false
    }

    pub(crate) fn raise_detached_window(&self) {}
}

impl Drop for WebView {
    fn drop(&mut self) {
        let (controller, children) = {
            let mut state = self.inner.state.borrow_mut();
            state.closed = true;
            (state.controller.take(), state.owned_children.clone())
        };
        // `Close` can fail, and a window left disabled on the host could never
        // be clicked again.
        for child in direct_children(self.inner.host_hwnd, &children) {
            // The return value is "was it disabled before", not an error.
            let _previously_disabled = unsafe { EnableWindow(child, true) };
        }
        if let Some(controller) = controller {
            unsafe { controller.Close() }.log_err();
        }
    }
}

impl Inner {
    /// COM calls can re-enter our own event handlers (which write to `state`),
    /// so every caller clones the interface out and drops the borrow first.
    fn webview(&self) -> Option<ICoreWebView2> {
        self.state.borrow().webview.clone()
    }

    fn controller(&self) -> Option<ICoreWebView2Controller> {
        self.state.borrow().controller.clone()
    }

    /// Runs as the one creation in flight ([`CreationQueue`]). The completion
    /// handler holds `turn` until the controller has attached or failed; it is
    /// handed back when the creation fails before it began, so that the queue
    /// can start the next one at once.
    fn create_controller(
        self: &Rc<Self>,
        environment: ICoreWebView2Environment,
        turn: CreationTurn,
    ) -> Option<CreationTurn> {
        // Shared with the handler, so that a synchronous failure - after which
        // the handler is never called - can hand the turn back right here.
        let turn = Rc::new(Cell::new(Some(turn)));
        let handler = {
            let surface = Rc::downgrade(self);
            let turn = turn.clone();
            CreateCoreWebView2ControllerCompletedHandler::create(Box::new(
                move |result, controller| {
                    let controller =
                        result.and_then(|()| controller.ok_or_else(|| E_POINTER.into()));
                    match (surface.upgrade(), controller) {
                        (Some(surface), Ok(controller)) => surface.attach_controller(controller),
                        (Some(surface), Err(error)) => surface.report_failure(&error),
                        // The surface was dropped while its controller was being
                        // created, so nothing else would ever close it.
                        (None, Ok(controller)) => {
                            unsafe { controller.Close() }.log_err();
                        }
                        (None, Err(error)) => log::info!(
                            "WebView2: creating a controller for a surface that has since closed failed: {error:?}"
                        ),
                    }
                    // Only now, so that the next creation counts this
                    // controller's windows among the host's existing children.
                    drop(turn.take());
                    Ok(())
                },
            ))
        };

        log::info!(
            "WebView2: creating a controller on host hwnd {:#x}",
            self.host_hwnd.0 as isize
        );
        let children_before_controller = enumerate_child_windows(self.host_hwnd);
        self.state.borrow_mut().children_before_controller = children_before_controller;
        if let Err(error) =
            unsafe { environment.CreateCoreWebView2Controller(self.host_hwnd, &handler) }
        {
            self.report_failure(&error);
            return turn.take();
        }
        None
    }

    fn attach_controller(self: &Rc<Self>, controller: ICoreWebView2Controller) {
        let webview = match unsafe { controller.CoreWebView2() } {
            Ok(webview) => webview,
            Err(error) => {
                // Its window would otherwise stay on the host without belonging
                // to any surface.
                unsafe { controller.Close() }.log_err();
                self.report_failure(&error);
                return;
            }
        };

        // WebView2 would otherwise rescale itself from the monitor DPI, which
        // disagrees with GPUI's own scale factor while a window straddles two
        // monitors.
        if let Ok(controller) = controller.cast::<ICoreWebView2Controller3>() {
            unsafe { controller.SetShouldDetectMonitorScaleChanges(false) }.log_err();
        }

        self.register_event_handlers(&controller, &webview);

        let owned_children = {
            let children_after = enumerate_child_windows(self.host_hwnd);
            let appeared = {
                let state = self.state.borrow();
                children_after
                    .into_iter()
                    .filter(|child| !state.children_before_controller.contains(child))
                    .collect::<Vec<_>>()
            };
            windows_of_new_controller(self.host_hwnd.0 as isize, &appeared, parent_window)
        };

        let (pending_url, pending_dark_mode, pending_background_color) = {
            let mut state = self.state.borrow_mut();
            state.controller = Some(controller);
            state.webview = Some(webview.clone());
            state.owned_children = owned_children;
            state.children_before_controller = Vec::new();
            // Force the first apply: the surface was never configured.
            state.applied_bounds = None;
            state.applied_scale = None;
            state.applied_visible = None;
            state.applied_input_blocked = None;
            (
                state.pending_url.take(),
                state.pending_dark_mode.take(),
                state.pending_background_color.take(),
            )
        };

        log::info!(
            "WebView2: controller created on host hwnd {:#x}",
            self.host_hwnd.0 as isize
        );

        if let Some(dark) = pending_dark_mode {
            apply_dark_mode(&webview, dark);
        }
        // Before the first `SetIsVisible(true)`, so the surface never shows
        // WebView2's default white in the frame it appears.
        if let Some(color) = pending_background_color
            && let Some(controller) = self.controller()
        {
            apply_background_color(&controller, color);
        }
        self.apply_viewport(false);
        // Always take the inventory once, even when the surface stays hidden:
        // it is the one log line that separates "WebView2 never attached" from
        // "the child window exists but is never composited".
        self.log_owned_children();
        if let Some(url) = pending_url {
            log::info!("WebView2: navigating to the queued {url}");
            let url = HSTRING::from(url.as_str());
            unsafe { webview.Navigate(PCWSTR(url.as_ptr())) }.log_err();
        }

        self.send(WebViewEvent::Ready);
        self.publish_location(&webview);

        // The calls above can pump messages, so the surface may have been
        // dropped meanwhile, and its `Drop` may have run before the controller
        // was stored, finding nothing to close.
        let closed = self.state.borrow().closed;
        if closed {
            let controller = self.state.borrow_mut().controller.take();
            if let Some(controller) = controller {
                unsafe { controller.Close() }.log_err();
            }
        }
    }

    fn register_event_handlers(
        self: &Rc<Self>,
        controller: &ICoreWebView2Controller,
        webview: &ICoreWebView2,
    ) {
        let mut token = 0i64;

        {
            let events = self.events.clone();
            let handler = SourceChangedEventHandler::create(Box::new(move |webview, _| {
                if let Some(webview) = webview {
                    send_on(&events, WebViewEvent::UrlChanged(read_source(&webview)));
                }
                Ok(())
            }));
            unsafe { webview.add_SourceChanged(&handler, &mut token) }.log_err();
        }

        {
            let events = self.events.clone();
            let handler = DocumentTitleChangedEventHandler::create(Box::new(move |webview, _| {
                if let Some(webview) = webview {
                    send_on(&events, WebViewEvent::TitleChanged(read_title(&webview)));
                }
                Ok(())
            }));
            unsafe { webview.add_DocumentTitleChanged(&handler, &mut token) }.log_err();
        }

        {
            let events = self.events.clone();
            let handler = HistoryChangedEventHandler::create(Box::new(move |webview, _| {
                if let Some(webview) = webview {
                    let (can_go_back, can_go_forward) = read_history(&webview);
                    send_on(
                        &events,
                        WebViewEvent::HistoryChanged {
                            can_go_back,
                            can_go_forward,
                        },
                    );
                }
                Ok(())
            }));
            unsafe { webview.add_HistoryChanged(&handler, &mut token) }.log_err();
        }

        {
            let events = self.events.clone();
            let handler =
                NavigationCompletedEventHandler::create(Box::new(move |webview, args| {
                    let mut succeeded = windows::core::BOOL::from(false);
                    if let Some(args) = args {
                        unsafe { args.IsSuccess(&mut succeeded) }.log_err();
                    }
                    if let Some(webview) = webview {
                        send_on(&events, WebViewEvent::UrlChanged(read_source(&webview)));
                        let (can_go_back, can_go_forward) = read_history(&webview);
                        send_on(
                            &events,
                            WebViewEvent::HistoryChanged {
                                can_go_back,
                                can_go_forward,
                            },
                        );
                    }
                    log::info!(
                        "WebView2: navigation completed (succeeded: {})",
                        succeeded.as_bool()
                    );
                    send_on(
                        &events,
                        WebViewEvent::LoadFinished {
                            succeeded: succeeded.as_bool(),
                        },
                    );
                    Ok(())
                }));
            unsafe { webview.add_NavigationCompleted(&handler, &mut token) }.log_err();
        }

        {
            let events = self.events.clone();
            let handler = NewWindowRequestedEventHandler::create(Box::new(move |_, args| {
                if let Some(args) = args {
                    let mut uri = windows::core::PWSTR::null();
                    if unsafe { args.Uri(&mut uri) }.log_err().is_some() {
                        let uri = take_pwstr(uri);
                        if !uri.is_empty() {
                            send_on(&events, WebViewEvent::OpenInPlace(uri));
                        }
                    }
                    unsafe { args.SetHandled(true) }.log_err();
                }
                Ok(())
            }));
            unsafe { webview.add_NewWindowRequested(&handler, &mut token) }.log_err();
        }

        {
            let events = self.events.clone();
            let host_hwnd = self.host_hwnd;
            let handler = AcceleratorKeyPressedEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()) };
                let mut kind = COREWEBVIEW2_KEY_EVENT_KIND::default();
                let mut virtual_key = 0u32;
                unsafe {
                    args.KeyEventKind(&mut kind)?;
                    args.VirtualKey(&mut virtual_key)?;
                }
                if kind == COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN
                    && virtual_key == u32::from(VK_ESCAPE.0)
                {
                    unsafe { args.SetHandled(true) }.log_err();
                    // gpui_windows never handles WM_SETFOCUS, so the host HWND
                    // has to be told explicitly that it owns the keyboard again
                    // before GPUI's logical focus is restored.
                    unsafe { SetFocus(Some(host_hwnd)) }.log_err();
                    send_on(&events, WebViewEvent::FocusReturned);
                }
                Ok(())
            }));
            unsafe { controller.add_AcceleratorKeyPressed(&handler, &mut token) }.log_err();
        }

        // Weak: the controller holds these handlers, and the surface holds the
        // controller.
        {
            let surface = Rc::downgrade(self);
            let handler = FocusChangedEventHandler::create(Box::new(move |_, _| {
                if let Some(surface) = surface.upgrade() {
                    surface.page_took_focus();
                }
                Ok(())
            }));
            unsafe { controller.add_GotFocus(&handler, &mut token) }.log_err();
        }

        {
            let surface = Rc::downgrade(self);
            let handler = FocusChangedEventHandler::create(Box::new(move |_, _| {
                if let Some(surface) = surface.upgrade() {
                    surface.set_page_focus(false);
                }
                Ok(())
            }));
            unsafe { controller.add_LostFocus(&handler, &mut token) }.log_err();
        }
    }

    fn set_page_focus(&self, focused: bool) {
        self.state.borrow_mut().page_has_focus = focused;
        self.send(WebViewEvent::PageFocusChanged(focused));
    }

    /// WebView2's `GotFocus` arrives asynchronously, so a click that reached
    /// the page just before a modal blocked it can report focus after the
    /// keyboard was already handed back. Reported, it would make the panel pull
    /// gpui's focus away from the modal.
    fn page_took_focus(&self) {
        let input_blocked = self.state.borrow().input_blocked;
        if input_blocked {
            log::info!(
                "WebView2: handing back the focus the page took just before its input was blocked"
            );
            self.return_keyboard_focus();
            return;
        }
        self.set_page_focus(true);
    }

    fn return_keyboard_focus(&self) {
        unsafe { SetFocus(Some(self.host_hwnd)) }.log_err();
        self.state.borrow_mut().page_has_focus = false;
    }

    /// `page_has_focus` follows WebView2's focus events, which arrive
    /// asynchronously and can still read false just after the page took the
    /// focus. The host's and the page's input queues are attached, so
    /// `GetFocus` reports the page's focused window too, although another
    /// process owns it.
    fn page_owns_keyboard_focus(&self, children: &[HWND]) -> bool {
        if self.state.borrow().page_has_focus {
            return true;
        }
        let focused = unsafe { GetFocus() };
        !focused.is_invalid()
            && children
                .iter()
                .any(|child| *child == focused || unsafe { IsChild(*child, focused) }.as_bool())
    }

    fn move_focus_into_page(&self) {
        // `MoveFocus` fires `GotFocus` synchronously, and that handler writes to
        // `state`, so the borrow must be released before the call.
        if let Some(controller) = self.controller() {
            unsafe { controller.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC) }.log_err();
        }
    }

    fn move_pending_focus(&self) {
        let (pending_focus, view_focused) = {
            let mut state = self.state.borrow_mut();
            (std::mem::take(&mut state.pending_focus), state.view_focused)
        };
        // With gpui's focus gone elsewhere meanwhile, the page would pull the
        // keyboard away from wherever the user moved on to.
        if pending_focus && view_focused {
            log::info!("WebView2: moving the focus the page was refused while it took no input");
            self.move_focus_into_page();
        }
    }

    /// `defer_cuts` postpones opening holes in the child window; see
    /// [`WebView::sync_viewport`] for why, and [`Inner::apply_clip`] for what is
    /// deferred. Returns whether a cut is still outstanding.
    fn apply_viewport(&self, defer_cuts: bool) -> bool {
        // Collect the deltas, drop the borrow, then talk to WebView2: the calls
        // below can re-enter handlers that write to `state`.
        let (controller, scale, bounds, input_blocked, visible, hidden_reason) = {
            let state = self.state.borrow();
            let Some(controller) = state.controller.clone() else {
                return false;
            };
            let showing = state.visible && state.applied_visible != Some(true);
            (
                controller,
                (state.applied_scale != Some(state.scale)).then_some(state.scale),
                (state.applied_bounds != Some(state.bounds)).then_some(state.bounds),
                // Also on every show, so that a window whose enabled state was
                // changed behind this surface's back gets its own state back.
                (showing || state.applied_input_blocked != Some(state.input_blocked))
                    .then_some(state.input_blocked),
                (state.applied_visible != Some(state.visible)).then_some(state.visible),
                state.hidden_reason,
            )
        };

        if let Some(scale) = scale
            && let Ok(scaled) = controller.cast::<ICoreWebView2Controller3>()
        {
            if unsafe { scaled.SetRasterizationScale(scale) }
                .log_err()
                .is_some()
            {
                log::info!("WebView2: rasterization scale set to {scale}");
                self.state.borrow_mut().applied_scale = Some(scale);
            }
        }

        if let Some(bounds) = bounds
            && unsafe { controller.SetBounds(bounds) }.log_err().is_some()
        {
            log::info!(
                "WebView2: bounds set to ({}, {})-({}, {}) in physical pixels",
                bounds.left,
                bounds.top,
                bounds.right,
                bounds.bottom
            );
            self.state.borrow_mut().applied_bounds = Some(bounds);
        }

        // Ahead of the visibility, so that a surface shown while a modal is
        // open never takes a click in between.
        if let Some(input_blocked) = input_blocked {
            self.apply_input_blocked(input_blocked);
        }

        if let Some(visible) = visible
            && unsafe { controller.SetIsVisible(visible) }
                .log_err()
                .is_some()
        {
            match hidden_reason {
                Some(reason) => log::info!("WebView2: surface hidden because {reason}"),
                None => log::info!("WebView2: surface shown"),
            }
            // The borrow has to be gone before the Win32 calls below, and
            // holding it across the `if` would deadlock this cell.
            let first_show = {
                let mut state = self.state.borrow_mut();
                state.applied_visible = Some(visible);
                let first_show = visible && !state.nudged_child_windows;
                state.nudged_child_windows |= visible;
                first_show
            };
            if first_show {
                self.log_owned_children();
                // The Win32 calls can pump messages back into our own handlers,
                // which borrow `state`, so hand them an owned copy.
                let owned_children = self.state.borrow().owned_children.clone();
                nudge_child_windows(&owned_children);
            }
        }

        // After the visibility, so that a page shown in this same pass can take
        // it.
        if input_blocked == Some(false) {
            self.move_pending_focus();
        }

        self.apply_clip(defer_cuts)
    }

    /// Disables the controller's own top-level children while `input_blocked`
    /// and enables them again afterwards, for a page that has to stay on
    /// screen under something gpui does not report as an overlay (a workspace
    /// modal and its full-window backdrop).
    ///
    /// Windows' hit test skips a disabled child window (`WindowFromPoint` never
    /// returns one), and whatever WebView2 nests inside it is disabled along
    /// with it, so every click over the page reaches the host window and gpui
    /// dispatches it. A disabled window must not keep the keyboard focus -
    /// keystrokes would go nowhere - so the page hands it back first.
    fn apply_input_blocked(&self, input_blocked: bool) {
        // `EnableWindow` sends messages that can re-enter our own handlers,
        // which borrow `state`.
        let (owned_children, changed) = {
            let state = self.state.borrow();
            (
                state.owned_children.clone(),
                state.applied_input_blocked != Some(input_blocked),
            )
        };
        let children = direct_children(self.host_hwnd, &owned_children).collect::<Vec<_>>();
        if input_blocked && self.page_owns_keyboard_focus(&children) {
            self.return_keyboard_focus();
        }
        for child in &children {
            // The return value is "was it disabled before", not an error.
            let _previously_disabled = unsafe { EnableWindow(*child, !input_blocked) };
            if unsafe { IsWindowEnabled(*child) }.as_bool() == input_blocked {
                log::warn!(
                    "WebView2: child window {:#x} is still {}",
                    child.0 as isize,
                    if input_blocked { "enabled" } else { "disabled" }
                );
            }
        }
        if changed && input_blocked {
            log::info!(
                "WebView2: the surface takes no mouse input; clicks over the page go to the host window"
            );
        } else if changed {
            log::info!("WebView2: the surface takes mouse input");
        }
        self.state.borrow_mut().applied_input_blocked = Some(input_blocked);
    }

    /// Cuts the gpui overlays out of the child windows WebView2 renders into.
    ///
    /// Only the controller's own top-level children are touched: the region is
    /// expressed in each window's own coordinates, and clipping the outermost
    /// one clips everything WebView2 nests inside it.
    ///
    /// With `defer_cuts` the region is left alone whenever the new one has any
    /// hole in it, and `true` is returned so the caller can apply it after the
    /// overlay has been presented; a region without holes is always applied
    /// right away, since giving the surface back is never visible as a gap.
    fn apply_clip(&self, defer_cuts: bool) -> bool {
        let (bounds, holes, children, scale) = {
            let state = self.state.borrow();
            if state.controller.is_none() || !state.visible {
                return false;
            }
            let unchanged = match state.applied_clip.as_ref() {
                None => false,
                // The child windows carry no region and need none, so moving or
                // resizing the surface changes nothing to reapply.
                Some((_, applied)) if applied.is_empty() && state.holes.is_empty() => true,
                Some((bounds, applied)) => {
                    *bounds == state.bounds && applied.as_slice() == state.holes.as_slice()
                }
            };
            if unchanged {
                return false;
            }
            if defer_cuts && !state.holes.is_empty() {
                return true;
            }
            (
                state.bounds,
                state.holes.clone(),
                state.owned_children.clone(),
                state.scale,
            )
        };

        for child in direct_children(self.host_hwnd, &children) {
            clip_child_window(self.host_hwnd, child, &holes, scale, !self.composited);
        }

        self.state.borrow_mut().applied_clip = Some((bounds, holes));
        false
    }

    fn log_owned_children(&self) {
        let children = self.state.borrow().owned_children.clone();
        if children.is_empty() {
            log::warn!(
                "WebView2: the controller added no child window to host hwnd {:#x}; \
                 the browsing surface was never attached",
                self.host_hwnd.0 as isize
            );
            return;
        }
        for child in children {
            log_child_window(HWND(child as *mut c_void));
        }
    }

    fn publish_location(&self, webview: &ICoreWebView2) {
        self.send(WebViewEvent::UrlChanged(read_source(webview)));
        self.send(WebViewEvent::TitleChanged(read_title(webview)));
        let (can_go_back, can_go_forward) = read_history(webview);
        self.send(WebViewEvent::HistoryChanged {
            can_go_back,
            can_go_forward,
        });
    }

    fn report_failure(&self, error: &windows::core::Error) {
        log::error!("WebView2 creation failed: {error:?}");
        self.send(WebViewEvent::Failed(format!("{error}")));
    }

    fn report_failure_message(&self, message: &str) {
        log::error!("WebView2 creation failed: {message}");
        self.send(WebViewEvent::Failed(message.to_string()));
    }

    fn send(&self, event: WebViewEvent) {
        send_on(&self.events, event);
    }
}

/// The receiver is dropped together with the panel, and WebView2 keeps firing
/// callbacks until the controller is closed, so a closed channel is expected.
fn send_on(events: &UnboundedSender<WebViewEvent>, event: WebViewEvent) {
    events.unbounded_send(event).ok();
}

unsafe extern "system" fn collect_child_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // The pointer comes straight from `enumerate_child_windows` below, which
    // keeps the `Vec` alive for the whole call.
    let children = unsafe { &mut *(lparam.0 as *mut Vec<isize>) };
    children.push(hwnd.0 as isize);
    true.into()
}

/// Every descendant HWND of the host window, used to work out which ones a
/// freshly created controller added.
fn enumerate_child_windows(host_hwnd: HWND) -> Vec<isize> {
    let mut children: Vec<isize> = Vec::new();
    // The return value only says whether the callback ran to the end, which it
    // always does here; an empty `children` is the real "nothing found" signal.
    let _enumerated = unsafe {
        EnumChildWindows(
            Some(host_hwnd),
            Some(collect_child_window),
            LPARAM(&mut children as *mut _ as isize),
        )
    };
    children
}

/// The windows among `children` - a subtree as [`enumerate_child_windows`]
/// reports it - that hang directly off the host window. A clipping region or a
/// disabled state set on those covers the windows WebView2 nests inside them.
fn direct_children(host_hwnd: HWND, children: &[isize]) -> impl Iterator<Item = HWND> {
    children
        .iter()
        .map(|child| HWND(*child as *mut c_void))
        .filter(move |child| unsafe { GetParent(*child) }.ok() == Some(host_hwnd))
}

/// The windows among `appeared` - the host's descendants that were not there
/// before this controller's creation started - that belong to it: its new
/// direct children of the host and whatever is nested inside them. Another
/// surface can nest a new window inside its own, older one meanwhile (its
/// render widget after its first navigation); that one appears too, but hangs
/// off a window that is not new. `parent_of` is [`parent_window`], replaceable
/// for tests.
fn windows_of_new_controller(
    host_hwnd: isize,
    appeared: &[isize],
    parent_of: impl Fn(isize) -> Option<isize>,
) -> Vec<isize> {
    let new_direct_children = appeared
        .iter()
        .copied()
        .filter(|window| parent_of(*window) == Some(host_hwnd))
        .collect::<Vec<_>>();
    appeared
        .iter()
        .copied()
        .filter(|window| {
            std::iter::successors(Some(*window), |current| {
                parent_of(*current).filter(|parent| *parent != host_hwnd)
            })
            .any(|ancestor| new_direct_children.contains(&ancestor))
        })
        .collect()
}

fn parent_window(window: isize) -> Option<isize> {
    unsafe { GetParent(HWND(window as *mut c_void)) }
        .ok()
        .map(|parent| parent.0 as isize)
}

/// Diagnostics: when the surface is configured correctly but the user still
/// sees nothing, this log tells whether the child HWND exists and where Windows
/// thinks it is - which separates "WebView2 never attached" from "the child
/// window is fine but the compositor never shows it".
fn log_child_window(child: HWND) {
    let mut class = [0u16; 128];
    let written = unsafe { GetClassNameW(child, &mut class) };
    let class = String::from_utf16_lossy(&class[..written.max(0) as usize]);
    let mut rect = RECT::default();
    unsafe { GetWindowRect(child, &mut rect) }.log_err();
    log::info!(
        "WebView2: child window {:#x} class {class} visible {} at ({}, {})-({}, {}) in screen pixels",
        child.0 as isize,
        unsafe { IsWindowVisible(child) }.as_bool(),
        rect.left,
        rect.top,
        rect.right,
        rect.bottom
    );
}

/// Belt and braces on the first show: WebView2 shows its own child, but a
/// window parented while the host was hidden has been seen to stay hidden.
///
/// Only this controller's own children may be nudged. Several surfaces share
/// the host window, and forcing a sibling we deliberately hid back into view
/// would leave an invisible window eating mouse input over the editor.
fn nudge_child_windows(children: &[isize]) {
    for child in children {
        let child = HWND(*child as *mut c_void);
        // The return value is "was it visible before", not an error.
        let _previously_visible = unsafe { ShowWindow(child, SW_SHOWNA) };
        unsafe {
            SetWindowPos(
                child,
                Some(HWND_TOP),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )
        }
        .log_err();
    }
}

/// Where the browsing surface goes and which parts of it gpui draws over, in
/// physical pixels relative to the host window's client origin.
///
/// Kept pure so that the DPI rounding and the intersections can be tested
/// without a window; [`WebView::sync_viewport`] is the only caller.
fn viewport_geometry(viewport: &Viewport) -> (RECT, Vec<RECT>) {
    let to_device = |value: f32| (value * viewport.scale_factor).round() as i32;
    let left = to_device(viewport.origin_x);
    let top = to_device(viewport.origin_y);
    let bounds = RECT {
        left,
        top,
        right: left + to_device(viewport.width).max(0),
        bottom: top + to_device(viewport.height).max(0),
    };

    let sliver_depth = to_device(EDGE_SLIVER_DEPTH);
    let holes = viewport
        .overlays
        .iter()
        .filter_map(|overlay| {
            let overlay = RECT {
                left: to_device(overlay.left),
                top: to_device(overlay.top),
                right: to_device(overlay.right),
                bottom: to_device(overlay.bottom),
            };
            let hole = intersect_rects(bounds, overlay)?;
            (!is_edge_sliver(bounds, overlay, hole, sliver_depth)).then_some(hole)
        })
        .collect();

    (bounds, holes)
}

/// Whether `hole`, the part of `overlay` over `bounds`, is a band at most
/// `depth` deep along one edge of `bounds` that `overlay` carries on past - the
/// shape of a neighbouring dock's resize handle; see [`EDGE_SLIVER_DEPTH`].
fn is_edge_sliver(bounds: RECT, overlay: RECT, hole: RECT, depth: i32) -> bool {
    (overlay.left < bounds.left && hole.right - bounds.left <= depth)
        || (overlay.right > bounds.right && bounds.right - hole.left <= depth)
        || (overlay.top < bounds.top && hole.bottom - bounds.top <= depth)
        || (overlay.bottom > bounds.bottom && bounds.bottom - hole.top <= depth)
}

/// Radius, in physical pixels, of the rounded corners a hole is cut with.
///
/// Clamped to half the shorter side: a larger radius would round the hole into
/// an ellipse that no longer covers the overlay it was cut for. A hole thinner
/// than a couple of pixels gets square corners instead, since rounding one is
/// all corner and no hole.
fn hole_corner_radius(hole: RECT, scale: f64) -> i32 {
    let half_width = (hole.right - hole.left) / 2;
    let half_height = (hole.bottom - hole.top) / 2;
    let radius = (OVERLAY_CORNER_RADIUS * scale).round() as i32;
    radius.min(half_width).min(half_height).max(0)
}

/// `None` when the two rectangles do not overlap, so that degenerate
/// rectangles never reach the region arithmetic.
fn intersect_rects(one: RECT, other: RECT) -> Option<RECT> {
    let intersection = RECT {
        left: one.left.max(other.left),
        top: one.top.max(other.top),
        right: one.right.min(other.right),
        bottom: one.bottom.min(other.bottom),
    };
    (intersection.left < intersection.right && intersection.top < intersection.bottom)
        .then_some(intersection)
}

/// Gives `child` a window region that is its own rectangle minus `holes`
/// (host client coordinates, physical pixels), or removes the region when
/// there is nothing to cut away. `scale` is the window's scale factor, which
/// turns [`OVERLAY_CORNER_RADIUS`] into physical pixels.
///
/// Windows keeps a window region across moves and resizes without adjusting it,
/// so the region is rebuilt from the child's current rectangle whenever either
/// the holes or the surface bounds change.
///
/// `redraw` asks Windows to erase and repaint the child. Under
/// DirectComposition that is pure cost - DWM recomposites the child's visual
/// from the new region on its own - and it is visible as a dark flash across
/// the page every time a tooltip appears or goes away. Without
/// DirectComposition the child owns those pixels, so the repaint is what makes
/// the change show up.
fn clip_child_window(host_hwnd: HWND, child: HWND, holes: &[RECT], scale: f64, redraw: bool) {
    if holes.is_empty() {
        // Passing no region restores the default (the whole window).
        if unsafe { SetWindowRgn(child, None, redraw) } == 0 {
            log::warn!(
                "WebView2: could not clear the clipping region of child window {:#x}",
                child.0 as isize
            );
        }
        return;
    }

    let mut child_rect = RECT::default();
    if unsafe { GetWindowRect(child, &mut child_rect) }
        .log_err()
        .is_none()
    {
        return;
    }
    let mut origin = POINT {
        x: child_rect.left,
        y: child_rect.top,
    };
    if !unsafe { ScreenToClient(host_hwnd, &mut origin) }.as_bool() {
        log::warn!(
            "WebView2: could not map child window {:#x} into the host's client area",
            child.0 as isize
        );
        return;
    }

    let region = unsafe {
        CreateRectRgn(
            0,
            0,
            child_rect.right - child_rect.left,
            child_rect.bottom - child_rect.top,
        )
    };
    if region.is_invalid() {
        log::warn!("WebView2: could not create a clipping region for the browsing surface");
        return;
    }

    for hole in holes {
        let radius = hole_corner_radius(*hole, scale);
        let (left, top, right, bottom) = (
            hole.left - origin.x,
            hole.top - origin.y,
            hole.right - origin.x,
            hole.bottom - origin.y,
        );
        let hole = unsafe {
            if radius > 0 {
                // `CreateRoundRectRgn` takes the size of the ellipse the corners
                // are cut from, which is twice the radius.
                CreateRoundRectRgn(left, top, right, bottom, radius * 2, radius * 2)
            } else {
                CreateRectRgn(left, top, right, bottom)
            }
        };
        if hole.is_invalid() {
            log::warn!("WebView2: could not create a region for a gpui overlay");
            continue;
        }
        if unsafe { CombineRgn(Some(region), Some(region), Some(hole), RGN_DIFF) } == RGN_ERROR {
            log::warn!("WebView2: could not subtract a gpui overlay from the clipping region");
        }
        unsafe { DeleteObject(hole.into()) }.ok().log_err();
    }

    // On success the window owns the region and must not be deleted here; on
    // failure it is still ours.
    if unsafe { SetWindowRgn(child, Some(region), redraw) } == 0 {
        log::warn!(
            "WebView2: could not clip child window {:#x} to the gpui overlays",
            child.0 as isize
        );
        unsafe { DeleteObject(region.into()) }.ok().log_err();
    }
}

/// `ICoreWebView2Controller2` ships with every runtime we support, so a failed
/// cast means an unexpectedly old one; the page then keeps WebView2's white
/// default, which is cosmetic.
fn apply_background_color(controller: &ICoreWebView2Controller, color: COREWEBVIEW2_COLOR) {
    let Ok(controller) = controller.cast::<ICoreWebView2Controller2>() else {
        log::warn!("WebView2: this runtime cannot set a default background colour");
        return;
    };
    if unsafe { controller.SetDefaultBackgroundColor(color) }
        .log_err()
        .is_some()
    {
        log::info!(
            "WebView2: default background colour set to #{:02x}{:02x}{:02x}",
            color.R,
            color.G,
            color.B
        );
    }
}

fn apply_dark_mode(webview: &ICoreWebView2, dark: bool) {
    let scheme: COREWEBVIEW2_PREFERRED_COLOR_SCHEME = if dark {
        COREWEBVIEW2_PREFERRED_COLOR_SCHEME_DARK
    } else {
        COREWEBVIEW2_PREFERRED_COLOR_SCHEME_LIGHT
    };
    if let Ok(webview) = webview.cast::<ICoreWebView2_13>()
        && let Some(profile) = unsafe { webview.Profile() }.log_err()
    {
        unsafe { profile.SetPreferredColorScheme(scheme) }.log_err();
    }
}

fn read_source(webview: &ICoreWebView2) -> String {
    let mut source = windows::core::PWSTR::null();
    match unsafe { webview.Source(&mut source) }.log_err() {
        Some(()) => take_pwstr(source),
        None => String::new(),
    }
}

fn read_title(webview: &ICoreWebView2) -> String {
    let mut title = windows::core::PWSTR::null();
    match unsafe { webview.DocumentTitle(&mut title) }.log_err() {
        Some(()) => take_pwstr(title),
        None => String::new(),
    }
}

fn read_history(webview: &ICoreWebView2) -> (bool, bool) {
    let mut can_go_back = windows::core::BOOL::from(false);
    let mut can_go_forward = windows::core::BOOL::from(false);
    unsafe { webview.CanGoBack(&mut can_go_back) }.log_err();
    unsafe { webview.CanGoForward(&mut can_go_forward) }.log_err();
    (can_go_back.as_bool(), can_go_forward.as_bool())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewport(scale_factor: f32, overlays: Vec<OverlayRect>) -> Viewport {
        Viewport {
            origin_x: 100.,
            origin_y: 50.,
            width: 400.,
            height: 300.,
            scale_factor,
            hidden_reason: None,
            input_blocked: false,
            overlays,
        }
    }

    fn overlay(left: f32, top: f32, right: f32, bottom: f32) -> OverlayRect {
        OverlayRect {
            left,
            top,
            right,
            bottom,
        }
    }

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn bounds_and_holes_are_scaled_to_physical_pixels() {
        let (bounds, holes) =
            viewport_geometry(&viewport(1.5, vec![overlay(120., 60., 200., 90.)]));

        assert_eq!(bounds, rect(150, 75, 750, 525));
        assert_eq!(holes, vec![rect(180, 90, 300, 135)]);
    }

    #[test]
    fn overlays_beside_the_surface_are_not_holes() {
        let (_, holes) = viewport_geometry(&viewport(
            1.,
            vec![
                // Entirely above the surface.
                overlay(120., 0., 200., 40.),
                // Entirely to its right.
                overlay(520., 60., 600., 90.),
                // Sharing the left edge, so still zero area.
                overlay(20., 60., 100., 90.),
            ],
        ));

        assert!(holes.is_empty(), "{holes:?}");
    }

    #[test]
    fn an_overlapping_overlay_is_clipped_to_the_surface() {
        // A tooltip that starts above and to the left of the surface.
        let (_, holes) = viewport_geometry(&viewport(1., vec![overlay(40., 20., 160., 120.)]));

        assert_eq!(holes, vec![rect(100, 50, 160, 120)]);
    }

    #[test]
    fn every_overlapping_overlay_becomes_its_own_hole() {
        // Two overlays far apart must stay separate: their bounding box would
        // cover most of the surface.
        let (_, holes) = viewport_geometry(&viewport(
            1.,
            vec![
                overlay(110., 60., 150., 80.),
                overlay(2000., 2000., 2100., 2100.),
                overlay(400., 300., 460., 330.),
            ],
        ));

        assert_eq!(
            holes,
            vec![rect(110, 60, 150, 80), rect(400, 300, 460, 330)]
        );
    }

    #[test]
    fn an_overlay_covering_the_whole_surface_leaves_nothing() {
        let (bounds, holes) = viewport_geometry(&viewport(1., vec![overlay(0., 0., 4000., 4000.)]));

        assert_eq!(holes, vec![bounds]);
    }

    #[test]
    fn a_dock_resize_handle_straddling_an_edge_is_not_a_hole() {
        // The surface spans (100, 50)-(500, 350). A handle is 6px across and
        // is placed from inside the dock's 1px border, so it reaches 3px into
        // the dock's own content and 2px into its neighbour, and it runs past
        // the surface along the whole edge.
        let handles = [
            // A panel in the right dock, whose border is the column x = 99.
            overlay(97., 0., 103., 2000.),
            // A panel in the left dock, whose border is the column x = 500.
            overlay(497., 0., 503., 2000.),
            // A panel in the bottom dock, whose border is the row y = 49.
            overlay(0., 47., 2000., 53.),
            // A centre tab beside the left dock, whose border is the column x = 99.
            overlay(96., 0., 102., 2000.),
            // A centre tab beside the right dock, whose border is the column x = 500.
            overlay(498., 0., 504., 2000.),
            // A centre tab above the bottom dock, whose border is the row y = 350.
            overlay(0., 348., 2000., 354.),
        ];
        for handle in handles {
            let (_, holes) = viewport_geometry(&viewport(1., vec![handle]));
            assert!(holes.is_empty(), "{handle:?} was cut out as {holes:?}");
        }
    }

    #[test]
    fn a_thin_overlay_inside_the_surface_is_still_a_hole() {
        // One flush with the left edge and one just below the top edge; neither
        // carries on past the edge it runs along.
        let (_, holes) = viewport_geometry(&viewport(
            1.,
            vec![
                overlay(100., 60., 103., 200.),
                overlay(150., 52., 300., 55.),
            ],
        ));

        assert_eq!(holes, vec![rect(100, 60, 103, 200), rect(150, 52, 300, 55)]);
    }

    #[test]
    fn a_menu_crossing_an_edge_deep_into_the_surface_is_still_a_hole() {
        // A context menu opened beside the left dock that runs well into the
        // page, and one hanging off the bottom of the surface.
        let (_, holes) = viewport_geometry(&viewport(
            1.,
            vec![
                overlay(40., 100., 260., 300.),
                overlay(300., 280., 460., 420.),
            ],
        ));

        assert_eq!(
            holes,
            vec![rect(100, 100, 260, 300), rect(300, 280, 460, 350)]
        );
    }

    #[test]
    fn a_sliver_at_a_fractional_scale_is_not_a_hole() {
        // At 1.5 the surface is (150, 75)-(750, 525) and a sliver may be 6
        // physical pixels deep. The handle ends 2.6 logical pixels into the
        // surface, which rounds to 154.
        let (_, holes) = viewport_geometry(&viewport(1.5, vec![overlay(96.6, 0., 102.6, 2000.)]));
        assert!(holes.is_empty(), "{holes:?}");

        // Five logical pixels in rounds to 158, 8 physical pixels deep.
        let (_, holes) = viewport_geometry(&viewport(1.5, vec![overlay(96.6, 0., 105., 2000.)]));
        assert_eq!(holes, vec![rect(150, 75, 158, 525)]);
    }

    #[test]
    fn a_three_pixel_deep_handle_is_not_a_hole_at_common_scale_factors() {
        // The dock side of a handle, the deeper one, on each edge of the surface.
        let handles = [
            overlay(97., 0., 103., 2000.),
            overlay(497., 0., 503., 2000.),
            overlay(0., 47., 2000., 53.),
            overlay(0., 347., 2000., 353.),
        ];
        for scale_factor in [1., 1.25, 1.5, 1.75, 2.] {
            for handle in handles {
                let (_, holes) = viewport_geometry(&viewport(scale_factor, vec![handle]));
                assert!(
                    holes.is_empty(),
                    "{handle:?} at scale {scale_factor} was cut out as {holes:?}"
                );
            }
        }
    }

    #[test]
    fn a_large_menu_that_only_grazes_an_edge_is_not_cut_out() {
        // Deliberate: a menu that overlaps the surface by no more than a handle
        // would is left alone, so those few pixels of it pass clicks through to
        // the page instead of taking them.
        let menus = [
            overlay(20., 100., 101., 400.),
            overlay(20., 100., 104., 300.),
            overlay(200., -150., 400., 54.),
            overlay(300., 347., 700., 600.),
        ];
        for menu in menus {
            let (_, holes) = viewport_geometry(&viewport(1., vec![menu]));
            assert!(holes.is_empty(), "{menu:?} was cut out as {holes:?}");
        }

        // One pixel deeper, and it is a hole again.
        let (_, holes) = viewport_geometry(&viewport(1., vec![overlay(20., 100., 105., 300.)]));
        assert_eq!(holes, vec![rect(100, 100, 105, 300)]);
    }

    /// Stand-ins for controller creations: each records that it started, then
    /// either keeps its turn in `kept` (a creation still running) or hands it
    /// straight back.
    #[derive(Default)]
    struct FakeCreations {
        started: Rc<RefCell<Vec<&'static str>>>,
        kept: Rc<RefCell<VecDeque<CreationTurn>>>,
    }

    impl FakeCreations {
        fn keeping(&self, name: &'static str) -> CreationStarter {
            let started = self.started.clone();
            let kept = self.kept.clone();
            Box::new(move |turn| {
                started.borrow_mut().push(name);
                kept.borrow_mut().push_back(turn);
                None
            })
        }

        fn handing_back(&self, name: &'static str) -> CreationStarter {
            let started = self.started.clone();
            Box::new(move |turn| {
                started.borrow_mut().push(name);
                Some(turn)
            })
        }

        /// Ends the oldest creation still holding its turn.
        fn end_oldest(&self) {
            let turn = self.kept.borrow_mut().pop_front();
            drop(turn);
        }

        fn end_all(&self) {
            while !self.kept.borrow().is_empty() {
                self.end_oldest();
            }
        }

        fn started(&self) -> Vec<&'static str> {
            self.started.borrow().clone()
        }
    }

    #[test]
    fn controller_creations_start_one_at_a_time_in_request_order() {
        let queue = Rc::new(CreationQueue::default());
        let creations = FakeCreations::default();
        let now = Instant::now();
        queue.request(1, creations.keeping("panel"), now);
        // Painted in the same frame: they wait until the panel's controller has
        // attached or failed.
        queue.request(2, creations.keeping("centre tab"), now);
        queue.request(3, creations.keeping("split tab"), now);
        assert_eq!(creations.started(), ["panel"]);

        creations.end_oldest();
        assert_eq!(creations.started(), ["panel", "centre tab"]);
        creations.end_oldest();
        assert_eq!(creations.started(), ["panel", "centre tab", "split tab"]);
        creations.end_oldest();

        // Idle again, so the next request does not wait.
        queue.request(4, creations.keeping("late tab"), now);
        assert_eq!(creations.started().last(), Some(&"late tab"));
        creations.end_all();
    }

    #[test]
    fn requests_handed_straight_back_are_skipped_without_stalling() {
        let queue = Rc::new(CreationQueue::default());
        let creations = FakeCreations::default();
        let now = Instant::now();
        queue.request(1, creations.keeping("panel"), now);
        // A tab closed while it waited, and one whose creation failed at once.
        queue.request(2, creations.handing_back("closed tab"), now);
        queue.request(3, creations.handing_back("failed tab"), now);
        queue.request(4, creations.keeping("centre tab"), now);

        creations.end_oldest();
        assert_eq!(
            creations.started(),
            ["panel", "closed tab", "failed tab", "centre tab"]
        );

        // One handed back with nothing behind it leaves the queue idle.
        creations.end_oldest();
        queue.request(5, creations.handing_back("another closed tab"), now);
        queue.request(6, creations.keeping("late tab"), now);
        assert_eq!(creations.started().last(), Some(&"late tab"));
        creations.end_all();
    }

    #[test]
    fn a_creation_stuck_past_the_timeout_is_given_up_on() {
        let queue = Rc::new(CreationQueue::default());
        let creations = FakeCreations::default();
        let start = Instant::now();
        queue.request(1, creations.keeping("stuck tab"), start);
        queue.request(
            2,
            creations.keeping("centre tab"),
            start + Duration::from_secs(29),
        );
        assert_eq!(creations.started(), ["stuck tab"]);

        // The next request after the timeout gives up on the stuck creation
        // and starts the oldest waiting one; the new one queues behind it.
        queue.request(
            3,
            creations.keeping("late tab"),
            start + Duration::from_secs(31),
        );
        assert_eq!(creations.started(), ["stuck tab", "centre tab"]);
        creations.end_all();
    }

    #[test]
    fn a_turn_from_a_creation_given_up_on_does_not_advance_the_queue() {
        let queue = Rc::new(CreationQueue::default());
        let creations = FakeCreations::default();
        let start = Instant::now();
        let after_the_timeout = start + CREATION_TIMEOUT + Duration::from_secs(1);
        queue.request(1, creations.keeping("stuck tab"), start);
        queue.request(2, creations.keeping("centre tab"), after_the_timeout);
        queue.request(
            3,
            creations.keeping("late tab"),
            after_the_timeout + Duration::from_secs(1),
        );
        assert_eq!(creations.started(), ["stuck tab", "centre tab"]);

        // The stuck creation ends after all, while the centre tab's still
        // runs: the late tab keeps waiting.
        creations.end_oldest();
        assert_eq!(creations.started(), ["stuck tab", "centre tab"]);
        creations.end_oldest();
        assert_eq!(creations.started(), ["stuck tab", "centre tab", "late tab"]);
        creations.end_all();
    }

    #[test]
    fn only_windows_inside_the_new_controllers_own_are_claimed() {
        // On host 1, this controller added 10, with 11 inside it and 12 inside
        // that; meanwhile a neighbour nested 21 inside its older 20.
        let parent_of = |window: isize| match window {
            10 | 20 => Some(1),
            11 => Some(10),
            12 => Some(11),
            21 => Some(20),
            _ => None,
        };
        let owned = windows_of_new_controller(1, &[10, 11, 12, 21], parent_of);
        assert_eq!(owned, [10, 11, 12]);
    }

    #[test]
    fn hole_corners_are_rounded_like_the_overlay_that_is_cut_out() {
        assert_eq!(hole_corner_radius(rect(0, 0, 200, 100), 1.), 8);
        assert_eq!(hole_corner_radius(rect(0, 0, 200, 100), 1.5), 12);
        assert_eq!(hole_corner_radius(rect(0, 0, 200, 100), 2.), 16);
    }

    #[test]
    fn a_thin_hole_is_not_rounded_away() {
        // A tooltip clipped down to a sliver by the edge of the surface: half of
        // the shorter side is all the rounding the hole can take.
        assert_eq!(hole_corner_radius(rect(0, 0, 200, 6), 1.), 3);
        assert_eq!(hole_corner_radius(rect(0, 0, 1, 100), 1.), 0);
        assert_eq!(hole_corner_radius(rect(0, 0, 0, 0), 2.), 0);
    }

    #[test]
    fn intersecting_rectangles_that_only_touch_have_no_intersection() {
        assert_eq!(
            intersect_rects(rect(0, 0, 10, 10), rect(5, 5, 20, 20)),
            Some(rect(5, 5, 10, 10))
        );
        assert_eq!(
            intersect_rects(rect(0, 0, 10, 10), rect(10, 0, 20, 10)),
            None
        );
        assert_eq!(
            intersect_rects(rect(0, 0, 10, 10), rect(0, 10, 10, 20)),
            None
        );
    }
}
