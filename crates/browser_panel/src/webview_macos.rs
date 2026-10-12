//! WKWebView host for [`crate::BrowserView`] on macOS (ADR-F89).
//!
//! The page is a `WKWebView` inside a container `NSView` that is added as a
//! subview of the GPUI view. A subview's layer is a sublayer of GPUI's Metal
//! layer, so the page composites in front of everything gpui draws - the same
//! arrangement as a Windows host without DirectComposition, which is why
//! [`host_uses_direct_composition`] reports `false` and the panel hides the
//! page during drags and modals. gpui's tooltips, menus and popovers are made
//! visible by masking holes out of the container's layer, and the container's
//! hit test lets clicks through those holes to gpui.
//!
//! Everything here runs on the main thread, which is gpui's foreground thread
//! on macOS. WebKit calls the delegates there too, but without a GPUI context,
//! so the view hears about them through an unbounded channel, as on Windows.

use std::{
    cell::{Cell, OnceCell, RefCell},
    ffi::{CString, c_char, c_void},
    ptr::{self, NonNull},
    rc::{Rc, Weak},
};

use anyhow::{Context as _, Result, anyhow, bail};
use block2::{DynBlock, RcBlock};
use futures::channel::{mpsc::UnboundedSender, oneshot};
use gpui::{SharedString, Window};
use objc2::{
    ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject},
    sel,
};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
    NSAutoresizingMaskOptions, NSBitmapImageFileType, NSBitmapImageRep, NSColor, NSEvent,
    NSEventMask, NSImage, NSResponder, NSView, NSWindow,
};
use objc2_core_graphics::CGMutablePath;
use objc2_foundation::{
    NSArray, NSDictionary, NSError, NSKeyValueObservingOptions,
    NSObjectNSKeyValueObserverRegistration, NSOperatingSystemVersion, NSPoint, NSProcessInfo,
    NSRect, NSRunLoopCommonModes, NSSize, NSString, NSURL, NSURLRequest, NSUUID,
};
use objc2_quartz_core::{CALayer, CAShapeLayer, CATransaction, kCAFillRuleEvenOdd};
use objc2_web_kit::{
    WKDownload, WKFrameInfo, WKMediaCaptureType, WKNavigation, WKNavigationAction,
    WKNavigationActionPolicy, WKNavigationDelegate, WKNavigationResponse,
    WKNavigationResponsePolicy, WKPermissionDecision, WKPreferences, WKSecurityOrigin,
    WKSnapshotConfiguration, WKUIDelegate, WKUserContentController, WKUserScript,
    WKUserScriptInjectionTime, WKWebView, WKWebViewConfiguration, WKWebsiteDataStore,
    WKWindowFeatures,
};
use url::{Host, Url};

pub(crate) use crate::webview_types::*;

/// The data store of the AI profile. A fixed identifier, so that what the user
/// signs in to inside an AI tab (the app under test) survives restarts, and
/// never the default store, which holds the user's own sign-ins (ADR-F89).
const AI_DATA_STORE_IDENTIFIER: [u8; 16] = [
    0x6b, 0x1f, 0x4a, 0x52, 0x93, 0xe7, 0x4c, 0x0d, 0x8a, 0x2e, 0x5d, 0x7c, 0x90, 0xb3, 0xf1, 0xa4,
];

/// `kVK_Escape`.
const ESCAPE_KEY_CODE: u16 = 53;

/// Corner radius of the holes cut for gpui's overlays, in points; see the
/// constant of the same name in `webview_windows`.
const OVERLAY_CORNER_RADIUS: f64 = 8.;

/// How deep, in points, an overlay may reach into the surface across one of
/// its edges and still not be cut out; see `EDGE_SLIVER_DEPTH` in
/// `webview_windows` (a neighbouring dock's resize handle).
const EDGE_SLIVER_DEPTH: f64 = 4.;

/// `browser_eval` runs in every AI tab: one is only created once WebRTC and
/// WebTransport are known to be off in it.
///
/// ICE (WebRTC) and QUIC (WebTransport) are UDP, which the HTTP CONNECT proxy
/// cannot carry, so either would reach the network around the egress
/// allowlist. WebKit has no public switch for them short of Lockdown Mode
/// (which also takes JIT, WebAssembly and web fonts away from the app under
/// test), so an AI tab turns them off through `WKPreferences` SPI, guarded by
/// `respondsToSelector:` and read back; where that does not hold, no AI tab is
/// created. Page JavaScript could get around anything weaker, such as
/// [`DISABLE_WEBRTC_SCRIPT`] (a script-made frame keeps the constructors).
pub(crate) const SUPPORTS_SCRIPT_EVAL: bool = true;

/// Defence in depth behind the preferences of [`SUPPORTS_SCRIPT_EVAL`]:
/// removes the WebRTC constructors before any page script runs, in every frame WebKit injects user scripts
/// into. A frame it does not inject into (one a script creates and reads
/// before it navigates) keeps them. `navigator.mediaDevices` stays: capturing
/// a camera or a microphone opens no connection by itself.
const DISABLE_WEBRTC_SCRIPT: &str = r#"(() => {
  for (const name of [
    "RTCPeerConnection", "webkitRTCPeerConnection", "RTCDataChannel",
    "RTCRtpSender", "RTCRtpReceiver", "RTCRtpTransceiver", "RTCIceCandidate",
    "RTCIceTransport", "RTCDtlsTransport", "RTCSctpTransport",
    "RTCSessionDescription", "RTCCertificate",
  ]) {
    try {
      Object.defineProperty(window, name, { value: undefined, writable: false, configurable: false });
    } catch (error) {
      try { delete window[name]; } catch (ignored) {}
    }
  }
})();"#;

/// Identifies this backend's key-value observations among any others the page
/// view may have.
static KVO_CONTEXT: u8 = 0;

/// The key paths of `WKWebView` that become [`WebViewEvent`]s.
const OBSERVED_KEY_PATHS: [&str; 4] = ["URL", "title", "canGoBack", "canGoForward"];

fn kvo_context() -> *mut c_void {
    ptr::addr_of!(KVO_CONTEXT).cast_mut().cast()
}

pub(crate) fn host_window_handle(window: &Window) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    // `gpui::Window` has its own inherent `window_handle`, so the trait method
    // has to be named explicitly.
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::AppKit(handle) => Some(handle.ns_view.as_ptr() as isize),
        _ => None,
    }
}

/// The page is a subview of the GPUI view, so it is always in front of what
/// gpui draws; nothing is composited above it.
pub(crate) fn host_uses_direct_composition(_host: isize) -> bool {
    false
}

/// WebKit ships with macOS, so a failure here is never a missing runtime.
pub(crate) fn runtime_missing_message() -> SharedString {
    i18n::t!("The built-in browser couldn't be started in this window.").into()
}

/// Ties the Objective-C objects back to the surface they belong to. Weak, so
/// that the surface (which owns them) is the only thing keeping it alive; set
/// once the surface exists, which is after the objects do.
#[derive(Default)]
struct SurfaceLink {
    surface: OnceCell<Weak<Inner>>,
}

impl SurfaceLink {
    fn surface(&self) -> Option<Rc<Inner>> {
        self.surface.get().and_then(Weak::upgrade)
    }

    fn link(&self, surface: &Rc<Inner>) {
        if self.surface.set(Rc::downgrade(surface)).is_err() {
            log::error!("WKWebView: an object was linked to a second surface");
        }
    }
}

define_class!(
    /// The container the page sits in. It carries the mask that lets gpui's
    /// overlays show through and refuses the mouse where they are, or
    /// everywhere while the page takes no input.
    // SAFETY: `NSView` has no subclassing requirements, and nothing here
    // implements `Drop`.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = SurfaceLink]
    struct SurfaceContainer;

    impl SurfaceContainer {
        /// `point` is in the superview's (the GPUI view's) coordinates.
        /// Returning nil makes AppKit hand the event to the GPUI view.
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> *mut NSView {
            if let Some(surface) = self.ivars().surface() {
                let superview = unsafe { self.superview() };
                let local = self.convertPoint_fromView(point, superview.as_deref());
                let from_top = if self.isFlipped() {
                    local.y
                } else {
                    self.bounds().size.height - local.y
                };
                if surface.refuses_mouse_at(local.x, from_top) {
                    return ptr::null_mut();
                }
            }
            unsafe { msg_send![super(self), hitTest: point] }
        }
    }
);

impl SurfaceContainer {
    fn new(mtm: MainThreadMarker) -> Option<Retained<Self>> {
        let this = Self::alloc(mtm).set_ivars(SurfaceLink::default());
        unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] }
    }
}

define_class!(
    /// The page itself. Subclassed only to hear when it gains and loses the
    /// keyboard focus, which gpui cannot see.
    // SAFETY: `WKWebView` may be subclassed, the overrides call through to it,
    // and nothing here implements `Drop`.
    #[unsafe(super(WKWebView, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = SurfaceLink]
    struct PageView;

    impl PageView {
        #[unsafe(method(becomeFirstResponder))]
        fn become_first_responder(&self) -> bool {
            let accepted: bool = unsafe { msg_send![super(self), becomeFirstResponder] };
            if accepted && let Some(surface) = self.ivars().surface() {
                surface.set_page_focus(true);
            }
            accepted
        }

        #[unsafe(method(resignFirstResponder))]
        fn resign_first_responder(&self) -> bool {
            let resigned: bool = unsafe { msg_send![super(self), resignFirstResponder] };
            if resigned && let Some(surface) = self.ivars().surface() {
                surface.set_page_focus(false);
            }
            resigned
        }
    }
);

impl PageView {
    fn new(
        configuration: &WKWebViewConfiguration,
        mtm: MainThreadMarker,
    ) -> Option<Retained<Self>> {
        let this = Self::alloc(mtm).set_ivars(SurfaceLink::default());
        unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO, configuration: configuration] }
    }

    /// Whether an input method is composing in the page; `Esc` then belongs
    /// to the input method.
    fn has_marked_text(&self) -> bool {
        // `WKWebView` implements `NSTextInputClient` on macOS; checked anyway,
        // since an unrecognised selector would raise.
        self.respondsToSelector(sel!(hasMarkedText)) && unsafe { msg_send![self, hasMarkedText] }
    }
}

define_class!(
    /// WebKit's navigation and UI delegate for one surface, and the observer of
    /// the page's location, title and history.
    // SAFETY: `NSObject` has no subclassing requirements, and nothing here
    // implements `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = SurfaceLink]
    struct SurfaceDelegate;

    impl SurfaceDelegate {
        #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
        fn observe_value(
            &self,
            key_path: Option<&NSString>,
            _object: Option<&AnyObject>,
            _change: Option<&AnyObject>,
            context: *mut c_void,
        ) {
            if context != kvo_context() {
                return;
            }
            if let (Some(surface), Some(key_path)) = (self.ivars().surface(), key_path) {
                surface.observed_change(&key_path.to_string());
            }
        }
    }

    unsafe impl NSObjectProtocol for SurfaceDelegate {}

    unsafe impl WKNavigationDelegate for SurfaceDelegate {
        #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
        fn decide_policy_for_navigation_action(
            &self,
            _web_view: &WKWebView,
            navigation_action: &WKNavigationAction,
            decision_handler: &DynBlock<dyn Fn(WKNavigationActionPolicy)>,
        ) {
            let policy = match self.ivars().surface() {
                Some(surface) => surface.navigation_action_policy(navigation_action),
                None => WKNavigationActionPolicy::Cancel,
            };
            decision_handler.call((policy,));
        }

        #[unsafe(method(webView:decidePolicyForNavigationResponse:decisionHandler:))]
        fn decide_policy_for_navigation_response(
            &self,
            _web_view: &WKWebView,
            navigation_response: &WKNavigationResponse,
            decision_handler: &DynBlock<dyn Fn(WKNavigationResponsePolicy)>,
        ) {
            let policy = match self.ivars().surface() {
                Some(surface) => surface.navigation_response_policy(navigation_response),
                None => WKNavigationResponsePolicy::Cancel,
            };
            decision_handler.call((policy,));
        }

        #[unsafe(method(webView:didFinishNavigation:))]
        fn did_finish_navigation(&self, _web_view: &WKWebView, _navigation: Option<&WKNavigation>) {
            if let Some(surface) = self.ivars().surface() {
                surface.load_finished(true);
            }
        }

        #[unsafe(method(webView:didFailNavigation:withError:))]
        fn did_fail_navigation(
            &self,
            _web_view: &WKWebView,
            _navigation: Option<&WKNavigation>,
            error: &NSError,
        ) {
            log::info!("WKWebView: navigation failed: {}", describe_error(error));
            if let Some(surface) = self.ivars().surface() {
                surface.load_finished(false);
            }
        }

        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn did_fail_provisional_navigation(
            &self,
            _web_view: &WKWebView,
            _navigation: Option<&WKNavigation>,
            error: &NSError,
        ) {
            log::info!("WKWebView: navigation failed to start: {}", describe_error(error));
            if let Some(surface) = self.ivars().surface() {
                surface.load_finished(false);
            }
        }

        /// Only reached through WebKit's own UI (a context menu's "Download
        /// Linked File"): the policies above never ask for a download. With no
        /// download delegate WebKit would not save it either, so it is
        /// cancelled outright.
        #[unsafe(method(webView:navigationAction:didBecomeDownload:))]
        fn navigation_action_did_become_download(
            &self,
            _web_view: &WKWebView,
            _navigation_action: &WKNavigationAction,
            download: &WKDownload,
        ) {
            cancel_download(self.ivars().surface(), download);
        }

        #[unsafe(method(webView:navigationResponse:didBecomeDownload:))]
        fn navigation_response_did_become_download(
            &self,
            _web_view: &WKWebView,
            _navigation_response: &WKNavigationResponse,
            download: &WKDownload,
        ) {
            cancel_download(self.ivars().surface(), download);
        }

        #[unsafe(method(webViewWebContentProcessDidTerminate:))]
        fn web_content_process_did_terminate(&self, _web_view: &WKWebView) {
            log::error!("WKWebView: the web content process terminated");
            if let Some(surface) = self.ivars().surface() {
                surface.load_finished(false);
            }
        }
    }

    unsafe impl WKUIDelegate for SurfaceDelegate {
        /// The panel hosts a single page per surface, so a `target="_blank"`
        /// link or `window.open` is re-targeted at this one instead.
        #[unsafe(method_id(webView:createWebViewWithConfiguration:forNavigationAction:windowFeatures:))]
        fn create_web_view(
            &self,
            _web_view: &WKWebView,
            _configuration: &WKWebViewConfiguration,
            navigation_action: &WKNavigationAction,
            _window_features: &WKWindowFeatures,
        ) -> Option<Retained<WKWebView>> {
            let url = request_url(navigation_action);
            if let (Some(surface), Some(url)) = (self.ivars().surface(), url)
                && !url.is_empty()
            {
                surface.send(WebViewEvent::OpenInPlace(url));
            }
            None
        }

        /// AI tabs never get the camera or the microphone: the AI must not
        /// capture what is around this machine. Other tabs keep WebKit's
        /// default, which asks the user. Geolocation has no public delegate
        /// method (and the app bundle declares location use for terminal
        /// programs), so `automation.js` answers it with a denial in AI tabs.
        #[unsafe(method(webView:requestMediaCapturePermissionForOrigin:initiatedByFrame:type:decisionHandler:))]
        fn request_media_capture_permission(
            &self,
            _web_view: &WKWebView,
            _origin: &WKSecurityOrigin,
            _frame: &WKFrameInfo,
            _capture_type: WKMediaCaptureType,
            decision_handler: &DynBlock<dyn Fn(WKPermissionDecision)>,
        ) {
            let decision = match self.ivars().surface() {
                Some(surface) if surface.profile == WebViewProfile::User => {
                    WKPermissionDecision::Prompt
                }
                _ => WKPermissionDecision::Deny,
            };
            decision_handler.call((decision,));
        }
    }
);

impl SurfaceDelegate {
    fn new(mtm: MainThreadMarker) -> Option<Retained<Self>> {
        let this = Self::alloc(mtm).set_ivars(SurfaceLink::default());
        unsafe { msg_send![super(this), init] }
    }
}

pub(crate) struct WebView {
    inner: Rc<Inner>,
}

struct Inner {
    /// The GPUI view. Weak: gpui owns it, and the surface must not keep a
    /// closed window's view alive.
    host_view: objc2::rc::Weak<NSView>,
    container: Retained<SurfaceContainer>,
    page: Retained<PageView>,
    /// `WKWebView` holds its delegates weakly.
    delegate: Retained<SurfaceDelegate>,
    /// The local key-down monitor that turns `Esc` in the page into
    /// [`WebViewEvent::FocusReturned`].
    key_monitor: RefCell<Option<Retained<AnyObject>>>,
    navigation_filter: Option<Rc<dyn Fn(&str) -> NavigationDecision>>,
    block_downloads: bool,
    profile: WebViewProfile,
    events: UnboundedSender<WebViewEvent>,
    state: RefCell<State>,
}

#[derive(Default)]
struct State {
    /// Where the page goes, in points from the GPUI view's top-left corner.
    bounds: SurfaceRect,
    /// Parts of `bounds` gpui draws over this frame, in the same space.
    holes: Vec<SurfaceRect>,
    visible: bool,
    hidden_reason: Option<&'static str>,
    input_blocked: bool,
    /// Whether the view held gpui's focus at the last paint.
    view_focused: bool,
    /// A `focus_page` that came while the page took no input; moved into the
    /// page once it does again, if the view still holds gpui's focus.
    pending_focus: bool,
    applied_frame: Option<NSRect>,
    applied_visible: Option<bool>,
    applied_input_blocked: Option<bool>,
    /// The `(bounds, holes)` the current mask was built from.
    applied_clip: Option<(SurfaceRect, Vec<SurfaceRect>)>,
    /// The holes of the current mask, relative to the container's top-left
    /// corner, for the hit test.
    mouse_holes: Vec<SurfaceRect>,
}

impl WebView {
    /// Creates the page right away; WebKit has nothing asynchronous to wait
    /// for. [`WebViewEvent::Ready`] is still sent, so the view follows the
    /// same path as on Windows.
    pub(crate) fn new(
        host: isize,
        options: WebViewOptions,
        events: UnboundedSender<WebViewEvent>,
    ) -> Result<Self> {
        let mtm = MainThreadMarker::new()
            .context("the built-in browser has to be created on the main thread")?;
        let host_view = NonNull::new(host as *mut NSView).context("the host view is null")?;
        // SAFETY: `host` is the GPUI view of a live window, handed out by
        // `host_window_handle` during this paint.
        let host_view =
            unsafe { Retained::retain(host_view.as_ptr()) }.context("the host view is null")?;

        let data_store = website_data_store(&options, mtm)?;
        let configuration = unsafe { WKWebViewConfiguration::new(mtm) };
        unsafe { configuration.setWebsiteDataStore(&data_store) };
        let user_content = unsafe { configuration.userContentController() };
        // Ahead of the container and the page, so that a refusal leaves
        // nothing behind in the host view.
        if options.profile == WebViewProfile::Ai {
            let preferences = unsafe { configuration.preferences() };
            if !switch_off_unproxied_transports(&preferences) {
                bail!(
                    "AI tabs need WebKit's switch for WebRTC, which this version of macOS \
                     doesn't offer: WebRTC would reach the network around the egress proxy"
                );
            }
            add_document_start_script(&user_content, DISABLE_WEBRTC_SCRIPT, mtm);
        }
        if let Some(script) = &options.document_start_script {
            add_document_start_script(&user_content, script, mtm);
        }

        let container =
            SurfaceContainer::new(mtm).context("could not create the page's container")?;
        let page = PageView::new(&configuration, mtm).context("could not create the WKWebView")?;
        let delegate = SurfaceDelegate::new(mtm).context("could not create the WebKit delegate")?;
        let inner = Rc::new(Inner {
            host_view: objc2::rc::Weak::from_retained(&host_view),
            container,
            page,
            delegate,
            key_monitor: RefCell::new(None),
            navigation_filter: options.navigation_filter.clone(),
            block_downloads: options.block_downloads,
            profile: options.profile,
            events,
            state: RefCell::new(State {
                hidden_reason: Some("the panel has not been painted yet"),
                applied_visible: Some(false),
                ..State::default()
            }),
        });
        inner.container.ivars().link(&inner);
        inner.page.ivars().link(&inner);
        inner.delegate.ivars().link(&inner);
        inner.attach(&host_view);

        log::info!(
            "WKWebView: created a {:?} surface in host view {host:#x}",
            options.profile
        );
        inner.send(WebViewEvent::Ready);
        inner.publish_location();
        Ok(Self { inner })
    }

    /// Places the page over the panel's content area. GPUI's logical pixels
    /// are AppKit's points, so only the vertical axis needs converting (the
    /// GPUI view is not flipped).
    ///
    /// The overlays gpui reported for this frame are masked out of the page,
    /// which is what makes them visible at all: the page is in front of
    /// everything gpui draws. Returns `true` when that was held back to the
    /// next frame, for the same reason as on Windows: the frame that draws
    /// the overlay has not been presented yet.
    ///
    /// `view_focused` says whether the view holds gpui's focus: a
    /// [`WebView::focus_page`] refused while the page took no input is only
    /// moved into the page later while it does.
    #[must_use]
    pub(crate) fn sync_viewport(&self, viewport: Viewport, view_focused: bool) -> bool {
        let (bounds, holes) = viewport_geometry(&viewport);
        let mut hidden_reason = viewport.hidden_reason;
        if hidden_reason.is_none() && bounds.is_empty() {
            hidden_reason = Some("the content rect is empty");
        }
        {
            let mut state = self.inner.state.borrow_mut();
            state.bounds = bounds;
            state.holes = holes;
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
        log::info!("WKWebView: navigating to {url}");
        let Some(ns_url) = NSURL::URLWithString(&NSString::from_str(url)) else {
            log::warn!("WKWebView: cannot navigate to {url}: not a valid URL");
            self.inner.load_finished(false);
            return;
        };
        let request = NSURLRequest::requestWithURL(&ns_url);
        if unsafe { self.inner.page.loadRequest(&request) }.is_none() {
            log::warn!("WKWebView: WebKit did not start a navigation to {url}");
        }
    }

    pub(crate) fn go_back(&self) {
        if unsafe { self.inner.page.goBack() }.is_none() {
            log::info!("WKWebView: there is no page to go back to");
        }
    }

    pub(crate) fn go_forward(&self) {
        if unsafe { self.inner.page.goForward() }.is_none() {
            log::info!("WKWebView: there is no page to go forward to");
        }
    }

    pub(crate) fn reload(&self) {
        if unsafe { self.inner.page.reload() }.is_none() {
            log::info!("WKWebView: there is nothing to reload");
        }
    }

    /// The page follows the view's appearance for `prefers-color-scheme`.
    pub(crate) fn set_dark_mode(&self, dark: bool) {
        let name = if dark {
            unsafe { NSAppearanceNameDarkAqua }
        } else {
            unsafe { NSAppearanceNameAqua }
        };
        match NSAppearance::appearanceNamed(name) {
            Some(appearance) => self.inner.page.setAppearance(Some(&appearance)),
            None => log::warn!("WKWebView: the system has no {name} appearance"),
        }
    }

    /// What WebKit shows where the page has nothing to draw, such as past its
    /// end while scrolling elastically. A page's own background stays the
    /// page's: the only switch for that (`drawsBackground`) is private API.
    pub(crate) fn set_background_color(&self, red: u8, green: u8, blue: u8) {
        let page = &self.inner.page;
        // macOS 12 and later.
        if !page.respondsToSelector(sel!(setUnderPageBackgroundColor:)) {
            return;
        }
        let channel = |value: u8| f64::from(value) / 255.;
        let color = NSColor::colorWithSRGBRed_green_blue_alpha(
            channel(red),
            channel(green),
            channel(blue),
            1.,
        );
        unsafe { page.setUnderPageBackgroundColor(Some(&color)) };
    }

    /// Hands the keyboard back to the GPUI view when the page owns it. AppKit
    /// does not move the focus off the page when a click lands on the GPUI
    /// view (it never asks to be first responder), so the panel calls this
    /// from a window-wide mouse-down listener.
    pub(crate) fn release_keyboard_focus(&self) {
        // The keyboard is wanted elsewhere now, so a focus request still
        // waiting for the page to take input again is stale.
        self.inner.state.borrow_mut().pending_focus = false;
        if self.inner.page_owns_keyboard_focus() {
            self.inner.request_host_focus();
        }
    }

    /// Moves keyboard focus into the page. GPUI keeps its own focus state, so
    /// the caller is responsible for parking the logical focus on the panel.
    pub(crate) fn focus_page(&self) {
        let input_blocked = self.inner.state.borrow().applied_input_blocked == Some(true);
        if input_blocked {
            self.inner.state.borrow_mut().pending_focus = true;
            return;
        }
        self.inner.request_page_focus();
    }
}

impl WebView {
    /// Runs `script` (an expression, or an IIFE for statements) in the page's
    /// main frame and hands back the JSON of its completion value, like
    /// WebView2's `ExecuteScript`. WebKit converts results to Foundation types
    /// itself, so the script is wrapped to serialise in the page instead.
    pub(crate) fn execute_script(&self, script: &str) -> oneshot::Receiver<Result<String>> {
        let (sender, receiver) = oneshot::channel();
        let sender = Cell::new(Some(sender));
        let handler = RcBlock::new(move |value: *mut AnyObject, error: *mut NSError| {
            // WebKit calls this once; the `take` keeps a second call harmless.
            let Some(sender) = sender.take() else {
                return;
            };
            let result = unsafe { script_result(value.as_ref(), error.as_ref()) };
            sender.send(result).ok();
        });
        let source = NSString::from_str(&json_completion_script(script));
        unsafe {
            self.inner
                .page
                .evaluateJavaScript_completionHandler(&source, Some(&handler))
        };
        receiver
    }

    /// The visible viewport as a PNG, at the display's pixel density.
    pub(crate) fn capture_png(&self) -> oneshot::Receiver<Result<Vec<u8>>> {
        let (sender, receiver) = oneshot::channel();
        let sender = Cell::new(Some(sender));
        let handler = RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
            let Some(sender) = sender.take() else {
                return;
            };
            let result = unsafe { snapshot_png(image.as_ref(), error.as_ref()) };
            sender.send(result).ok();
        });
        // A page that is not on screen presents no further updates, so waiting
        // for the next one (the default) would never call back.
        let on_screen = !self.inner.container.isHiddenOrHasHiddenAncestor()
            && self.inner.page.window().is_some();
        let mtm = MainThreadMarker::from(&*self.inner.page);
        let configuration = unsafe { WKSnapshotConfiguration::new(mtm) };
        unsafe { configuration.setAfterScreenUpdates(on_screen) };
        unsafe {
            self.inner
                .page
                .takeSnapshotWithConfiguration_completionHandler(Some(&configuration), &handler)
        };
        receiver
    }

    /// WebKit has no public API for trusted input, so the core synthesises it
    /// in page JavaScript instead.
    pub(crate) fn dispatch_input(&self, _input: NativeInput) -> oneshot::Receiver<Result<()>> {
        completed(Err(NativeInputUnsupported.into()))
    }

    /// The page is made inspectable when it is created (macOS 13.3 and
    /// later), but no public API opens Web Inspector, so the user opens it
    /// from the page's context menu.
    pub(crate) fn open_developer_tools(&self) {
        if self.inner.page.respondsToSelector(sel!(isInspectable)) {
            log::info!(
                "WKWebView: Web Inspector has no public API to open it; \
                 right-click the page and choose Inspect Element"
            );
        } else {
            log::warn!("WKWebView: Web Inspector needs macOS 13.3 or later");
        }
    }

    pub(crate) fn is_detached(&self) -> bool {
        false
    }

    pub(crate) fn raise_detached_window(&self) {}
}

impl Drop for WebView {
    fn drop(&mut self) {
        let inner = &self.inner;
        // Removing a first responder from its window leaves the window itself
        // as first responder, and the GPUI view never asks for the focus on a
        // click, so gpui would stay deaf to the keyboard.
        if inner.page_owns_keyboard_focus() {
            inner.request_host_focus();
        }
        // A focus request for the page that has not run yet would retain it
        // and hand it the keyboard after it has left the window.
        if let Some(window) = inner.page.window() {
            let page: &NSView = &inner.page;
            unsafe {
                let () = msg_send![
                    NSObject::class(),
                    cancelPreviousPerformRequestsWithTarget: &*window,
                    selector: sel!(makeFirstResponder:),
                    object: page,
                ];
            }
        }
        if let Some(monitor) = inner.key_monitor.borrow_mut().take() {
            unsafe { NSEvent::removeMonitor(&monitor) };
        }
        for key_path in OBSERVED_KEY_PATHS {
            unsafe {
                inner.page.removeObserver_forKeyPath_context(
                    &inner.delegate,
                    &NSString::from_str(key_path),
                    kvo_context(),
                )
            };
        }
        unsafe {
            inner.page.setNavigationDelegate(None);
            inner.page.setUIDelegate(None);
            inner.page.stopLoading();
        }
        inner.container.removeFromSuperview();
    }
}

impl Inner {
    fn attach(self: &Rc<Self>, host_view: &NSView) {
        let container = &self.container;
        let page = &self.page;
        container.setWantsLayer(true);
        container.setHidden(true);
        page.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        container.addSubview(page);
        host_view.addSubview(container);

        unsafe {
            page.setNavigationDelegate(Some(ProtocolObject::from_ref(&*self.delegate)));
            page.setUIDelegate(Some(ProtocolObject::from_ref(&*self.delegate)));
        }
        // macOS 13.3 and later; before that a page is inspectable only in a
        // debug build of the app.
        if page.respondsToSelector(sel!(setInspectable:)) {
            unsafe { page.setInspectable(true) };
        }
        for key_path in OBSERVED_KEY_PATHS {
            unsafe {
                page.addObserver_forKeyPath_options_context(
                    &self.delegate,
                    &NSString::from_str(key_path),
                    NSKeyValueObservingOptions::New,
                    kvo_context(),
                )
            };
        }
        self.install_key_monitor();
    }

    /// `Esc` has to be caught before AppKit offers the key to the GPUI view
    /// as a key equivalent: gpui binds it in the workspace (`Unfollow`) and
    /// would swallow it while its logical focus is on the panel.
    fn install_key_monitor(self: &Rc<Self>) {
        let surface = Rc::downgrade(self);
        let handler = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
            // SAFETY: AppKit hands the monitor a live event.
            let event_ref = unsafe { event.as_ref() };
            if let Some(surface) = surface.upgrade()
                && surface.takes_escape(event_ref)
            {
                return ptr::null_mut();
            }
            event.as_ptr()
        });
        let monitor = unsafe {
            NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::KeyDown, &handler)
        };
        if monitor.is_none() {
            log::warn!("WKWebView: could not watch for Esc; it will not leave the page");
        }
        *self.key_monitor.borrow_mut() = monitor;
    }

    fn takes_escape(&self, event: &NSEvent) -> bool {
        if event.keyCode() != ESCAPE_KEY_CODE || !self.page_owns_keyboard_focus() {
            return false;
        }
        let Some(window) = self.page.window() else {
            return false;
        };
        let same_window = event.windowNumber() == window.windowNumber();
        if !same_window || self.page.has_marked_text() {
            return false;
        }
        // The monitor runs while AppKit dispatches the event, outside any gpui
        // update, so the GPUI view can take the focus right here.
        if let Some(host_view) = self.host_view.load() {
            window.makeFirstResponder(Some(&host_view));
        }
        self.send(WebViewEvent::FocusReturned);
        true
    }

    fn set_page_focus(&self, focused: bool) {
        self.send(WebViewEvent::PageFocusChanged(focused));
    }

    fn page_owns_keyboard_focus(&self) -> bool {
        let Some(window) = self.page.window() else {
            return false;
        };
        let Some(responder) = window.firstResponder() else {
            return false;
        };
        match responder.downcast::<NSView>() {
            Ok(view) => view.isDescendantOf(&self.container),
            Err(_) => false,
        }
    }

    /// The first-responder changes are left to the run loop: making a view
    /// first responder can call into the GPUI view's text input client, and
    /// these requests arrive while gpui is updating (painting, or dispatching
    /// a mouse event or an action), which such a call would re-enter.
    fn request_page_focus(&self) {
        if let Some(window) = self.page.window() {
            perform_make_first_responder(&window, &self.page);
        }
    }

    fn request_host_focus(&self) {
        if let (Some(window), Some(host_view)) = (self.page.window(), self.host_view.load()) {
            perform_make_first_responder(&window, &host_view);
        }
    }

    /// `x` and `from_top` are relative to the container's top-left corner.
    fn refuses_mouse_at(&self, x: f64, from_top: f64) -> bool {
        // The hit test can run inside AppKit calls made while the state is
        // borrowed (a frame change updates the cursor); the page keeps the
        // mouse then.
        let Ok(state) = self.state.try_borrow() else {
            return false;
        };
        state.applied_input_blocked == Some(true)
            || state
                .mouse_holes
                .iter()
                .any(|hole| hole.contains(x, from_top))
    }

    /// `defer_cuts` postpones masking holes into the page; see
    /// [`WebView::sync_viewport`]. Returns whether a cut is still outstanding.
    fn apply_viewport(&self, defer_cuts: bool) -> bool {
        let Some(host_view) = self.host_view.load() else {
            return false;
        };
        // Collect the deltas, drop the borrow, then talk to AppKit: the calls
        // below can re-enter the overrides above, which borrow `state`.
        let (bounds, input_blocked, visible, hidden_reason) = {
            let state = self.state.borrow();
            let showing = state.visible && state.applied_visible != Some(true);
            (
                state.bounds,
                // Also on every show, like on Windows.
                (showing || state.applied_input_blocked != Some(state.input_blocked))
                    .then_some(state.input_blocked),
                (state.applied_visible != Some(state.visible)).then_some(state.visible),
                state.hidden_reason,
            )
        };

        // Recomputed on every pass: an unflipped host measures from its
        // bottom edge, so resizing the window moves the page in its
        // coordinates even when gpui's layout has not changed.
        let frame = bounds.to_view_rect(host_view.bounds().size.height, host_view.isFlipped());
        let frame_changed = self.state.borrow().applied_frame != Some(frame);
        if frame_changed {
            self.container.setFrame(frame);
            log::debug!(
                "WKWebView: frame set to ({}, {}) {}x{} in points",
                frame.origin.x,
                frame.origin.y,
                frame.size.width,
                frame.size.height
            );
            self.state.borrow_mut().applied_frame = Some(frame);
        }

        // Ahead of the visibility, so that a surface shown while a modal is
        // open never takes a click in between. The hit test reads the applied
        // value; only the keyboard has to be taken off the page here.
        if let Some(input_blocked) = input_blocked {
            if input_blocked && self.page_owns_keyboard_focus() {
                self.request_host_focus();
            }
            self.state.borrow_mut().applied_input_blocked = Some(input_blocked);
        }

        if let Some(visible) = visible {
            // AppKit gives the first responder of a hidden view to the window
            // itself, not to the GPUI view.
            if !visible && self.page_owns_keyboard_focus() {
                self.request_host_focus();
            }
            self.container.setHidden(!visible);
            match hidden_reason {
                Some(reason) => log::info!("WKWebView: surface hidden because {reason}"),
                None => log::info!("WKWebView: surface shown"),
            }
            self.state.borrow_mut().applied_visible = Some(visible);
        }

        // After the visibility, so that a page shown in this same pass can
        // take it.
        if input_blocked == Some(false) {
            self.move_pending_focus();
        }

        self.apply_clip(defer_cuts)
    }

    fn move_pending_focus(&self) {
        let (pending_focus, view_focused) = {
            let mut state = self.state.borrow_mut();
            (std::mem::take(&mut state.pending_focus), state.view_focused)
        };
        // With gpui's focus gone elsewhere meanwhile, the page would pull the
        // keyboard away from wherever the user moved on to.
        if pending_focus && view_focused {
            log::info!("WKWebView: moving the focus the page was refused while it took no input");
            self.request_page_focus();
        }
    }

    /// Masks gpui's overlays out of the container. With `defer_cuts` a mask
    /// with holes is left for [`WebView::apply_deferred_clip`] and `true` is
    /// returned; giving the page back is never deferred.
    fn apply_clip(&self, defer_cuts: bool) -> bool {
        let (bounds, holes) = {
            let state = self.state.borrow();
            if !state.visible {
                return false;
            }
            let unchanged = match state.applied_clip.as_ref() {
                None => false,
                // No mask, and none needed: moving or resizing changes nothing.
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
            (state.bounds, state.holes.clone())
        };

        let local_holes = holes
            .iter()
            .map(|hole| hole.offset(-bounds.left, -bounds.top))
            .collect::<Vec<_>>();
        self.set_mask(bounds.width(), bounds.height(), &local_holes);

        let mut state = self.state.borrow_mut();
        state.mouse_holes = local_holes;
        state.applied_clip = Some((bounds, holes));
        false
    }

    /// `holes` are relative to the container's top-left corner.
    fn set_mask(&self, width: f64, height: f64, holes: &[SurfaceRect]) {
        // `-[NSView layer]`; the typed binding needs AppKit's QuartzCore
        // feature, which nothing else here does.
        let layer: Option<Retained<CALayer>> = unsafe { msg_send![&*self.container, layer] };
        let Some(layer) = layer else {
            log::warn!("WKWebView: the container has no layer to mask gpui's overlays out of");
            return;
        };
        // The mask layer belongs to no view, so every change to it would
        // otherwise be animated.
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        if holes.is_empty() {
            unsafe { layer.setMask(None) };
        } else {
            // Whether the layer's own coordinates run downwards on screen, which
            // AppKit derives from the views above it; the mask's path is drawn
            // in those coordinates.
            let flipped = layer.contentsAreFlipped();
            let whole = NSRect::new(NSPoint::new(0., 0.), NSSize::new(width, height));
            let path = CGMutablePath::new();
            unsafe { CGMutablePath::add_rect(Some(&path), ptr::null(), whole) };
            for hole in holes {
                let rect = hole.to_view_rect(height, flipped);
                let radius = hole.corner_radius();
                unsafe {
                    if radius > 0. {
                        CGMutablePath::add_rounded_rect(
                            Some(&path),
                            ptr::null(),
                            rect,
                            radius,
                            radius,
                        );
                    } else {
                        CGMutablePath::add_rect(Some(&path), ptr::null(), rect);
                    }
                }
            }
            let mask = CAShapeLayer::new();
            mask.setFrame(whole);
            mask.setPath(Some(&path));
            mask.setFillRule(unsafe { kCAFillRuleEvenOdd });
            unsafe { layer.setMask(Some(&mask)) };
        }
        CATransaction::commit();
    }

    fn navigation_action_policy(
        &self,
        navigation_action: &WKNavigationAction,
    ) -> WKNavigationActionPolicy {
        let url = request_url(navigation_action);
        // macOS 11.3 and later: `<a download>` and the like.
        if self.block_downloads
            && navigation_action.respondsToSelector(sel!(shouldPerformDownload))
            && unsafe { navigation_action.shouldPerformDownload() }
        {
            let url = url.unwrap_or_default();
            log::info!("WKWebView: blocked a download of {url}");
            self.send(WebViewEvent::DownloadBlocked { url });
            return WKNavigationActionPolicy::Cancel;
        }

        // No target frame is a request for a new window, which the UI
        // delegate turns into a navigation of this page that is decided in
        // its turn; frames inside the page are not top-level navigations.
        let top_level = unsafe { navigation_action.targetFrame() }
            .is_some_and(|frame| unsafe { frame.isMainFrame() });
        if !top_level {
            return WKNavigationActionPolicy::Allow;
        }
        // Defence in depth: `new` refuses an AI tab without WebRTC switched
        // off, and a `data:` page is the AI's own JavaScript, so one only opens
        // while the switch still reads back as off.
        if self.profile == WebViewProfile::Ai
            && let Some(url) = url.as_deref()
            && is_data_url(url)
            && !peer_connections_are_off(&*unsafe { self.page.configuration().preferences() })
        {
            let reason =
                "WebRTC is not switched off in this AI tab, so it does not open data: pages"
                    .to_string();
            log::info!("WKWebView: blocked a navigation to a data: URL: {reason}");
            self.send(WebViewEvent::NavigationBlocked {
                url: url.to_string(),
                reason,
            });
            return WKNavigationActionPolicy::Cancel;
        }
        let Some(filter) = &self.navigation_filter else {
            return WKNavigationActionPolicy::Allow;
        };
        let Some(url) = url else {
            self.send(WebViewEvent::NavigationBlocked {
                url: String::new(),
                reason: "the navigation has no URL".to_string(),
            });
            return WKNavigationActionPolicy::Cancel;
        };
        match filter(&url) {
            NavigationDecision::Allow => WKNavigationActionPolicy::Allow,
            NavigationDecision::Deny(reason) => {
                log::info!("WKWebView: blocked a navigation to {url}: {reason}");
                self.send(WebViewEvent::NavigationBlocked { url, reason });
                WKNavigationActionPolicy::Cancel
            }
        }
    }

    /// WebKit only downloads what the delegate asks it to, so a response it
    /// cannot show simply fails to load; with downloads blocked, that is
    /// reported as the blocked download it is.
    fn navigation_response_policy(
        &self,
        navigation_response: &WKNavigationResponse,
    ) -> WKNavigationResponsePolicy {
        if self.block_downloads && !unsafe { navigation_response.canShowMIMEType() } {
            let response = unsafe { navigation_response.response() };
            let url = response
                .URL()
                .and_then(|url| url.absoluteString())
                .map(|url| url.to_string())
                .unwrap_or_default();
            log::info!("WKWebView: blocked a download of {url}");
            self.send(WebViewEvent::DownloadBlocked { url });
            return WKNavigationResponsePolicy::Cancel;
        }
        WKNavigationResponsePolicy::Allow
    }

    fn observed_change(&self, key_path: &str) {
        let page = &self.page;
        match key_path {
            "URL" => self.send(WebViewEvent::UrlChanged(page_url(page))),
            "title" => self.send(WebViewEvent::TitleChanged(page_title(page))),
            "canGoBack" | "canGoForward" => self.send(history_event(page)),
            _ => {}
        }
    }

    fn load_finished(&self, succeeded: bool) {
        log::info!("WKWebView: navigation completed (succeeded: {succeeded})");
        self.publish_location();
        self.send(WebViewEvent::LoadFinished { succeeded });
    }

    fn publish_location(&self) {
        let page = &self.page;
        self.send(WebViewEvent::UrlChanged(page_url(page)));
        self.send(WebViewEvent::TitleChanged(page_title(page)));
        self.send(history_event(page));
    }

    /// The receiver is dropped together with the view, and WebKit can still
    /// call the delegates until the page is gone, so a closed channel is
    /// expected.
    fn send(&self, event: WebViewEvent) {
        self.events.unbounded_send(event).ok();
    }
}

fn cancel_download(surface: Option<Rc<Inner>>, download: &WKDownload) {
    let url = unsafe { download.originalRequest() }
        .and_then(|request| request.URL())
        .and_then(|url| url.absoluteString())
        .map(|url| url.to_string())
        .unwrap_or_default();
    unsafe { download.cancel(None) };
    match surface {
        Some(surface) if surface.block_downloads => {
            log::info!("WKWebView: blocked a download of {url}");
            surface.send(WebViewEvent::DownloadBlocked { url });
        }
        _ => log::warn!("WKWebView: downloads are not supported yet; cancelled {url}"),
    }
}

/// Asks `window` to make `responder` its first responder on the next pass of
/// the run loop, in every common mode, so that a menu tracking its own loop
/// does not hold it up.
fn perform_make_first_responder(window: &NSWindow, responder: &NSView) {
    let modes = NSArray::from_slice(&[unsafe { NSRunLoopCommonModes }]);
    unsafe {
        let () = msg_send![
            window,
            performSelector: sel!(makeFirstResponder:),
            withObject: responder,
            afterDelay: 0.0f64,
            inModes: &*modes,
        ];
    }
}

fn add_document_start_script(
    user_content: &WKUserContentController,
    source: &str,
    mtm: MainThreadMarker,
) {
    let script = unsafe {
        WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
            WKUserScript::alloc(mtm),
            &NSString::from_str(source),
            WKUserScriptInjectionTime::AtDocumentStart,
            false,
        )
    };
    unsafe { user_content.addUserScript(&script) };
}

/// Turns off what an AI tab could reach the network with around the HTTP
/// CONNECT proxy, and returns whether WebRTC and WebTransport are known to be
/// off; see [`SUPPORTS_SCRIPT_EVAL`]. All of it is `WKPreferences` SPI
/// (`WKPreferencesPrivate.h`), so every selector is checked first.
fn switch_off_unproxied_transports(preferences: &WKPreferences) -> bool {
    // `RTCPeerConnection` is `EnabledBySetting=PeerConnectionEnabled`: with it
    // off, no frame of the page has the constructor at all.
    let peer_connections_off = preferences.respondsToSelector(sel!(_setPeerConnectionEnabled:))
        && preferences.respondsToSelector(sel!(_peerConnectionEnabled))
        && {
            unsafe {
                let () = msg_send![preferences, _setPeerConnectionEnabled: false];
            }
            peer_connections_are_off(preferences)
        };
    // Name lookups that skip the proxy, and could carry data out in the name;
    // gone altogether from WebKit on macOS 26.
    if preferences.respondsToSelector(sel!(_setDNSPrefetchingEnabled:)) {
        unsafe {
            let () = msg_send![preferences, _setDNSPrefetchingEnabled: false];
        }
    }
    peer_connections_off && switch_off_feature(preferences, "WebTransportEnabled")
}

fn peer_connections_are_off(preferences: &WKPreferences) -> bool {
    preferences.respondsToSelector(sel!(_peerConnectionEnabled)) && {
        let enabled: bool = unsafe { msg_send![preferences, _peerConnectionEnabled] };
        !enabled
    }
}

/// Switches off the WebKit feature flag `key`, and returns whether it is off
/// afterwards. A WebKit that does not list the flag does not have the feature.
fn switch_off_feature(preferences: &WKPreferences, key: &str) -> bool {
    let class = WKPreferences::class();
    if !class.responds_to(sel!(_features))
        || !preferences.respondsToSelector(sel!(_setEnabled:forFeature:))
        || !preferences.respondsToSelector(sel!(_isEnabledForFeature:))
    {
        return false;
    }
    let features: Option<Retained<NSArray<AnyObject>>> = unsafe { msg_send![class, _features] };
    let Some(features) = features else {
        return false;
    };
    for index in 0..features.count() {
        let feature = features.objectAtIndex(index);
        if !feature.class().responds_to(sel!(key)) {
            continue;
        }
        let feature_key: Option<Retained<NSString>> = unsafe { msg_send![&*feature, key] };
        if feature_key.is_some_and(|feature_key| feature_key.to_string() == key) {
            let enabled: bool = unsafe {
                let () = msg_send![preferences, _setEnabled: false, forFeature: &*feature];
                msg_send![preferences, _isEnabledForFeature: &*feature]
            };
            return !enabled;
        }
    }
    true
}

/// Whether `url` is a `data:` URL; schemes are case-insensitive.
fn is_data_url(url: &str) -> bool {
    url.trim_start()
        .get(..5)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("data:"))
}

/// The user profile is WebKit's default store; an AI tab gets its own
/// persistent store and always the egress proxy, or no page at all.
///
/// WebKit keeps both under `~/Library/WebKit/<bundle id>/`, so
/// [`WebViewOptions::user_data_folder`] is not used here.
fn website_data_store(
    options: &WebViewOptions,
    mtm: MainThreadMarker,
) -> Result<Retained<WKWebsiteDataStore>> {
    // Both per-store identifiers and per-store proxies are macOS 14 APIs.
    let per_store_settings = is_macos_at_least(14, 0);
    match options.profile {
        WebViewProfile::User => {
            let store = unsafe { WKWebsiteDataStore::defaultDataStore(mtm) };
            match (&options.proxy_endpoint, per_store_settings) {
                (Some(endpoint), true) => {
                    if let Err(error) = set_store_proxy(&store, Some(endpoint)) {
                        log::warn!(
                            "browser.use_egress_proxy is on but the proxy could not be applied \
                             ({error:#}); the built-in browser will connect directly"
                        );
                    }
                }
                (Some(_), false) => log::warn!(
                    "browser.use_egress_proxy is on but WebKit takes a proxy only on macOS 14 \
                     and later; the built-in browser will connect directly"
                ),
                // The default store is shared by every user tab, so a proxy an
                // earlier tab applied has to be taken off again.
                (None, true) => {
                    if let Err(error) = set_store_proxy(&store, None) {
                        log::warn!("WKWebView: could not clear the proxy: {error:#}");
                    }
                }
                (None, false) => {}
            }
            Ok(store)
        }
        WebViewProfile::Ai => {
            if !per_store_settings {
                bail!(
                    "AI tabs need macOS 14 or later: earlier WebKit cannot keep them in a \
                     separate profile behind the egress proxy"
                );
            }
            let endpoint = options
                .proxy_endpoint
                .as_deref()
                .context("AI tabs need the egress proxy, and it is not running")?;
            let identifier = NSUUID::from_bytes(AI_DATA_STORE_IDENTIFIER);
            let store = unsafe { WKWebsiteDataStore::dataStoreForIdentifier(&identifier, mtm) };
            set_store_proxy(&store, Some(endpoint))
                .context("could not route the AI tab through the egress proxy")?;
            Ok(store)
        }
    }
}

/// Points every connection of `store` at the HTTP CONNECT proxy `endpoint`
/// (`http://127.0.0.1:<port>`), or at none. Loopback is not excluded: the
/// egress proxy allows it, and an exclusion by name would also send a
/// `*.localhost` name that a DNS server resolves elsewhere around the proxy.
fn set_store_proxy(store: &WKWebsiteDataStore, endpoint: Option<&str>) -> Result<()> {
    if !store.respondsToSelector(sel!(setProxyConfigurations:)) {
        bail!("this WebKit cannot set a proxy on a data store");
    }
    let configurations: Retained<NSArray<AnyObject>> = match endpoint {
        Some(endpoint) => {
            let configuration = http_connect_proxy_configuration(endpoint)?;
            NSArray::from_retained_slice(&[configuration])
        }
        None => NSArray::new(),
    };
    unsafe {
        let () = msg_send![store, setProxyConfigurations: &*configurations];
    }
    Ok(())
}

#[link(name = "Network", kind = "framework")]
unsafe extern "C" {
    /// Returns a retained `nw_endpoint_t`; macOS 10.14 and later.
    fn nw_endpoint_create_host(hostname: *const c_char, port: *const c_char) -> *mut AnyObject;
}

/// `nw_proxy_config_create_http_connect`, which only exists on macOS 14 and
/// later; looked up at runtime so that the binary still loads on older ones.
type CreateHttpConnectProxy = unsafe extern "C" fn(
    proxy_endpoint: *mut AnyObject,
    proxy_tls_options: *mut AnyObject,
) -> *mut AnyObject;

/// `nw_proxy_config_set_failover_allowed`, macOS 14 and later.
type SetProxyFailoverAllowed = unsafe extern "C" fn(proxy_config: *mut AnyObject, allowed: bool);

fn http_connect_proxy_configuration(endpoint: &str) -> Result<Retained<AnyObject>> {
    let (host, port) = proxy_host_and_port(endpoint)?;
    let symbol = unsafe {
        libc::dlsym(
            libc::RTLD_DEFAULT,
            c"nw_proxy_config_create_http_connect".as_ptr(),
        )
    };
    if symbol.is_null() {
        bail!("this macOS has no nw_proxy_config_create_http_connect");
    }
    // SAFETY: the symbol has this signature in Network.framework's headers.
    let create_http_connect: CreateHttpConnectProxy = unsafe { std::mem::transmute(symbol) };
    let symbol = unsafe {
        libc::dlsym(
            libc::RTLD_DEFAULT,
            c"nw_proxy_config_set_failover_allowed".as_ptr(),
        )
    };
    if symbol.is_null() {
        bail!("this macOS has no nw_proxy_config_set_failover_allowed");
    }
    // SAFETY: as above.
    let set_failover_allowed: SetProxyFailoverAllowed = unsafe { std::mem::transmute(symbol) };

    let host = CString::new(host)?;
    let port = CString::new(port.to_string())?;
    // Network objects are Objective-C objects and are returned retained.
    let proxy_endpoint =
        unsafe { Retained::from_raw(nw_endpoint_create_host(host.as_ptr(), port.as_ptr())) }
            .with_context(|| format!("could not make a network endpoint of {endpoint}"))?;
    let configuration = unsafe {
        Retained::from_raw(create_http_connect(
            Retained::as_ptr(&proxy_endpoint).cast_mut(),
            ptr::null_mut(),
        ))
    }
    .with_context(|| format!("could not make a proxy configuration for {endpoint}"))?;
    // Already the default; spelled out because a connection the proxy refuses
    // must fail, never retry around it.
    unsafe { set_failover_allowed(Retained::as_ptr(&configuration).cast_mut(), false) };
    Ok(configuration)
}

fn proxy_host_and_port(endpoint: &str) -> Result<(String, u16)> {
    let url = Url::parse(endpoint).with_context(|| format!("invalid proxy endpoint {endpoint}"))?;
    let host = match url.host() {
        Some(Host::Domain(domain)) => domain.to_string(),
        Some(Host::Ipv4(address)) => address.to_string(),
        Some(Host::Ipv6(address)) => address.to_string(),
        None => bail!("the proxy endpoint {endpoint} has no host"),
    };
    let port = url
        .port_or_known_default()
        .with_context(|| format!("the proxy endpoint {endpoint} has no port"))?;
    Ok((host, port))
}

fn is_macos_at_least(major: isize, minor: isize) -> bool {
    NSProcessInfo::processInfo().isOperatingSystemAtLeastVersion(NSOperatingSystemVersion {
        majorVersion: major,
        minorVersion: minor,
        patchVersion: 0,
    })
}

fn request_url(navigation_action: &WKNavigationAction) -> Option<String> {
    let request = unsafe { navigation_action.request() };
    let url = request.URL()?;
    Some(url.absoluteString()?.to_string())
}

fn page_url(page: &WKWebView) -> String {
    unsafe { page.URL() }
        .and_then(|url| url.absoluteString())
        .map(|url| url.to_string())
        .unwrap_or_default()
}

fn page_title(page: &WKWebView) -> String {
    unsafe { page.title() }
        .map(|title| title.to_string())
        .unwrap_or_default()
}

fn history_event(page: &WKWebView) -> WebViewEvent {
    WebViewEvent::HistoryChanged {
        can_go_back: unsafe { page.canGoBack() },
        can_go_forward: unsafe { page.canGoForward() },
    }
}

/// `NSError`'s description, plus the message of a JavaScript exception, which
/// WebKit keeps in the user info rather than in the description.
fn describe_error(error: &NSError) -> String {
    let description = error.localizedDescription().to_string();
    let exception = error
        .userInfo()
        .objectForKey(&NSString::from_str("WKJavaScriptExceptionMessage"))
        .and_then(|message| message.downcast::<NSString>().ok())
        .map(|message| message.to_string());
    match exception {
        Some(exception) => format!("{description}: {exception}"),
        None => description,
    }
}

/// # Safety
///
/// `value` and `error` are what WebKit passed to the completion handler of
/// `evaluateJavaScript:completionHandler:`.
unsafe fn script_result(value: Option<&AnyObject>, error: Option<&NSError>) -> Result<String> {
    if let Some(error) = error {
        return Err(anyhow!("the script failed: {}", describe_error(error)));
    }
    let value = value.context("the script returned nothing")?;
    let text = value
        .downcast_ref::<NSString>()
        .context("the script did not return a string")?;
    Ok(text.to_string())
}

/// # Safety
///
/// `image` and `error` are what WebKit passed to the completion handler of
/// `takeSnapshotWithConfiguration:completionHandler:`.
unsafe fn snapshot_png(image: Option<&NSImage>, error: Option<&NSError>) -> Result<Vec<u8>> {
    if let Some(error) = error {
        return Err(anyhow!("the snapshot failed: {}", describe_error(error)));
    }
    let image = image.context("WebKit returned neither a snapshot nor an error")?;
    // The snapshot keeps its pixels in a CGImage-backed representation; a TIFF
    // round trip turns that into a bitmap at its full pixel density.
    let tiff = image
        .TIFFRepresentation()
        .context("the snapshot has no bitmap")?;
    let bitmap = NSBitmapImageRep::imageRepWithData(&tiff).context("the snapshot has no bitmap")?;
    let png = unsafe {
        bitmap.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }
    .context("could not encode the snapshot as PNG")?;
    Ok(png.to_vec())
}

/// Wraps `script` so that the page serialises its completion value:
/// `evaluateJavaScript` would otherwise convert it to Foundation types, and
/// reject `undefined` and whatever else it cannot convert. The script is an
/// expression (an IIFE for statements); a trailing `;` is tolerated, and the
/// line breaks keep a trailing `//` comment from swallowing the wrapper.
fn json_completion_script(script: &str) -> String {
    let expression = script.trim().trim_end_matches(';').trim_end();
    format!(
        "(() => {{ const value = (\n{expression}\n); \
         const json = JSON.stringify(value); \
         return json === undefined ? \"null\" : json; }})()"
    )
}

/// A rectangle in points, measured from the top-left corner of whatever it is
/// relative to (the GPUI view, or the container).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct SurfaceRect {
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
}

impl SurfaceRect {
    fn width(&self) -> f64 {
        (self.right - self.left).max(0.)
    }

    fn height(&self) -> f64 {
        (self.bottom - self.top).max(0.)
    }

    fn is_empty(&self) -> bool {
        self.right <= self.left || self.bottom <= self.top
    }

    fn contains(&self, x: f64, y: f64) -> bool {
        self.left <= x && x < self.right && self.top <= y && y < self.bottom
    }

    fn offset(&self, x: f64, y: f64) -> Self {
        Self {
            left: self.left + x,
            top: self.top + y,
            right: self.right + x,
            bottom: self.bottom + y,
        }
    }

    fn intersect(&self, other: &Self) -> Option<Self> {
        let intersection = Self {
            left: self.left.max(other.left),
            top: self.top.max(other.top),
            right: self.right.min(other.right),
            bottom: self.bottom.min(other.bottom),
        };
        (!intersection.is_empty()).then_some(intersection)
    }

    fn union(&self, other: &Self) -> Self {
        Self {
            left: self.left.min(other.left),
            top: self.top.min(other.top),
            right: self.right.max(other.right),
            bottom: self.bottom.max(other.bottom),
        }
    }

    /// The same rectangle in the coordinates of a view (or layer) of the given
    /// height: an unflipped one measures `y` from its bottom edge.
    fn to_view_rect(&self, view_height: f64, flipped: bool) -> NSRect {
        let y = if flipped {
            self.top
        } else {
            view_height - self.bottom
        };
        NSRect::new(
            NSPoint::new(self.left, y),
            NSSize::new(self.width(), self.height()),
        )
    }

    /// Clamped to half the shorter side: CoreGraphics refuses a larger radius,
    /// and it would no longer cover the overlay.
    fn corner_radius(&self) -> f64 {
        OVERLAY_CORNER_RADIUS
            .min(self.width() / 2.)
            .min(self.height() / 2.)
            .max(0.)
    }
}

/// Where the page goes and which parts of it gpui draws over, in points from
/// the GPUI view's top-left corner, snapped to device pixels so that the page
/// is not resampled. Overlapping holes are merged: the mask is filled even-odd,
/// so the page would show again where two holes overlap.
fn viewport_geometry(viewport: &Viewport) -> (SurfaceRect, Vec<SurfaceRect>) {
    let scale = f64::from(viewport.scale_factor.max(0.1));
    let snap = |value: f32| (f64::from(value) * scale).round() / scale;
    let left = snap(viewport.origin_x);
    let top = snap(viewport.origin_y);
    let bounds = SurfaceRect {
        left,
        top,
        right: left + snap(viewport.width).max(0.),
        bottom: top + snap(viewport.height).max(0.),
    };

    let holes = viewport
        .overlays
        .iter()
        .filter_map(|overlay| {
            let overlay = SurfaceRect {
                left: snap(overlay.left),
                top: snap(overlay.top),
                right: snap(overlay.right),
                bottom: snap(overlay.bottom),
            };
            let hole = bounds.intersect(&overlay)?;
            (!is_edge_sliver(&bounds, &overlay, &hole, EDGE_SLIVER_DEPTH)).then_some(hole)
        })
        .collect();

    (bounds, merge_overlapping(holes))
}

/// Whether `hole`, the part of `overlay` over `bounds`, is a band at most
/// `depth` deep along one edge of `bounds` that `overlay` carries on past.
fn is_edge_sliver(
    bounds: &SurfaceRect,
    overlay: &SurfaceRect,
    hole: &SurfaceRect,
    depth: f64,
) -> bool {
    (overlay.left < bounds.left && hole.right - bounds.left <= depth)
        || (overlay.right > bounds.right && bounds.right - hole.left <= depth)
        || (overlay.top < bounds.top && hole.bottom - bounds.top <= depth)
        || (overlay.bottom > bounds.bottom && bounds.bottom - hole.top <= depth)
}

/// Replaces every group of overlapping rectangles by their bounding box, until
/// no two overlap.
fn merge_overlapping(rects: Vec<SurfaceRect>) -> Vec<SurfaceRect> {
    let mut merged: Vec<SurfaceRect> = Vec::with_capacity(rects.len());
    for rect in rects {
        let mut current = rect;
        // A grown rectangle can overlap ones it missed before, so keep folding
        // until it stands alone.
        while let Some(position) = merged
            .iter()
            .position(|other| current.intersect(other).is_some())
        {
            current = current.union(&merged.swap_remove(position));
        }
        merged.push(current);
    }
    merged
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

    fn rect(left: f64, top: f64, right: f64, bottom: f64) -> SurfaceRect {
        SurfaceRect {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn bounds_and_holes_stay_in_points() {
        let (bounds, holes) = viewport_geometry(&viewport(2., vec![overlay(120., 60., 200., 90.)]));

        assert_eq!(bounds, rect(100., 50., 500., 350.));
        assert_eq!(holes, vec![rect(120., 60., 200., 90.)]);
    }

    #[test]
    fn fractional_points_snap_to_device_pixels() {
        let mut fractional = viewport(2., Vec::new());
        fractional.origin_x = 100.3;
        fractional.width = 199.9;

        let (bounds, _) = viewport_geometry(&fractional);

        assert_eq!(bounds.left, 100.5);
        assert_eq!(bounds.right, 100.5 + 200.);
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
    fn a_neighbouring_resize_handle_is_not_cut_but_a_popover_is() {
        let (_, holes) = viewport_geometry(&viewport(
            1.,
            vec![
                // A dock's resize handle reaching 3pt in across the left edge.
                overlay(97., 0., 103., 1000.),
                // A menu hanging over the same edge, far deeper.
                overlay(60., 100., 200., 200.),
            ],
        ));

        assert_eq!(holes, vec![rect(100., 100., 200., 200.)]);
    }

    #[test]
    fn overlapping_holes_merge_into_their_bounding_box() {
        let (_, holes) = viewport_geometry(&viewport(
            1.,
            vec![
                overlay(120., 60., 200., 120.),
                overlay(400., 200., 450., 250.),
                overlay(180., 100., 260., 160.),
            ],
        ));

        assert_eq!(
            holes,
            vec![rect(400., 200., 450., 250.), rect(120., 60., 260., 160.)]
        );
    }

    #[test]
    fn a_merged_hole_absorbs_what_it_grows_into() {
        let merged = merge_overlapping(vec![
            rect(0., 0., 10., 10.),
            rect(20., 0., 30., 10.),
            // Bridges the two above.
            rect(5., 0., 25., 5.),
        ]);

        assert_eq!(merged, vec![rect(0., 0., 30., 10.)]);
    }

    #[test]
    fn touching_holes_stay_apart() {
        let merged = merge_overlapping(vec![rect(0., 0., 10., 10.), rect(10., 0., 20., 10.)]);

        assert_eq!(merged.len(), 2, "{merged:?}");
    }

    #[test]
    fn rects_convert_to_unflipped_and_flipped_views() {
        let hole = rect(10., 20., 110., 70.);

        let unflipped = hole.to_view_rect(300., false);
        assert_eq!((unflipped.origin.x, unflipped.origin.y), (10., 230.));
        assert_eq!((unflipped.size.width, unflipped.size.height), (100., 50.));

        let flipped = hole.to_view_rect(300., true);
        assert_eq!((flipped.origin.x, flipped.origin.y), (10., 20.));
    }

    #[test]
    fn corner_radius_never_exceeds_half_a_side() {
        assert_eq!(
            rect(0., 0., 100., 100.).corner_radius(),
            OVERLAY_CORNER_RADIUS
        );
        assert_eq!(rect(0., 0., 100., 6.).corner_radius(), 3.);
        assert_eq!(rect(0., 0., 0., 6.).corner_radius(), 0.);
    }

    #[test]
    fn proxy_endpoints_split_into_host_and_port() {
        assert_eq!(
            proxy_host_and_port("http://127.0.0.1:43123").ok(),
            Some(("127.0.0.1".to_string(), 43123))
        );
        assert_eq!(
            proxy_host_and_port("http://[::1]:8080/").ok(),
            Some(("::1".to_string(), 8080))
        );
        assert_eq!(
            proxy_host_and_port("http://localhost").ok(),
            Some(("localhost".to_string(), 80)),
            "the scheme's port when none is given"
        );
        assert!(proxy_host_and_port("not a url").is_err());
    }

    #[test]
    fn holes_move_into_the_containers_space() {
        assert_eq!(
            rect(120., 60., 200., 90.).offset(-100., -50.),
            rect(20., 10., 100., 40.)
        );
    }

    #[test]
    fn scripts_are_wrapped_to_return_json() {
        let wrapped = json_completion_script("  document.title; ");
        assert!(wrapped.contains("(\ndocument.title\n)"), "{wrapped}");
        assert!(wrapped.starts_with("(() => {"), "{wrapped}");
        assert!(wrapped.ends_with("})()"), "{wrapped}");

        let commented = json_completion_script("1 + 1 // two");
        assert!(commented.contains("1 + 1 // two\n)"), "{commented}");

        assert!(wrapped.contains("JSON.stringify(value)"), "{wrapped}");
        assert!(
            wrapped.contains("json === undefined ? \"null\" : json"),
            "{wrapped}"
        );
        assert!(
            json_completion_script("x;;\n").contains("(\nx\n)"),
            "every trailing semicolon goes"
        );
    }

    #[test]
    fn data_urls_are_recognised_in_any_case() {
        assert!(is_data_url("data:text/html,<p>hi</p>"));
        assert!(is_data_url("DATA:text/html,x"));
        assert!(is_data_url(" Data:,"));
        assert!(!is_data_url("https://example.com/data:x"));
        assert!(!is_data_url("about:blank"));
        assert!(!is_data_url("data"));
        assert!(!is_data_url(""));
    }

    #[test]
    fn slivers_are_ignored_on_every_edge_but_deeper_overlays_are_cut() {
        let bounds = rect(100., 50., 500., 350.);
        let sliver = |overlay: SurfaceRect| {
            bounds
                .intersect(&overlay)
                .is_some_and(|hole| is_edge_sliver(&bounds, &overlay, &hole, EDGE_SLIVER_DEPTH))
        };

        assert!(sliver(rect(97., 100., 103., 200.)), "left");
        assert!(sliver(rect(497., 100., 503., 200.)), "right");
        assert!(sliver(rect(200., 47., 300., 53.)), "top");
        assert!(sliver(rect(200., 347., 300., 353.)), "bottom");

        assert!(!sliver(rect(90., 100., 110., 200.)), "left, too deep");
        assert!(!sliver(rect(490., 100., 510., 200.)), "right, too deep");
        assert!(!sliver(rect(200., 40., 300., 60.)), "top, too deep");
        assert!(!sliver(rect(200., 340., 300., 360.)), "bottom, too deep");
        // As shallow as a sliver, but wholly inside: a thin popover border.
        assert!(!sliver(rect(102., 100., 105., 200.)), "inside");
    }

    #[test]
    fn rects_contain_their_top_left_edges_only() {
        let hole = rect(10., 20., 30., 40.);

        assert!(hole.contains(10., 20.));
        assert!(hole.contains(29.9, 39.9));
        assert!(!hole.contains(30., 25.));
        assert!(!hole.contains(15., 40.));
        assert!(!hole.contains(9.9, 25.));
        assert!(!hole.contains(15., 19.9));
    }
}
