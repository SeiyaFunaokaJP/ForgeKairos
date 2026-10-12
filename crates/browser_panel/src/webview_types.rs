//! The data every webview backend (`webview_windows`, `webview_macos`,
//! `webview_linux`, `webview_unsupported`) exchanges with [`crate::BrowserView`].
//!
//! Each backend re-exports these, so the rest of the crate only ever names
//! `crate::webview::*`. Keeping one definition means a backend cannot drift
//! from what the view builds and matches on.

use std::{fmt, path::PathBuf, rc::Rc};

use anyhow::Result;
use browser_control::{KeyModifier, MouseButton};
use futures::channel::oneshot;

/// Which browser profile (cookies, storage, cache) a surface uses. AI tabs
/// never share the user's profile, so nothing the user signed in to is
/// reachable by an AI agent (ADR-F89).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WebViewProfile {
    User,
    // TEMPORARY STUB (core): constructed once AI tabs exist.
    #[allow(dead_code)]
    Ai,
}

/// The answer of [`WebViewOptions::navigation_filter`] for one top-level
/// navigation. `Deny` carries the reason reported back to whoever navigated.
#[derive(Clone, Debug, PartialEq, Eq)]
// TEMPORARY STUB (core): constructed by the AI tab's navigation filter.
#[allow(dead_code)]
pub(crate) enum NavigationDecision {
    Allow,
    Deny(String),
}

/// Everything a backend needs to create a surface.
#[derive(Clone)]
pub(crate) struct WebViewOptions {
    pub(crate) profile: WebViewProfile,
    pub(crate) user_data_folder: PathBuf,
    /// `http://127.0.0.1:<port>`; `None` connects directly.
    pub(crate) proxy_endpoint: Option<String>,
    /// Runs before the page's own scripts in every top-level document (frames
    /// are best effort).
    pub(crate) document_start_script: Option<String>,
    /// Asked before every top-level navigation; a denied one is cancelled and
    /// reported as [`WebViewEvent::NavigationBlocked`].
    pub(crate) navigation_filter: Option<Rc<dyn Fn(&str) -> NavigationDecision>>,
    /// Cancels every download and reports it as [`WebViewEvent::DownloadBlocked`].
    pub(crate) block_downloads: bool,
}

impl WebViewOptions {
    /// Whether anything beyond the plain user profile is asked for.
    pub(crate) fn uses_ai_tab_features(&self) -> bool {
        self.profile == WebViewProfile::Ai
            || self.document_start_script.is_some()
            || self.navigation_filter.is_some()
            || self.block_downloads
    }
}

/// Trusted (browser-level, `isTrusted == true`) input. Coordinates are CSS
/// pixels relative to the viewport's top-left corner.
#[derive(Clone, Debug, PartialEq)]
// TEMPORARY STUB (core): constructed by the browser automation.
#[allow(dead_code)]
pub(crate) enum NativeInput {
    MouseMove {
        x: f64,
        y: f64,
    },
    MouseClick {
        x: f64,
        y: f64,
        button: MouseButton,
        click_count: u8,
        modifiers: Vec<KeyModifier>,
    },
    Wheel {
        x: f64,
        y: f64,
        delta_x: f64,
        delta_y: f64,
    },
    /// `key` is a `KeyboardEvent.key` value, `code` a `KeyboardEvent.code` one.
    Key {
        key: String,
        code: String,
        modifiers: Vec<KeyModifier>,
    },
    InsertText(String),
}

/// The error of `WebView::dispatch_input` on a backend without trusted input;
/// the caller downcasts to it and synthesises the input in page JavaScript
/// instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NativeInputUnsupported;

impl fmt::Display for NativeInputUnsupported {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("this browser backend has no native input")
    }
}

impl std::error::Error for NativeInputUnsupported {}

/// Everything a backend needs to tell the view about. Backends call back
/// without a GPUI context, so these travel over an unbounded channel that the
/// view drains from a GPUI task.
pub(crate) enum WebViewEvent {
    Ready,
    /// Creation failed; the payload says why, for the log.
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
    /// Whether the page currently owns the native keyboard focus. GPUI's own
    /// focus state cannot see this, so the panel renders a hint from it.
    PageFocusChanged(bool),
    /// A `target="_blank"` navigation; the panel hosts a single view, so it is
    /// re-targeted at the current view instead of spawning a browser window.
    OpenInPlace(String),
    /// A top-level navigation that [`WebViewOptions::navigation_filter`] denied.
    // TEMPORARY STUB (win/mac/linux): sent once the backends apply the filter.
    #[allow(dead_code)]
    NavigationBlocked {
        url: String,
        reason: String,
    },
    /// A download cancelled because of [`WebViewOptions::block_downloads`].
    // TEMPORARY STUB (win/mac/linux): sent once the backends block downloads.
    #[allow(dead_code)]
    DownloadBlocked {
        url: String,
    },
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
    /// Whether the page stays on screen but takes no mouse input, because gpui
    /// draws something over it that `overlays` cannot describe: a workspace
    /// modal and the full-window backdrop that dismisses it.
    pub(crate) input_blocked: bool,
    /// What gpui paints on top of the element tree this frame (tooltips,
    /// context menus, popovers), in the same logical pixels and origin as the
    /// viewport itself. Whatever of it lands on the surface is cut out of the
    /// native surface; see `WebView::sync_viewport`.
    pub(crate) overlays: Vec<OverlayRect>,
}

/// A gpui overlay rectangle in logical pixels, relative to the host window's
/// client origin - the same space [`Viewport`] uses.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct OverlayRect {
    pub(crate) left: f32,
    pub(crate) top: f32,
    pub(crate) right: f32,
    pub(crate) bottom: f32,
}

/// A receiver that already holds `value`, for the asynchronous `WebView`
/// methods when the answer is known up front.
pub(crate) fn completed<T>(value: Result<T>) -> oneshot::Receiver<Result<T>> {
    let (sender, receiver) = oneshot::channel();
    // Cannot fail: the receiver is alive until it is returned below.
    sender.send(value).ok();
    receiver
}
