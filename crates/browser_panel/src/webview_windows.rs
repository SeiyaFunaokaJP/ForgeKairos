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
    cell::RefCell,
    ffi::c_void,
    path::{Path, PathBuf},
    rc::Rc,
};

use anyhow::{Context as _, Result};
use futures::channel::mpsc::UnboundedSender;
use util::ResultExt as _;
use webview2_com::{
    AcceleratorKeyPressedEventHandler, CoreWebView2EnvironmentOptions,
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    DocumentTitleChangedEventHandler, FocusChangedEventHandler, HistoryChangedEventHandler,
    Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_KEY_EVENT_KIND, COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN,
        COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC, COREWEBVIEW2_PREFERRED_COLOR_SCHEME,
        COREWEBVIEW2_PREFERRED_COLOR_SCHEME_DARK, COREWEBVIEW2_PREFERRED_COLOR_SCHEME_LIGHT,
        CreateCoreWebView2EnvironmentWithOptions, ICoreWebView2, ICoreWebView2_13,
        ICoreWebView2Controller, ICoreWebView2Controller3, ICoreWebView2Environment,
        ICoreWebView2EnvironmentOptions,
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
            Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE},
            WindowsAndMessaging::{
                EnumChildWindows, GWL_EXSTYLE, GetClassNameW, GetParent, GetWindowLongPtrW,
                GetWindowRect, HWND_TOP, IsIconic, IsWindowVisible, SW_SHOWNA, SWP_NOACTIVATE,
                SWP_NOMOVE, SWP_NOSIZE, SetWindowPos, ShowWindow, WS_EX_NOREDIRECTIONBITMAP,
            },
        },
    },
    core::{BOOL, HSTRING, Interface as _, PCWSTR},
};

/// Everything the WebView2 callbacks need to tell the GPUI side about.
pub(crate) enum WebViewEvent {
    Ready,
    /// Creation failed; the payload is the Japanese message to render in place
    /// of the browsing surface.
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
    /// The user asked (with `Esc`) to hand keyboard focus back to the editor.
    FocusReturned,
    /// Whether the page currently owns the Win32 keyboard focus. GPUI's own
    /// focus state cannot see this, so the panel renders a hint from it.
    PageFocusChanged(bool),
    /// A `target="_blank"` navigation; the panel hosts a single view, so it is
    /// re-targeted at the current view instead of spawning a browser window.
    OpenInPlace(String),
}

/// Where the browsing surface should sit, in GPUI's logical pixels relative to
/// the window's client origin, plus why it is (not) shown. Passed as one value
/// because the panel recomputes all of it on every paint.
pub(crate) struct Viewport {
    pub(crate) origin_x: f32,
    pub(crate) origin_y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) scale_factor: f32,
    /// `None` when the surface should be shown; otherwise why it is hidden.
    /// Only used for logging, so the reason has to be a `'static` label.
    pub(crate) hidden_reason: Option<&'static str>,
    /// What gpui paints on top of the element tree this frame (tooltips,
    /// context menus, popovers), in the same logical pixels and origin as the
    /// viewport itself. The child HWND is composited above everything gpui
    /// draws, so whatever of this lands on the surface has to be cut out of the
    /// child window; see [`WebView::sync_viewport`].
    pub(crate) overlays: Vec<OverlayRect>,
}

/// Corner radius of the holes cut for gpui's overlays, in logical pixels.
///
/// Tooltips, context menus and popovers are all drawn by `ui::elevation_2`,
/// which rounds its corners by `rounded_lg` (0.5rem, 8px at the default rem
/// size). A square hole would expose what gpui painted *under* the overlay -
/// the panel's own dark background - in the four corners.
const OVERLAY_CORNER_RADIUS: f64 = 8.;

/// A gpui overlay rectangle in logical pixels, relative to the host window's
/// client origin - the same space [`Viewport`] uses.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct OverlayRect {
    pub(crate) left: f32,
    pub(crate) top: f32,
    pub(crate) right: f32,
    pub(crate) bottom: f32,
}

/// Whether the host window was created with `WS_EX_NOREDIRECTIONBITMAP`, which
/// `gpui_windows` does unless `GPUI_DISABLE_DIRECT_COMPOSITION` is set.
///
/// Such a window has no DWM redirection surface, and child HWNDs have nowhere
/// to draw; on top of that gpui binds its composition tree with
/// `CreateTargetForHwnd(hwnd, topmost = true)`, which composites gpui's own
/// (opaque) swap chain *above* every child window. Either one on its own is
/// enough to make a windowed-hosted WebView2 invisible, so the panel warns
/// instead of leaving the user with a blank surface.
pub(crate) fn host_uses_direct_composition(host_hwnd: isize) -> bool {
    let hwnd = HWND(host_hwnd as *mut c_void);
    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    (ex_style as u32) & WS_EX_NOREDIRECTIONBITMAP.0 != 0
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

pub(crate) struct WebView {
    inner: Rc<Inner>,
}

struct Inner {
    host_hwnd: HWND,
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
    page_has_focus: bool,
    hidden_reason: Option<&'static str>,
    /// The render widget is nudged to the top of the sibling order once, the
    /// first time the surface is made visible; it is a workaround, not
    /// something to repeat per frame.
    nudged_child_windows: bool,
    /// Child HWNDs the host window had before this controller was created, and
    /// the ones this controller added. Several surfaces share the host window
    /// now, so the nudge must never touch a sibling that is deliberately
    /// hidden - showing it again would leave an invisible window eating clicks.
    children_before_controller: Vec<isize>,
    owned_children: Vec<isize>,
}

impl WebView {
    /// Starts asynchronous creation of a WebView2 environment and controller.
    /// Returning `Ok` only means the request was accepted; readiness (or
    /// failure) arrives as a [`WebViewEvent`].
    pub(crate) fn new(
        host_hwnd: isize,
        user_data_folder: &Path,
        proxy_endpoint: Option<&str>,
        events: UnboundedSender<WebViewEvent>,
    ) -> Result<Self> {
        let inner = Rc::new(Inner {
            host_hwnd: HWND(host_hwnd as *mut c_void),
            events,
            state: RefCell::new(State {
                scale: 1.0,
                hidden_reason: Some("the panel has not been painted yet"),
                ..Default::default()
            }),
        });

        log::info!(
            "WebView2: requesting a surface (host hwnd {host_hwnd:#x}, direct composition on the host window: {})",
            host_uses_direct_composition(host_hwnd),
        );

        let waiter = {
            let inner = inner.clone();
            Box::new(
                move |outcome: Result<ICoreWebView2Environment, String>| match outcome {
                    Ok(environment) => inner.create_controller(environment),
                    Err(message) => inner.report_failure_message(&message),
                },
            ) as EnvironmentWaiter
        };
        with_shared_environment(user_data_folder, proxy_endpoint, waiter)?;

        Ok(Self { inner })
    }

    /// Places the browsing surface under the panel's content area.
    /// `ICoreWebView2Controller::SetBounds` wants client-area coordinates of the
    /// parent HWND in physical pixels, so GPUI's logical bounds are multiplied
    /// by the window's scale factor here (and nowhere else: WebView2 is told not
    /// to follow the monitor DPI on its own).
    ///
    /// The overlays gpui reported for this frame are turned into holes in the
    /// child window's region, because the compositor puts the child in front of
    /// gpui's swap chain and a tooltip drawn over the surface would otherwise be
    /// invisible (ADR-F19).
    ///
    /// Returns `true` when cutting those holes was held back: this runs while
    /// gpui paints, so the frame that draws the overlay has not been presented
    /// yet, and opening the hole now uncovers the host window (whose class brush
    /// is black) for one frame. The caller applies it from a next-frame callback
    /// with [`WebView::apply_deferred_clip`] instead. Removing holes is never
    /// deferred - it only ever covers more of the surface back up.
    #[must_use]
    pub(crate) fn sync_viewport(&self, viewport: Viewport) -> bool {
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

    /// Hands the Win32 keyboard focus back to the GPUI window when the page
    /// currently owns it. Windows leaves focus on the child HWND after a click
    /// lands on the parent's client area, so GPUI would stay key-dead until the
    /// next `Esc`; the panel calls this from a window-wide mouse-down listener.
    pub(crate) fn release_keyboard_focus(&self) {
        if !self.inner.state.borrow().page_has_focus {
            return;
        }
        unsafe { SetFocus(Some(self.inner.host_hwnd)) }.log_err();
        self.inner.state.borrow_mut().page_has_focus = false;
    }

    /// Moves keyboard focus into the page. GPUI keeps its own focus state, so
    /// the caller is responsible for parking the logical focus on the panel.
    pub(crate) fn focus_page(&self) {
        // `MoveFocus` fires `GotFocus` synchronously, and that handler writes to
        // `state`, so the borrow must be released before the call.
        if let Some(controller) = self.inner.controller() {
            unsafe { controller.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC) }.log_err();
        }
    }
}

impl Drop for WebView {
    fn drop(&mut self) {
        let controller = self.inner.state.borrow_mut().controller.take();
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

    fn create_controller(self: &Rc<Self>, environment: ICoreWebView2Environment) {
        let handler = {
            let this = self.clone();
            CreateCoreWebView2ControllerCompletedHandler::create(Box::new(
                move |result, controller| {
                    match result.and_then(|()| controller.ok_or_else(|| E_POINTER.into())) {
                        Ok(controller) => this.attach_controller(controller),
                        Err(error) => this.report_failure(&error),
                    }
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
        }
    }

    fn attach_controller(self: &Rc<Self>, controller: ICoreWebView2Controller) {
        let webview = match unsafe { controller.CoreWebView2() } {
            Ok(webview) => webview,
            Err(error) => {
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
            let state = self.state.borrow();
            children_after
                .into_iter()
                .filter(|child| !state.children_before_controller.contains(child))
                .collect::<Vec<_>>()
        };

        let (pending_url, pending_dark_mode) = {
            let mut state = self.state.borrow_mut();
            state.controller = Some(controller);
            state.webview = Some(webview.clone());
            state.owned_children = owned_children;
            state.children_before_controller = Vec::new();
            // Force the first apply: the surface was never configured.
            state.applied_bounds = None;
            state.applied_scale = None;
            state.applied_visible = None;
            (state.pending_url.take(), state.pending_dark_mode.take())
        };

        log::info!(
            "WebView2: controller created on host hwnd {:#x}",
            self.host_hwnd.0 as isize
        );

        if let Some(dark) = pending_dark_mode {
            apply_dark_mode(&webview, dark);
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

        {
            let this = self.clone();
            let handler = FocusChangedEventHandler::create(Box::new(move |_, _| {
                this.set_page_focus(true);
                Ok(())
            }));
            unsafe { controller.add_GotFocus(&handler, &mut token) }.log_err();
        }

        {
            let this = self.clone();
            let handler = FocusChangedEventHandler::create(Box::new(move |_, _| {
                this.set_page_focus(false);
                Ok(())
            }));
            unsafe { controller.add_LostFocus(&handler, &mut token) }.log_err();
        }
    }

    fn set_page_focus(&self, focused: bool) {
        self.state.borrow_mut().page_has_focus = focused;
        self.send(WebViewEvent::PageFocusChanged(focused));
    }

    /// `defer_cuts` postpones opening holes in the child window; see
    /// [`WebView::sync_viewport`] for why, and [`Inner::apply_clip`] for what is
    /// deferred. Returns whether a cut is still outstanding.
    fn apply_viewport(&self, defer_cuts: bool) -> bool {
        // Collect the deltas, drop the borrow, then talk to WebView2: the calls
        // below can re-enter handlers that write to `state`.
        let (controller, scale, bounds, visible, hidden_reason) = {
            let state = self.state.borrow();
            let Some(controller) = state.controller.clone() else {
                return false;
            };
            (
                controller,
                (state.applied_scale != Some(state.scale)).then_some(state.scale),
                (state.applied_bounds != Some(state.bounds)).then_some(state.bounds),
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

        self.apply_clip(defer_cuts)
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

        for child in children {
            let child = HWND(child as *mut c_void);
            // `EnumChildWindows` reports the whole subtree; the windows nested
            // inside WebView2's own host are clipped along with it.
            if unsafe { GetParent(child) }.ok() != Some(self.host_hwnd) {
                continue;
            }
            clip_child_window(self.host_hwnd, child, &holes, scale);
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

/// Diagnostics: when the surface is configured correctly but the user still
/// sees nothing, this log tells whether the child HWND exists and where Windows
/// thinks it is - which separates "WebView2 never attached" from "the child
/// window is fine but the compositor never shows it" (see
/// [`host_uses_direct_composition`]).
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

    let holes = viewport
        .overlays
        .iter()
        .filter_map(|overlay| {
            intersect_rects(
                bounds,
                RECT {
                    left: to_device(overlay.left),
                    top: to_device(overlay.top),
                    right: to_device(overlay.right),
                    bottom: to_device(overlay.bottom),
                },
            )
        })
        .collect();

    (bounds, holes)
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
fn clip_child_window(host_hwnd: HWND, child: HWND, holes: &[RECT], scale: f64) {
    if holes.is_empty() {
        // Passing no region restores the default (the whole window) and
        // repaints it.
        if unsafe { SetWindowRgn(child, None, true) } == 0 {
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
    if unsafe { SetWindowRgn(child, Some(region), true) } == 0 {
        log::warn!(
            "WebView2: could not clip child window {:#x} to the gpui overlays",
            child.0 as isize
        );
        unsafe { DeleteObject(region.into()) }.ok().log_err();
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
