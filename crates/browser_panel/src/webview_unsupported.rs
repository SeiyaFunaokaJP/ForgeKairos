//! The backend for platforms without an embeddable web engine. `WebView::new`
//! always fails, so the view shows the "not supported" notice and the way out
//! to an external browser instead of a page.

use anyhow::{Result, anyhow};
use futures::channel::{mpsc::UnboundedSender, oneshot};
use gpui::{SharedString, Window};

pub(crate) use crate::webview_types::*;

/// Whether `browser_eval` may run arbitrary page scripts in AI tabs. A backend whose engine
/// cannot stop page scripts from opening WebRTC connections around the egress proxy says no.
pub(crate) const SUPPORTS_SCRIPT_EVAL: bool = true;

pub(crate) struct WebView;

impl WebView {
    pub(crate) fn new(
        _host: isize,
        _options: WebViewOptions,
        _events: UnboundedSender<WebViewEvent>,
    ) -> Result<Self> {
        Err(unsupported())
    }

    #[must_use]
    pub(crate) fn sync_viewport(&self, _viewport: Viewport, _view_focused: bool) -> bool {
        false
    }

    pub(crate) fn apply_deferred_clip(&self) {}

    pub(crate) fn hide(&self, _reason: &'static str) {}

    pub(crate) fn navigate(&self, _url: &str) {}

    pub(crate) fn go_back(&self) {}

    pub(crate) fn go_forward(&self) {}

    pub(crate) fn reload(&self) {}

    pub(crate) fn set_dark_mode(&self, _dark: bool) {}

    pub(crate) fn set_background_color(&self, _red: u8, _green: u8, _blue: u8) {}

    pub(crate) fn release_keyboard_focus(&self) {}

    pub(crate) fn focus_page(&self) {}

    pub(crate) fn execute_script(&self, _script: &str) -> oneshot::Receiver<Result<String>> {
        completed(Err(unsupported()))
    }

    pub(crate) fn capture_png(&self) -> oneshot::Receiver<Result<Vec<u8>>> {
        completed(Err(unsupported()))
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

fn unsupported() -> anyhow::Error {
    anyhow!("the built-in browser has no web engine on this platform")
}

/// There is nothing to host the page in, but handing out a handle lets
/// `WebView::new` run and report the platform as unsupported; `None` would
/// make the view wait for a handle forever.
pub(crate) fn host_window_handle(_window: &Window) -> Option<isize> {
    Some(0)
}

pub(crate) fn host_uses_direct_composition(_host: isize) -> bool {
    false
}

pub(crate) fn runtime_missing_message() -> SharedString {
    i18n::t!("The built-in browser isn't supported on this platform.").into()
}
