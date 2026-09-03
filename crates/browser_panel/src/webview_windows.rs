//! WebView2 host for [`crate::BrowserPanel`] (ADR-F19).
//!
//! The controller is created as a child of the GPUI top-level HWND, so every
//! COM object here lives on the UI thread and is deliberately `!Send`. WebView2
//! delivers its callbacks on that same thread, but without a GPUI context, so
//! everything the panel needs to react to is forwarded through an unbounded
//! channel that the panel drains from a GPUI task.

use std::{cell::RefCell, ffi::c_void, path::Path, rc::Rc};

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
        Foundation::{E_POINTER, HWND, LPARAM, RECT},
        UI::{
            Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE},
            WindowsAndMessaging::{
                EnumChildWindows, GWL_EXSTYLE, GetClassNameW, GetWindowLongPtrW, GetWindowRect,
                HWND_TOP, IsIconic, IsWindowVisible, SW_SHOWNA, SWP_NOACTIVATE, SWP_NOMOVE,
                SWP_NOSIZE, SetWindowPos, ShowWindow, WS_EX_NOREDIRECTIONBITMAP,
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
    scale: f64,
    visible: bool,
    applied_bounds: Option<RECT>,
    applied_scale: Option<f64>,
    applied_visible: Option<bool>,
    page_has_focus: bool,
    hidden_reason: Option<&'static str>,
    /// The render widget is nudged to the top of the sibling order once, the
    /// first time the surface is made visible; it is a workaround, not
    /// something to repeat per frame.
    nudged_child_windows: bool,
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

        let options = CoreWebView2EnvironmentOptions::default();
        if let Some(proxy_endpoint) = proxy_endpoint {
            // Chromium reads `--proxy-server` once at browser-process start, so
            // it has to be set before the environment exists; there is no
            // runtime equivalent in the WebView2 API.
            unsafe {
                options
                    .set_additional_browser_arguments(format!("--proxy-server={proxy_endpoint}"));
            }
        }
        let options: ICoreWebView2EnvironmentOptions = options.into();
        log::info!(
            "WebView2: requesting an environment (host hwnd {host_hwnd:#x}, user data folder {}, proxy {}, direct composition on the host window: {})",
            user_data_folder.display(),
            proxy_endpoint.unwrap_or("none"),
            host_uses_direct_composition(host_hwnd),
        );
        let user_data_folder = HSTRING::from(user_data_folder.as_os_str());

        let handler = {
            let inner = inner.clone();
            CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(
                move |result, environment| {
                    match result.and_then(|()| environment.ok_or_else(|| E_POINTER.into())) {
                        Ok(environment) => {
                            log::info!("WebView2: environment created");
                            inner.create_controller(environment)
                        }
                        Err(error) => inner.report_failure(&error),
                    }
                    Ok(())
                },
            ))
        };

        unsafe {
            CreateCoreWebView2EnvironmentWithOptions(
                PCWSTR::null(),
                PCWSTR(user_data_folder.as_ptr()),
                &options,
                &handler,
            )
        }
        .context("failed to request a WebView2 environment")?;

        Ok(Self { inner })
    }

    /// Places the browsing surface under the panel's content area.
    /// `ICoreWebView2Controller::SetBounds` wants client-area coordinates of the
    /// parent HWND in physical pixels, so GPUI's logical bounds are multiplied
    /// by the window's scale factor here (and nowhere else: WebView2 is told not
    /// to follow the monitor DPI on its own).
    pub(crate) fn sync_viewport(&self, viewport: Viewport) {
        let scale = viewport.scale_factor.max(0.1) as f64;
        let to_device = |value: f32| (value * viewport.scale_factor).round() as i32;
        let left = to_device(viewport.origin_x);
        let top = to_device(viewport.origin_y);
        let bounds = RECT {
            left,
            top,
            right: left + to_device(viewport.width).max(0),
            bottom: top + to_device(viewport.height).max(0),
        };

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
            state.scale = scale;
            state.visible = hidden_reason.is_none();
            state.hidden_reason = hidden_reason;
        }
        self.inner.apply_viewport();
    }

    /// Hides the surface without touching the cached geometry, so the next
    /// paint can show it again in place.
    pub(crate) fn hide(&self, reason: &'static str) {
        {
            let mut state = self.inner.state.borrow_mut();
            state.visible = false;
            state.hidden_reason = Some(reason);
        }
        self.inner.apply_viewport();
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

        let (pending_url, pending_dark_mode) = {
            let mut state = self.state.borrow_mut();
            state.controller = Some(controller);
            state.webview = Some(webview.clone());
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
        self.apply_viewport();
        // Always take the inventory once, even when the surface stays hidden:
        // it is the one log line that separates "WebView2 never attached" from
        // "the child window exists but is never composited".
        log_child_windows(self.host_hwnd, false);
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

    fn apply_viewport(&self) {
        // Collect the deltas, drop the borrow, then talk to WebView2: the calls
        // below can re-enter handlers that write to `state`.
        let (controller, scale, bounds, visible, hidden_reason) = {
            let state = self.state.borrow();
            let Some(controller) = state.controller.clone() else {
                return;
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
            // The borrow has to be gone before `log_child_windows` talks to
            // Win32, and holding it across the `if` would deadlock this cell.
            let first_show = {
                let mut state = self.state.borrow_mut();
                state.applied_visible = Some(visible);
                let first_show = visible && !state.nudged_child_windows;
                state.nudged_child_windows |= visible;
                first_show
            };
            if first_show {
                log_child_windows(self.host_hwnd, true);
            }
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
    // The pointer comes straight from `log_child_windows` below, which keeps
    // the `Vec` alive for the whole call.
    let children = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
    children.push(hwnd);
    true.into()
}

/// Logs what WebView2 actually parented under the host window, and - when
/// `nudge` is set - pushes its render widget to the top of the sibling order.
///
/// This is diagnostics first: when the surface is configured correctly but the
/// user still sees nothing, the log tells whether the child HWND exists and
/// where Windows thinks it is - which separates "WebView2 never attached" from
/// "the child window is fine but the compositor never shows it" (see
/// [`host_uses_direct_composition`]). `nudge` is only passed while the surface
/// is meant to be on screen; forcing a window we deliberately hid back into
/// view would make it eat mouse input over the panel's content area.
fn log_child_windows(host_hwnd: HWND, nudge: bool) {
    let mut children: Vec<HWND> = Vec::new();
    // The return value only says whether the callback ran to the end, which it
    // always does here; an empty `children` is the real "nothing found" signal.
    let _enumerated = unsafe {
        EnumChildWindows(
            Some(host_hwnd),
            Some(collect_child_window),
            LPARAM(&mut children as *mut _ as isize),
        )
    };

    if children.is_empty() {
        log::warn!(
            "WebView2: the host window has no child windows; the browsing surface was never attached"
        );
        return;
    }

    for child in children {
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

        if nudge {
            // Belt and braces: WebView2 shows its own child, but a window that
            // was parented while the host was hidden has been seen to stay
            // hidden. The return value is "was it visible before", not an error.
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
