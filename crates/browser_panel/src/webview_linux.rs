//! WebKitGTK host for [`crate::BrowserView`] on Linux and FreeBSD (ADR-F89).
//!
//! GTK 3 and WebKitGTK 4.1 are opened at runtime ([`ffi`]), so the editor never
//! links against them and still starts where they are missing; the panel then
//! shows [`runtime_missing_message`] and the way out to an external browser.
//!
//! GTK runs on gpui's foreground thread, which is the main thread on Linux.
//! Nothing drives GLib's main context there on its own, so the view runs
//! [`pump_main_loop`] from a gpui task for as long as any surface exists.
//!
//! On X11 the page is embedded: a borderless GTK popup window holding the
//! `WebKitWebView` is reparented under gpui's X11 window and placed like the
//! WebView2 child window on Windows. Wayland has no way to put another
//! client's surface inside a window, so there the page lives in a separate,
//! normal GTK window ("detached") and the panel shows a placeholder.

use std::{
    cell::{Cell, OnceCell, RefCell},
    collections::HashMap,
    ffi::{CStr, CString, c_char, c_int, c_uint, c_ulong},
    os::unix::ffi::OsStrExt as _,
    path::{Path, PathBuf},
    ptr,
    rc::{Rc, Weak},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow, bail};
use futures::channel::{mpsc::UnboundedSender, oneshot};
use gpui::{SharedString, Window};

pub(crate) use crate::webview_types::*;

#[path = "webview_linux_ffi.rs"]
mod ffi;

use ffi::{
    CAIRO_STATUS_SUCCESS, CAIRO_STATUS_WRITE_ERROR, CairoRectangleInt, FALSE, Functions,
    GDK_KEY_ESCAPE, GTK_WINDOW_POPUP, GTK_WINDOW_TOPLEVEL, Gboolean, Gcallback, GdkEventKeyPrefix,
    GdkRgba, Gerror, Gpointer, TRUE, WEBKIT_COOKIE_PERSISTENT_STORAGE_SQLITE,
    WEBKIT_LOAD_COMMITTED, WEBKIT_LOAD_FINISHED, WEBKIT_LOAD_STARTED,
    WEBKIT_NETWORK_PROXY_MODE_CUSTOM, WEBKIT_POLICY_DECISION_TYPE_NAVIGATION_ACTION,
    WEBKIT_POLICY_DECISION_TYPE_NEW_WINDOW_ACTION, WEBKIT_POLICY_DECISION_TYPE_RESPONSE,
    WEBKIT_SNAPSHOT_OPTIONS_NONE, WEBKIT_SNAPSHOT_REGION_VISIBLE,
    WEBKIT_USER_CONTENT_INJECT_ALL_FRAMES, WEBKIT_USER_SCRIPT_INJECT_AT_DOCUMENT_START,
    X_CURRENT_TIME, X_REVERT_TO_PARENT, XWindow,
};

macro_rules! gcallback {
    ($handler:expr, $signature:ty) => {
        // SAFETY: GObject calls a handler back with the signature of the signal
        // it is connected to, which `$signature` spells out.
        unsafe { std::mem::transmute::<$signature, Gcallback>($handler) }
    };
}

/// The host handle [`host_window_handle`] reports for a gpui window that
/// cannot contain another client's surface (Wayland). [`WebView::new`] then
/// shows the page in a window of its own instead of failing, so the panel
/// still gets a working browser.
pub(crate) const DETACHED_HOST: isize = -1;

/// Page scripts in an AI tab cannot reach the network around the proxy:
/// WebRTC, the only UDP a page can open, is turned off in the engine
/// (`enable-webrtc`), so `browser_eval` may run arbitrary scripts.
pub(crate) const SUPPORTS_SCRIPT_EVAL: bool = true;

/// How long the view waits between two [`pump_main_loop`] calls. Short enough
/// that typing into a page does not lag, long enough that an idle browser costs
/// next to nothing.
pub(crate) const MAIN_LOOP_PUMP_INTERVAL: Duration = Duration::from_millis(8);

/// The most one [`pump_main_loop`] call spends dispatching GLib sources, so that
/// a page that keeps WebKit busy cannot starve gpui's own frame.
const MAIN_LOOP_BUDGET: Duration = Duration::from_millis(4);

/// Hosts that skip the proxy: literal loopback addresses, which cannot lead
/// anywhere but this machine. The name `localhost` is not listed because GLib
/// would bypass every `*.localhost` with it, which the egress policy does not
/// treat as loopback; it goes through the proxy, which allows it (ADR-F89).
const PROXY_BYPASS_HOSTS: &[&CStr] = &[c"127.0.0.0/8", c"::1"];

/// See `OVERLAY_CORNER_RADIUS` in `webview_windows`; the same gpui overlays are
/// cut out here.
const OVERLAY_CORNER_RADIUS: f64 = 8.;

/// See `EDGE_SLIVER_DEPTH` in `webview_windows`: a dock's resize handle reaches
/// a few pixels into the surface without painting anything there.
const EDGE_SLIVER_DEPTH: f32 = 4.;

pub(crate) fn host_window_handle(window: &Window) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    // `gpui::Window` has its own inherent `window_handle`, so the trait method
    // has to be named explicitly.
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::Xcb(handle) => Some(handle.window.get() as isize),
        RawWindowHandle::Xlib(handle) => Some(handle.window as isize),
        RawWindowHandle::Wayland(_) => Some(DETACHED_HOST),
        _ => None,
    }
}

/// gpui never composites anything in front of an X11 child window, so the
/// panel has to hide the page for drags and modals.
pub(crate) fn host_uses_direct_composition(_host: isize) -> bool {
    false
}

pub(crate) fn runtime_missing_message() -> SharedString {
    i18n::t!(
        "WebKitGTK 4.1 wasn't found. Install libwebkit2gtk-4.1-0 (Debian/Ubuntu) or webkit2gtk4.1 (Fedora)."
    )
    .into()
}

/// Claims the job of driving GLib's main context. Returns `true` to exactly
/// one caller until [`pump_main_loop`] gives the claim up again; that caller
/// runs `pump_main_loop` every [`MAIN_LOOP_PUMP_INTERVAL`] while it returns
/// `true`.
pub(crate) fn claim_main_loop_pump() -> bool {
    !PUMP_CLAIMED.replace(true)
}

/// Dispatches whatever GLib work is ready - GTK's X11 / Wayland events, WebKit's
/// IPC replies, timers - within [`MAIN_LOOP_BUDGET`]. Returns `false`, giving
/// up the claim, once no surface is left.
pub(crate) fn pump_main_loop() -> bool {
    let runtime = match loaded_runtime() {
        Some(runtime) if LIVE_SURFACES.get() > 0 || PENDING_REPLIES.get() > 0 => runtime,
        _ => {
            PUMP_CLAIMED.set(false);
            return false;
        }
    };
    let started = Instant::now();
    runtime.with_x_errors_ignored(|| {
        // SAFETY: on the thread GTK was initialised on; `NULL` is the default
        // context.
        while unsafe { (runtime.functions.g_main_context_iteration)(ptr::null_mut(), FALSE) }
            != FALSE
        {
            if started.elapsed() >= MAIN_LOOP_BUDGET {
                break;
            }
        }
    });
    true
}

struct Runtime {
    functions: Functions,
    /// GDK's default `GdkDisplay`.
    display: Gpointer,
    /// The Xlib `Display*` GDK talks to, when GTK runs on X11. Null on
    /// Wayland, or when Xlib or GDK's X11 backend is missing.
    x_display: Gpointer,
}

impl Runtime {
    /// Runs code that sends X requests which can fail through no fault of
    /// ours: focusing an unmapped window, or anything GTK does to the page's
    /// window after gpui destroyed its own window and, with it, every child.
    /// GDK answers an untrapped X error by ending the process - the whole
    /// editor - so everything that reaches GTK goes through here.
    fn with_x_errors_ignored<R>(&self, request: impl FnOnce() -> R) -> R {
        let traps = self
            .functions
            .gdk_x11_display_error_trap_push
            .zip(self.functions.gdk_x11_display_error_trap_pop_ignored)
            .filter(|_| !self.x_display.is_null());
        if let Some((push, _)) = traps {
            // SAFETY: with `x_display` set, `display` is an X11 `GdkDisplay`.
            unsafe { push(self.display) };
        }
        let result = request();
        if let Some((_, pop)) = traps {
            unsafe { pop(self.display) };
        }
        result
    }
}

/// One WebKit network session per user data folder. WebKit allows a data
/// directory to be used by one `WebKitWebsiteDataManager` only, so every
/// surface of a profile shares one context, as WebView2 surfaces share one
/// environment.
#[derive(Clone, Copy)]
struct ProfileContext {
    context: Gpointer,
    data_manager: Gpointer,
    /// The proxy is set per context, so a folder serves one profile only:
    /// otherwise creating a user surface would change the AI surfaces' proxy.
    profile: WebViewProfile,
}

thread_local! {
    /// GTK is single-threaded, so everything that touches it is thread local.
    /// Leaked: the libraries stay loaded for the life of the process anyway.
    static RUNTIME: OnceCell<Result<&'static Runtime, String>> = const { OnceCell::new() };
    static PROFILE_CONTEXTS: RefCell<HashMap<PathBuf, ProfileContext>> = RefCell::new(HashMap::new());
    static LIVE_SURFACES: Cell<usize> = const { Cell::new(0) };
    static PENDING_REPLIES: Cell<usize> = const { Cell::new(0) };
    static PUMP_CLAIMED: Cell<bool> = const { Cell::new(false) };
}

/// Loads and initialises GTK on first use. Which display backend GTK may use is
/// decided then, from the first host: embedding needs GTK on the same X server
/// as gpui, so an X11 host restricts GTK to X11.
fn runtime(host: isize) -> Result<&'static Runtime> {
    RUNTIME
        .with(|runtime| {
            runtime
                .get_or_init(|| {
                    load_runtime(host != DETACHED_HOST)
                        .map(|runtime| &*Box::leak(Box::new(runtime)))
                })
                .clone()
        })
        .map_err(|message| anyhow!(message))
}

fn loaded_runtime() -> Option<&'static Runtime> {
    RUNTIME.with(|runtime| {
        runtime
            .get()
            .and_then(|runtime| runtime.as_ref().ok().copied())
    })
}

fn load_runtime(embed_into_x11: bool) -> Result<Runtime, String> {
    let functions = Functions::load()?;
    let backends = if embed_into_x11 {
        c"x11"
    } else {
        c"wayland,x11"
    };
    // SAFETY: before `gtk_init_check`, on the thread that will run GTK.
    let initialised = unsafe {
        (functions.gdk_set_allowed_backends)(backends.as_ptr());
        // gpui owns the process locale; GTK would otherwise reset it to the
        // environment's, changing how every number in the editor is printed.
        (functions.gtk_disable_setlocale)();
        (functions.gtk_init_check)(ptr::null_mut(), ptr::null_mut())
    };
    if initialised == FALSE {
        return Err("GTK could not open the display".to_string());
    }

    let display = unsafe { (functions.gdk_display_get_default)() };
    let x_display = match (
        functions.gdk_x11_display_get_type,
        functions.gdk_x11_display_get_xdisplay,
        functions.gdk_x11_window_get_xid,
        functions.XReparentWindow,
        functions.XSetInputFocus,
        functions.XFlush,
        functions.gdk_x11_display_error_trap_push,
        functions.gdk_x11_display_error_trap_pop_ignored,
    ) {
        (
            Some(x11_display_type),
            Some(get_xdisplay),
            Some(_),
            Some(_),
            Some(_),
            Some(_),
            Some(_),
            Some(_),
        ) if !display.is_null()
            && unsafe { (functions.g_type_check_instance_is_a)(display, x11_display_type()) }
                != FALSE =>
        unsafe { get_xdisplay(display) },
        _ => ptr::null_mut(),
    };
    log::info!(
        "WebKitGTK: GTK is initialised ({})",
        if x_display.is_null() {
            "pages open in separate windows"
        } else {
            "pages are embedded into X11 windows"
        }
    );
    Ok(Runtime {
        functions,
        display,
        x_display,
    })
}

fn profile_context(
    runtime: &Runtime,
    folder: &Path,
    profile: WebViewProfile,
) -> Result<ProfileContext> {
    if let Some(context) = PROFILE_CONTEXTS.with(|contexts| contexts.borrow().get(folder).copied())
    {
        if context.profile != profile {
            bail!(
                "{} already holds the {:?} profile and cannot also hold the {profile:?} one",
                folder.display(),
                context.profile
            );
        }
        return Ok(context);
    }

    std::fs::create_dir_all(folder)
        .with_context(|| format!("failed to create {}", folder.display()))?;
    let data_directory = path_to_c_string(&folder.join("data"))?;
    let cache_directory = path_to_c_string(&folder.join("cache"))?;
    let cookie_file = path_to_c_string(&folder.join("cookies.sqlite"))?;
    let functions = &runtime.functions;
    // SAFETY: the property list is NULL-terminated and every value is a string,
    // as both properties are.
    let data_manager = unsafe {
        (functions.webkit_website_data_manager_new)(
            c"base-data-directory".as_ptr(),
            data_directory.as_ptr(),
            c"base-cache-directory".as_ptr(),
            cache_directory.as_ptr(),
            ptr::null::<c_char>(),
        )
    };
    if data_manager.is_null() {
        bail!("WebKitGTK could not create a website data manager");
    }
    let context = unsafe {
        let cookie_manager =
            (functions.webkit_website_data_manager_get_cookie_manager)(data_manager);
        // Without this, cookies (and with them every sign-in) would only last
        // as long as the process.
        (functions.webkit_cookie_manager_set_persistent_storage)(
            cookie_manager,
            cookie_file.as_ptr(),
            WEBKIT_COOKIE_PERSISTENT_STORAGE_SQLITE,
        );
        (functions.webkit_web_context_new_with_website_data_manager)(data_manager)
    };
    if context.is_null() {
        bail!("WebKitGTK could not create a web context");
    }
    log::info!(
        "WebKitGTK: created the web context for {}",
        folder.display()
    );
    let context = ProfileContext {
        context,
        data_manager,
        profile,
    };
    PROFILE_CONTEXTS.with(|contexts| {
        contexts.borrow_mut().insert(folder.to_path_buf(), context);
    });
    Ok(context)
}

/// An AI tab connects through the egress proxy or not at all. WebKit treats an
/// empty or unknown proxy address as no proxy, so anything but an `http://`
/// endpoint is refused rather than handed over.
fn check_ai_proxy(endpoint: Option<&str>) -> Result<()> {
    match endpoint {
        None => bail!("an AI tab needs the egress proxy, and none is running"),
        Some(endpoint)
            if endpoint
                .strip_prefix("http://")
                .is_some_and(|authority| !authority.is_empty()) =>
        {
            Ok(())
        }
        Some(endpoint) => bail!("{endpoint:?} is not an address of the egress proxy"),
    }
}

/// Routes the profile through `endpoint`, or back to a direct connection.
/// Applied on every surface creation rather than once, so that a change of
/// `browser.use_egress_proxy` reaches the next surface without a restart.
fn apply_proxy(runtime: &Runtime, data_manager: Gpointer, endpoint: Option<&str>) -> Result<()> {
    let functions = &runtime.functions;
    let (Some(set_proxy), Some(new_settings), Some(free_settings)) = (
        functions.webkit_website_data_manager_set_network_proxy_settings,
        functions.webkit_network_proxy_settings_new,
        functions.webkit_network_proxy_settings_free,
    ) else {
        if endpoint.is_some() {
            bail!("this WebKitGTK cannot route pages through a proxy (2.32 or newer is needed)");
        }
        return Ok(());
    };
    let Some(endpoint) = endpoint else {
        // SAFETY: the default mode takes no settings.
        unsafe { set_proxy(data_manager, 0, ptr::null_mut()) };
        return Ok(());
    };

    let endpoint = CString::new(endpoint).context("the proxy address contains a NUL character")?;
    let mut bypass_hosts = PROXY_BYPASS_HOSTS
        .iter()
        .map(|host| host.as_ptr())
        .collect::<Vec<_>>();
    bypass_hosts.push(ptr::null());
    // SAFETY: both strings outlive the call, which copies them; the host list
    // is NULL-terminated.
    unsafe {
        let settings = new_settings(endpoint.as_ptr(), bypass_hosts.as_ptr());
        if settings.is_null() {
            bail!("WebKitGTK rejected the proxy address");
        }
        set_proxy(data_manager, WEBKIT_NETWORK_PROXY_MODE_CUSTOM, settings);
        free_settings(settings);
    }
    Ok(())
}

pub(crate) struct WebView {
    inner: Rc<Inner>,
}

struct Inner {
    runtime: &'static Runtime,
    /// `None` in detached mode.
    embedding: Option<Embedding>,
    /// The `GtkWindow` holding [`Inner::view`]: a popup reparented under the
    /// host when embedded, a normal window when detached.
    window: Gpointer,
    /// The `WebKitWebView`. This struct holds a reference of its own, so the
    /// pointer stays valid until `Drop` lets go of it.
    view: Gpointer,
    context: Gpointer,
    events: UnboundedSender<WebViewEvent>,
    navigation_filter: Option<Rc<dyn Fn(&str) -> NavigationDecision>>,
    block_downloads: bool,
    /// AI tabs: `window.print()` is ignored (GTK's print dialog runs a nested
    /// main loop that would stall the editor until someone closes it), file
    /// choosers never open (a trusted click must not let the AI pick a local
    /// file) and every permission request is denied.
    ai_profile: bool,
    /// Every signal handler this surface connected, with its instance (of
    /// which it holds a reference); disconnected on drop, which also frees
    /// their data. The ones on the shared web context would otherwise outlive
    /// the surface.
    handlers: RefCell<Vec<(Gpointer, c_ulong)>>,
    state: RefCell<State>,
}

#[derive(Clone, Copy)]
struct Embedding {
    x_display: Gpointer,
    host: XWindow,
    /// The X window of [`Inner::window`].
    surface: XWindow,
}

#[derive(Default)]
struct State {
    bounds: DeviceRect,
    holes: Vec<DeviceRect>,
    scale: f64,
    visible: bool,
    hidden_reason: Option<&'static str>,
    input_blocked: bool,
    view_focused: bool,
    applied_bounds: Option<DeviceRect>,
    applied_scale: Option<f64>,
    applied_visible: Option<bool>,
    /// The `(bounds, holes, input_blocked)` the window's shapes were built
    /// from; the holes are window-relative, so a resize invalidates them.
    applied_shape: Option<(DeviceRect, Vec<DeviceRect>, bool)>,
    page_has_focus: bool,
    /// A `focus_page` that came while the page took no input; see the same
    /// field in `webview_windows`.
    pending_focus: bool,
    /// Set by `load-failed`, read by the `load-changed` that ends that load.
    load_failed: bool,
    /// Detached mode: the window has been shown once, after which only the
    /// user moves, resizes or closes it.
    detached_shown: bool,
}

impl WebView {
    /// Creates the surface. Unlike WebView2, WebKitGTK creates it
    /// synchronously, so [`WebViewEvent::Ready`] is queued before this returns.
    pub(crate) fn new(
        host: isize,
        options: WebViewOptions,
        events: UnboundedSender<WebViewEvent>,
    ) -> Result<Self> {
        let ai_profile = options.profile == WebViewProfile::Ai;
        if ai_profile {
            check_ai_proxy(options.proxy_endpoint.as_deref())?;
        }

        let runtime = runtime(host)?;
        let functions = &runtime.functions;

        let embedding_display =
            (host != DETACHED_HOST && !runtime.x_display.is_null()).then_some(runtime.x_display);
        if host != DETACHED_HOST && embedding_display.is_none() {
            log::warn!(
                "WebKitGTK: GTK is not on gpui's X11 display, so the page opens in a separate window"
            );
        }

        let profile = profile_context(runtime, &options.user_data_folder, options.profile)?;
        if let Err(error) = apply_proxy(
            runtime,
            profile.data_manager,
            options.proxy_endpoint.as_deref(),
        ) {
            // Fail closed for AI tabs; the user's own tabs keep today's
            // behaviour of connecting directly, as WebView2 does when no
            // proxy is running.
            if ai_profile {
                return Err(error.context("the AI tab cannot be routed through the egress proxy"));
            }
            log::warn!("WebKitGTK: {error:#}; the built-in browser will connect directly");
        }

        let document_start_script = options
            .document_start_script
            .as_deref()
            .map(CString::new)
            .transpose()
            .context("the document start script contains a NUL character")?;

        // SAFETY: plain object construction on the GTK thread; every
        // reference taken here is handed to the view or released below.
        let view = unsafe {
            let settings = (functions.webkit_settings_new)();
            set_boolean_property(functions, settings, c"enable-developer-extras", true);
            if ai_profile {
                // WebRTC's UDP would bypass the HTTP proxy. Both are already
                // off by default; turning them off explicitly keeps it that way
                // should a distribution change the default.
                set_boolean_property(functions, settings, c"enable-webrtc", false);
                set_boolean_property(functions, settings, c"enable-media-stream", false);
                // A prefetch would send page-chosen names to the system
                // resolver, outside the proxy. WebKitGTK 2.48 made the setting
                // a no-op, so this only helps on older versions.
                set_boolean_property(functions, settings, c"enable-dns-prefetching", false);
            }

            let content_manager = (functions.webkit_user_content_manager_new)();
            if let Some(source) = &document_start_script {
                let script = (functions.webkit_user_script_new)(
                    source.as_ptr(),
                    WEBKIT_USER_CONTENT_INJECT_ALL_FRAMES,
                    WEBKIT_USER_SCRIPT_INJECT_AT_DOCUMENT_START,
                    ptr::null(),
                    ptr::null(),
                );
                (functions.webkit_user_content_manager_add_script)(content_manager, script);
                (functions.webkit_user_script_unref)(script);
            }

            let view = (functions.g_object_new)(
                (functions.webkit_web_view_get_type)(),
                c"web-context".as_ptr(),
                profile.context,
                c"user-content-manager".as_ptr(),
                content_manager,
                c"settings".as_ptr(),
                settings,
                ptr::null::<c_char>(),
            );
            (functions.g_object_unref)(settings);
            (functions.g_object_unref)(content_manager);
            if view.is_null() {
                bail!("WebKitGTK could not create a web view");
            }
            // Owned by this struct from here on, independently of the window
            // it is added to.
            (functions.g_object_ref_sink)(view)
        };

        let (window, embedding) = match embedding_display {
            Some(x_display) => match runtime
                .with_x_errors_ignored(|| create_embedded_window(runtime, view, x_display, host))
            {
                Ok((window, embedding)) => (window, Some(embedding)),
                Err(error) => {
                    unsafe { (functions.g_object_unref)(view) };
                    return Err(error);
                }
            },
            None => (create_detached_window(runtime, view), None),
        };
        // GTK destroys a top-level whose X window goes away, which happens to
        // the embedded one when gpui destroys its window first; this reference
        // keeps the pointer valid until `Drop`.
        unsafe { (functions.g_object_ref)(window) };

        let inner = Rc::new(Inner {
            runtime,
            embedding,
            window,
            view,
            context: profile.context,
            events,
            navigation_filter: options.navigation_filter,
            block_downloads: options.block_downloads,
            ai_profile,
            handlers: RefCell::new(Vec::new()),
            state: RefCell::new(State {
                scale: 1.0,
                hidden_reason: Some("the panel has not been painted yet"),
                ..Default::default()
            }),
        });
        inner.connect_signals();
        LIVE_SURFACES.set(LIVE_SURFACES.get() + 1);

        log::info!(
            "WebKitGTK: created a {} surface ({:?} profile, proxy {})",
            if embedding.is_some() {
                "embedded"
            } else {
                "detached"
            },
            options.profile,
            options.proxy_endpoint.as_deref().unwrap_or("none"),
        );
        inner.send(WebViewEvent::Ready);
        Ok(Self { inner })
    }

    /// Places the page like `webview_windows` does: physical pixels relative
    /// to the host's client area, with gpui's overlays cut out of the window's
    /// shape (and its input shape, so they stay clickable). Returns `true`
    /// when cutting new holes was held back until the frame that paints the
    /// overlays is on screen; see the Windows backend for why.
    ///
    /// A detached window only takes its initial size from here and is shown
    /// the first time the panel wants the page visible; after that it belongs
    /// to the user, and the panel shows a placeholder instead.
    #[must_use]
    pub(crate) fn sync_viewport(&self, viewport: Viewport, view_focused: bool) -> bool {
        if self.inner.embedding.is_none() {
            self.inner.sync_detached(&viewport);
            return false;
        }

        let (bounds, holes) = viewport_geometry(&viewport);
        let mut hidden_reason = viewport.hidden_reason;
        if hidden_reason.is_none() && bounds.is_empty() {
            hidden_reason = Some("the content rect is empty");
        }
        {
            let mut state = self.inner.state.borrow_mut();
            state.bounds = bounds;
            state.holes = holes;
            state.scale = viewport.scale_factor.max(0.1) as f64;
            state.visible = hidden_reason.is_none();
            state.hidden_reason = hidden_reason;
            state.input_blocked = viewport.input_blocked;
            state.view_focused = view_focused;
            if hidden_reason.is_some() {
                state.pending_focus = false;
            }
        }
        let inner = &self.inner;
        inner
            .runtime
            .with_x_errors_ignored(|| inner.apply_viewport(true))
    }

    pub(crate) fn apply_deferred_clip(&self) {
        let inner = &self.inner;
        inner
            .runtime
            .with_x_errors_ignored(|| inner.apply_shape(false));
    }

    pub(crate) fn hide(&self, reason: &'static str) {
        if self.inner.embedding.is_none() {
            return;
        }
        {
            let mut state = self.inner.state.borrow_mut();
            state.visible = false;
            state.hidden_reason = Some(reason);
            state.pending_focus = false;
        }
        let inner = &self.inner;
        inner
            .runtime
            .with_x_errors_ignored(|| inner.apply_viewport(false));
    }

    pub(crate) fn navigate(&self, url: &str) {
        let Ok(url) = CString::new(url) else {
            log::error!("WebKitGTK: refusing to navigate to a URL with a NUL character");
            return;
        };
        log::info!("WebKitGTK: navigating to {}", url.to_string_lossy());
        unsafe { (self.inner.functions().webkit_web_view_load_uri)(self.inner.view, url.as_ptr()) };
    }

    pub(crate) fn go_back(&self) {
        unsafe { (self.inner.functions().webkit_web_view_go_back)(self.inner.view) };
    }

    pub(crate) fn go_forward(&self) {
        unsafe { (self.inner.functions().webkit_web_view_go_forward)(self.inner.view) };
    }

    pub(crate) fn reload(&self) {
        unsafe { (self.inner.functions().webkit_web_view_reload)(self.inner.view) };
    }

    /// WebKitGTK derives `prefers-color-scheme` from GTK's dark-theme
    /// preference, which only exists process-wide. The only GTK in this
    /// process is the browser, and every surface follows the same editor
    /// theme, so setting it globally is what a per-surface setting would be.
    pub(crate) fn set_dark_mode(&self, dark: bool) {
        let functions = self.inner.functions();
        let settings = unsafe { (functions.gtk_settings_get_default)() };
        if settings.is_null() {
            return;
        }
        unsafe {
            set_boolean_property(
                functions,
                settings,
                c"gtk-application-prefer-dark-theme",
                dark,
            )
        };
    }

    pub(crate) fn set_background_color(&self, red: u8, green: u8, blue: u8) {
        let color = GdkRgba {
            red: f64::from(red) / 255.,
            green: f64::from(green) / 255.,
            blue: f64::from(blue) / 255.,
            alpha: 1.,
        };
        unsafe {
            (self.inner.functions().webkit_web_view_set_background_color)(self.inner.view, &color)
        };
    }

    pub(crate) fn release_keyboard_focus(&self) {
        let page_has_focus = {
            let mut state = self.inner.state.borrow_mut();
            state.pending_focus = false;
            state.page_has_focus
        };
        if page_has_focus {
            self.inner.return_keyboard_focus();
        }
    }

    pub(crate) fn focus_page(&self) {
        if self.inner.embedding.is_none() {
            // The address bar sends the user to the page; in detached mode the
            // page is in its own window, so that is the one to bring up.
            if self.inner.state.borrow().detached_shown {
                self.raise_detached_window();
            }
            return;
        }
        let input_blocked = self
            .inner
            .state
            .borrow()
            .applied_shape
            .as_ref()
            .is_some_and(|(_, _, input_blocked)| *input_blocked);
        if input_blocked {
            self.inner.state.borrow_mut().pending_focus = true;
            return;
        }
        let inner = &self.inner;
        inner
            .runtime
            .with_x_errors_ignored(|| inner.move_focus_into_page());
    }
}

impl WebView {
    /// The JSON of the script's completion value, like WebView2's
    /// `ExecuteScript`: `undefined` (and anything `JSON.stringify` drops)
    /// comes back as `null`. A thrown exception is an error. Promises are not
    /// awaited; the caller polls for those.
    pub(crate) fn execute_script(&self, script: &str) -> oneshot::Receiver<Result<String>> {
        let Ok(source) = CString::new(script) else {
            return completed(Err(anyhow!("the script contains a NUL character")));
        };
        let functions = self.inner.functions();
        let (sender, receiver) = oneshot::channel();
        let request: Gpointer = Box::into_raw(Box::new(PendingReply::new(sender))).cast();
        // SAFETY: `request` is reclaimed by exactly one of the callbacks,
        // which WebKit always calls, also when the view goes away first.
        unsafe {
            match (
                functions.webkit_web_view_evaluate_javascript,
                functions.webkit_web_view_run_javascript,
            ) {
                (Some(evaluate), _) => evaluate(
                    self.inner.view,
                    source.as_ptr(),
                    -1,
                    ptr::null(),
                    ptr::null(),
                    ptr::null_mut(),
                    on_script_evaluated,
                    request,
                ),
                (None, Some(run)) => run(
                    self.inner.view,
                    source.as_ptr(),
                    ptr::null_mut(),
                    on_script_ran,
                    request,
                ),
                (None, None) => {
                    drop(Box::from_raw(request.cast::<ScriptSender>()));
                    return completed(Err(anyhow!("this WebKitGTK cannot run scripts")));
                }
            }
        }
        receiver
    }

    pub(crate) fn capture_png(&self) -> oneshot::Receiver<Result<Vec<u8>>> {
        let (sender, receiver) = oneshot::channel();
        let request: Gpointer = Box::into_raw(Box::new(PendingReply::new(sender))).cast();
        // SAFETY: `request` is reclaimed by the callback, which WebKit always
        // calls.
        unsafe {
            (self.inner.functions().webkit_web_view_get_snapshot)(
                self.inner.view,
                WEBKIT_SNAPSHOT_REGION_VISIBLE,
                WEBKIT_SNAPSHOT_OPTIONS_NONE,
                ptr::null_mut(),
                on_snapshot_taken,
                request,
            )
        };
        receiver
    }

    /// WebKitGTK has no public API for trusted input; the caller synthesises
    /// it in page JavaScript instead.
    pub(crate) fn dispatch_input(&self, _input: NativeInput) -> oneshot::Receiver<Result<()>> {
        completed(Err(NativeInputUnsupported.into()))
    }

    pub(crate) fn open_developer_tools(&self) {
        let functions = self.inner.functions();
        unsafe {
            let inspector = (functions.webkit_web_view_get_inspector)(self.inner.view);
            if !inspector.is_null() {
                (functions.webkit_web_inspector_show)(inspector);
            }
        }
    }

    pub(crate) fn is_detached(&self) -> bool {
        self.inner.embedding.is_none()
    }

    pub(crate) fn raise_detached_window(&self) {
        if self.inner.embedding.is_some() {
            return;
        }
        self.inner.state.borrow_mut().detached_shown = true;
        let functions = self.inner.functions();
        unsafe {
            (functions.gtk_widget_show)(self.inner.window);
            (functions.gtk_window_present)(self.inner.window);
        }
    }
}

impl Drop for WebView {
    fn drop(&mut self) {
        let functions = self.inner.functions();
        let handlers = std::mem::take(&mut *self.inner.handlers.borrow_mut());
        for (instance, handler) in handlers {
            // A destroyed window (and its view) already dropped its handlers.
            unsafe {
                if (functions.g_signal_handler_is_connected)(instance, handler) != FALSE {
                    (functions.g_signal_handler_disconnect)(instance, handler);
                }
                (functions.g_object_unref)(instance);
            }
        }
        let page_had_focus = self.inner.state.borrow().page_has_focus;
        if page_had_focus {
            // X would otherwise hand the focus of the destroyed window to the
            // host's parent rather than to the host.
            self.inner.return_keyboard_focus();
        }
        // If gpui destroyed its window first, X destroyed the page's with it,
        // and GDK's own `XDestroyWindow` fails.
        self.inner.runtime.with_x_errors_ignored(|| unsafe {
            (functions.gtk_widget_destroy)(self.inner.window);
            (functions.g_object_unref)(self.inner.window);
            (functions.g_object_unref)(self.inner.view);
        });
        LIVE_SURFACES.set(LIVE_SURFACES.get().saturating_sub(1));
    }
}

fn create_embedded_window(
    runtime: &Runtime,
    view: Gpointer,
    x_display: Gpointer,
    host: isize,
) -> Result<(Gpointer, Embedding)> {
    let functions = &runtime.functions;
    let (Some(get_xid), Some(reparent), Some(flush)) = (
        functions.gdk_x11_window_get_xid,
        functions.XReparentWindow,
        functions.XFlush,
    ) else {
        bail!("GTK's X11 backend is incomplete");
    };
    let host = host as XWindow;
    // A popup is override-redirect, so no window manager ever sees it - not
    // even in the moment between its creation and the reparenting.
    unsafe {
        let window = (functions.gtk_window_new)(GTK_WINDOW_POPUP);
        (functions.gtk_container_add)(window, view);
        (functions.gtk_widget_show)(view);
        // gpui sizes the window; without this GTK would not let it shrink
        // below WebKit's preferred size.
        (functions.gtk_widget_set_size_request)(view, 1, 1);
        (functions.gtk_widget_realize)(window);
        let gdk_window = (functions.gtk_widget_get_window)(window);
        if gdk_window.is_null() {
            (functions.gtk_widget_destroy)(window);
            bail!("GTK did not create an X11 window for the page");
        }
        let surface = get_xid(gdk_window);
        runtime.with_x_errors_ignored(|| {
            reparent(x_display, surface, host, 0, 0);
            flush(x_display);
        });
        Ok((
            window,
            Embedding {
                x_display,
                host,
                surface,
            },
        ))
    }
}

fn create_detached_window(runtime: &Runtime, view: Gpointer) -> Gpointer {
    let functions = &runtime.functions;
    unsafe {
        let window = (functions.gtk_window_new)(GTK_WINDOW_TOPLEVEL);
        (functions.gtk_container_add)(window, view);
        (functions.gtk_widget_show)(view);
        window
    }
}

/// Sets a boolean GObject property, skipping it when this version of the
/// library does not have it. Returns whether it was set.
///
/// # Safety
///
/// `object` must be a live GObject.
unsafe fn set_boolean_property(
    functions: &Functions,
    object: Gpointer,
    name: &CStr,
    value: bool,
) -> bool {
    // A `GTypeInstance` starts with its class pointer.
    let class = unsafe { *object.cast::<Gpointer>() };
    if unsafe { (functions.g_object_class_find_property)(class, name.as_ptr()) }.is_null() {
        log::info!(
            "WebKitGTK: this version has no {} property",
            name.to_string_lossy()
        );
        return false;
    }
    unsafe {
        (functions.g_object_set)(
            object,
            name.as_ptr(),
            if value { TRUE } else { FALSE },
            ptr::null::<c_char>(),
        )
    };
    true
}

impl Inner {
    fn functions(&self) -> &'static Functions {
        &self.runtime.functions
    }

    fn connect(self: &Rc<Self>, instance: Gpointer, signal: &CStr, handler: Gcallback) {
        let data: Gpointer = Box::into_raw(Box::new(Rc::downgrade(self))).cast();
        let handler_id = unsafe {
            (self.functions().g_signal_connect_data)(
                instance,
                signal.as_ptr(),
                handler,
                data,
                Some(release_signal_data),
                0,
            )
        };
        if handler_id == 0 {
            log::error!(
                "WebKitGTK: could not connect to {}",
                signal.to_string_lossy()
            );
            // GObject only takes the data over when the connection succeeds.
            drop(unsafe { Box::from_raw(data.cast::<Weak<Inner>>()) });
            return;
        }
        // Held until `Drop` disconnects, so the instance is still there to ask
        // even if GTK destroyed it (and the objects it owned) meanwhile.
        unsafe { (self.functions().g_object_ref)(instance) };
        self.handlers.borrow_mut().push((instance, handler_id));
    }

    fn connect_signals(self: &Rc<Self>) {
        let functions = self.functions();
        let view = self.view;
        self.connect(
            view,
            c"decide-policy",
            gcallback!(on_decide_policy, DecidePolicy),
        );
        self.connect(
            view,
            c"load-changed",
            gcallback!(on_load_changed, LoadChanged),
        );
        self.connect(view, c"load-failed", gcallback!(on_load_failed, LoadFailed));
        self.connect(view, c"notify::title", gcallback!(on_title_changed, Notify));
        self.connect(view, c"notify::uri", gcallback!(on_uri_changed, Notify));
        if self.ai_profile {
            self.connect(view, c"print", gcallback!(on_print, Print));
            // WebKit's own default denies too, except pointer lock; denying
            // here keeps AI tabs at "deny everything" whatever a distribution
            // or a later WebKit changes.
            self.connect(
                view,
                c"permission-request",
                gcallback!(on_permission_request, Request),
            );
            self.connect(
                view,
                c"run-file-chooser",
                gcallback!(on_run_file_chooser, Request),
            );
        }
        let back_forward_list = unsafe { (functions.webkit_web_view_get_back_forward_list)(view) };
        if !back_forward_list.is_null() {
            self.connect(
                back_forward_list,
                c"changed",
                gcallback!(on_history_changed, HistoryChanged),
            );
        }
        // The context is shared by every surface of the profile, so the handler
        // checks which view a download belongs to.
        self.connect(
            self.context,
            c"download-started",
            gcallback!(on_download_started, DownloadStarted),
        );
        if self.embedding.is_some() {
            self.connect(
                view,
                c"focus-in-event",
                gcallback!(on_focus_in, WidgetEvent),
            );
            self.connect(
                view,
                c"focus-out-event",
                gcallback!(on_focus_out, WidgetEvent),
            );
            self.connect(
                view,
                c"button-press-event",
                gcallback!(on_button_press, WidgetEvent),
            );
            // On the window, so that it runs before GTK hands the key to the
            // page: `Esc` leaves the page instead of reaching it.
            self.connect(
                self.window,
                c"key-press-event",
                gcallback!(on_key_press, KeyPress),
            );
        } else {
            self.connect(
                self.window,
                c"delete-event",
                gcallback!(on_delete, WidgetEvent),
            );
        }
    }

    fn send(&self, event: WebViewEvent) {
        // The receiver goes away with the view, while WebKit can still call
        // back until the surface is dropped.
        self.events.unbounded_send(event).ok();
    }

    fn decision(&self, url: &str) -> NavigationDecision {
        match &self.navigation_filter {
            // `about:srcdoc` is an `<iframe srcdoc>`, which loads nothing; see
            // `decide_policy` for why frames reach the filter at all.
            Some(filter) if url != "about:srcdoc" => filter(url),
            _ => NavigationDecision::Allow,
        }
    }

    /// WebKitGTK 4.1 asks the same `NAVIGATION_ACTION` question for frames
    /// as for the page and does not say which one it is asking about, so the
    /// filter sees frame navigations too, and a denied one is reported as
    /// [`WebViewEvent::NavigationBlocked`] like a top-level one.
    fn decide_policy(&self, decision: Gpointer, decision_type: c_int) -> Gboolean {
        let functions = self.functions();
        match decision_type {
            WEBKIT_POLICY_DECISION_TYPE_NAVIGATION_ACTION
            | WEBKIT_POLICY_DECISION_TYPE_NEW_WINDOW_ACTION => {
                let url = unsafe {
                    let action = (functions
                        .webkit_navigation_policy_decision_get_navigation_action)(
                        decision
                    );
                    let request = (functions.webkit_navigation_action_get_request)(action);
                    string_from((functions.webkit_uri_request_get_uri)(request))
                };
                let opens_window = decision_type == WEBKIT_POLICY_DECISION_TYPE_NEW_WINDOW_ACTION;
                match self.decision(&url) {
                    NavigationDecision::Deny(reason) => {
                        unsafe { (functions.webkit_policy_decision_ignore)(decision) };
                        log::info!("WebKitGTK: blocked a navigation to {url}: {reason}");
                        self.send(WebViewEvent::NavigationBlocked { url, reason });
                        TRUE
                    }
                    // The panel hosts one page per surface, so a new window is
                    // opened in place instead.
                    NavigationDecision::Allow if opens_window => {
                        unsafe { (functions.webkit_policy_decision_ignore)(decision) };
                        if !url.is_empty() {
                            self.send(WebViewEvent::OpenInPlace(url));
                        }
                        TRUE
                    }
                    NavigationDecision::Allow => FALSE,
                }
            }
            WEBKIT_POLICY_DECISION_TYPE_RESPONSE if self.block_downloads => {
                // A response WebKit cannot display is what it would download.
                let supported = unsafe {
                    (functions.webkit_response_policy_decision_is_mime_type_supported)(decision)
                };
                if supported != FALSE {
                    return FALSE;
                }
                let url = unsafe {
                    let response =
                        (functions.webkit_response_policy_decision_get_response)(decision);
                    string_from((functions.webkit_uri_response_get_uri)(response))
                };
                unsafe { (functions.webkit_policy_decision_ignore)(decision) };
                log::info!("WebKitGTK: blocked a download of {url}");
                self.send(WebViewEvent::DownloadBlocked { url });
                TRUE
            }
            _ => FALSE,
        }
    }

    /// Catches what the response policy does not: downloads started without
    /// a response to judge, such as `<a download>`.
    fn download_started(&self, download: Gpointer) {
        let functions = self.functions();
        if !self.block_downloads
            || unsafe { (functions.webkit_download_get_web_view)(download) } != self.view
        {
            return;
        }
        let url = unsafe {
            let request = (functions.webkit_download_get_request)(download);
            string_from((functions.webkit_uri_request_get_uri)(request))
        };
        unsafe { (functions.webkit_download_cancel)(download) };
        log::info!("WebKitGTK: cancelled a download of {url}");
        self.send(WebViewEvent::DownloadBlocked { url });
    }

    fn load_changed(&self, load_event: c_int) {
        match load_event {
            WEBKIT_LOAD_STARTED => self.state.borrow_mut().load_failed = false,
            WEBKIT_LOAD_COMMITTED => {
                self.send(WebViewEvent::UrlChanged(self.uri()));
                self.publish_history();
            }
            WEBKIT_LOAD_FINISHED => {
                let succeeded = !self.state.borrow().load_failed;
                log::info!("WebKitGTK: navigation completed (succeeded: {succeeded})");
                self.send(WebViewEvent::UrlChanged(self.uri()));
                self.publish_history();
                self.send(WebViewEvent::LoadFinished { succeeded });
            }
            _ => {}
        }
    }

    fn load_failed(&self, failing_url: String, error: Option<String>) {
        log::info!(
            "WebKitGTK: loading {failing_url} failed: {}",
            error.as_deref().unwrap_or("unknown error")
        );
        self.state.borrow_mut().load_failed = true;
    }

    fn title_changed(&self) {
        let title = unsafe { string_from((self.functions().webkit_web_view_get_title)(self.view)) };
        if self.embedding.is_none() {
            let window_title = if title.is_empty() {
                self.uri()
            } else {
                title.clone()
            };
            if let Ok(window_title) = CString::new(window_title) {
                unsafe {
                    (self.functions().gtk_window_set_title)(self.window, window_title.as_ptr())
                };
            }
        }
        self.send(WebViewEvent::TitleChanged(title));
    }

    fn uri(&self) -> String {
        unsafe { string_from((self.functions().webkit_web_view_get_uri)(self.view)) }
    }

    fn publish_history(&self) {
        let functions = self.functions();
        let (can_go_back, can_go_forward) = unsafe {
            (
                (functions.webkit_web_view_can_go_back)(self.view) != FALSE,
                (functions.webkit_web_view_can_go_forward)(self.view) != FALSE,
            )
        };
        self.send(WebViewEvent::HistoryChanged {
            can_go_back,
            can_go_forward,
        });
    }

    fn focus_changed(&self, focused: bool) {
        if focused && self.state.borrow().input_blocked {
            log::info!(
                "WebKitGTK: handing back the focus the page took just before its input was blocked"
            );
            self.return_keyboard_focus();
            return;
        }
        self.state.borrow_mut().page_has_focus = focused;
        self.send(WebViewEvent::PageFocusChanged(focused));
    }

    /// X gives the keyboard to whichever window was last told to take it, and
    /// no window manager is involved for a window inside gpui's, so a click
    /// into the page has to move the focus itself.
    fn button_pressed(&self) {
        let (page_has_focus, input_blocked) = {
            let state = self.state.borrow();
            (state.page_has_focus, state.input_blocked)
        };
        if !page_has_focus && !input_blocked {
            self.move_focus_into_page();
        }
    }

    fn move_focus_into_page(&self) {
        let Some(embedding) = self.embedding else {
            return;
        };
        self.give_x_focus_to(embedding, embedding.surface);
        unsafe { (self.functions().gtk_widget_grab_focus)(self.view) };
    }

    fn return_keyboard_focus(&self) {
        self.state.borrow_mut().page_has_focus = false;
        let Some(embedding) = self.embedding else {
            return;
        };
        self.give_x_focus_to(embedding, embedding.host);
    }

    /// The window may be unmapped (a hidden page, a minimised editor) or gone
    /// with gpui's, both of which X answers with an error.
    fn give_x_focus_to(&self, embedding: Embedding, window: XWindow) {
        let functions = self.functions();
        if let (Some(set_input_focus), Some(flush)) = (functions.XSetInputFocus, functions.XFlush) {
            self.runtime.with_x_errors_ignored(|| unsafe {
                set_input_focus(
                    embedding.x_display,
                    window,
                    X_REVERT_TO_PARENT,
                    X_CURRENT_TIME,
                );
                flush(embedding.x_display);
            });
        }
    }

    fn escape_pressed(&self) {
        self.return_keyboard_focus();
        self.send(WebViewEvent::FocusReturned);
    }

    fn sync_detached(&self, viewport: &Viewport) {
        if viewport.hidden_reason.is_some() || self.state.borrow().detached_shown {
            return;
        }
        self.state.borrow_mut().detached_shown = true;
        // GTK sizes windows in logical pixels on Wayland too, like gpui.
        let width = viewport.width.round().max(320.) as c_int;
        let height = viewport.height.round().max(240.) as c_int;
        let functions = self.functions();
        unsafe {
            (functions.gtk_window_set_default_size)(self.window, width, height);
            (functions.gtk_widget_show)(self.window);
        }
        log::info!("WebKitGTK: showed the detached window at {width}x{height}");
    }

    /// GDK counts in its own integer scale (`GDK_SCALE`), gpui in device
    /// pixels.
    fn gdk_scale(&self) -> i32 {
        let gdk_window = unsafe { (self.functions().gtk_widget_get_window)(self.window) };
        if gdk_window.is_null() {
            return 1;
        }
        unsafe { (self.functions().gdk_window_get_scale_factor)(gdk_window) }.max(1)
    }

    fn apply_viewport(&self, defer_cuts: bool) -> bool {
        let (bounds, scale, visible, input_blocked, hidden_reason) = {
            let state = self.state.borrow();
            (
                (state.applied_bounds != Some(state.bounds)).then_some(state.bounds),
                (state.applied_scale != Some(state.scale)).then_some(state.scale),
                (state.applied_visible != Some(state.visible)).then_some(state.visible),
                state.input_blocked,
                state.hidden_reason,
            )
        };
        let functions = self.functions();
        let gdk_scale = self.gdk_scale();

        if let Some(bounds) = bounds
            && !bounds.is_empty()
        {
            unsafe {
                (functions.gtk_window_move)(
                    self.window,
                    bounds.left / gdk_scale,
                    bounds.top / gdk_scale,
                );
                (functions.gtk_window_resize)(
                    self.window,
                    (bounds.width() / gdk_scale).max(1),
                    (bounds.height() / gdk_scale).max(1),
                );
            }
            log::info!(
                "WebKitGTK: bounds set to ({}, {})-({}, {}) in physical pixels",
                bounds.left,
                bounds.top,
                bounds.right,
                bounds.bottom
            );
            self.state.borrow_mut().applied_bounds = Some(bounds);
        }

        // WebKit renders at GDK's integer scale; gpui's own (possibly
        // fractional) scale is made up with the page zoom, as WebView2's
        // rasterization scale does on Windows.
        if let Some(scale) = scale {
            unsafe {
                (functions.webkit_web_view_set_zoom_level)(self.view, scale / f64::from(gdk_scale))
            };
            self.state.borrow_mut().applied_scale = Some(scale);
        }

        if input_blocked && self.state.borrow().page_has_focus {
            self.return_keyboard_focus();
        }
        let deferred = self.apply_shape(defer_cuts);

        if let Some(visible) = visible {
            unsafe {
                if visible {
                    (functions.gtk_widget_show)(self.window);
                } else {
                    (functions.gtk_widget_hide)(self.window);
                }
            }
            match hidden_reason {
                Some(reason) => log::info!("WebKitGTK: surface hidden because {reason}"),
                None => log::info!("WebKitGTK: surface shown"),
            }
            self.state.borrow_mut().applied_visible = Some(visible);
        }

        if !input_blocked {
            let (pending_focus, view_focused) = {
                let mut state = self.state.borrow_mut();
                let pending_focus = state.visible && std::mem::take(&mut state.pending_focus);
                (pending_focus, state.view_focused)
            };
            // With gpui's focus gone elsewhere meanwhile, the page would pull
            // the keyboard away from wherever the user moved on to.
            if pending_focus && view_focused {
                self.move_focus_into_page();
            }
        }
        deferred
    }

    /// Cuts gpui's overlays out of the window's shape, so they show through,
    /// and out of its input shape, so they get the clicks. `input_blocked`
    /// empties the input shape: the page stays visible but every click goes to
    /// gpui's window underneath.
    fn apply_shape(&self, defer_cuts: bool) -> bool {
        let (bounds, holes, input_blocked, scale) = {
            let state = self.state.borrow();
            if !state.visible {
                return false;
            }
            let wanted = (state.bounds, state.holes.clone(), state.input_blocked);
            if state.applied_shape.as_ref() == Some(&wanted) {
                return false;
            }
            // Removing holes is never held back: it only covers the page up.
            if defer_cuts && !wanted.1.is_empty() {
                return true;
            }
            (wanted.0, wanted.1, wanted.2, state.scale)
        };

        let functions = self.functions();
        let gdk_window = unsafe { (functions.gtk_widget_get_window)(self.window) };
        if gdk_window.is_null() {
            return false;
        }
        let gdk_scale = self.gdk_scale();
        let radius = (OVERLAY_CORNER_RADIUS * scale).round() as i32;
        let to_gdk = |rect: DeviceRect| CairoRectangleInt {
            x: (rect.left - bounds.left) / gdk_scale,
            y: (rect.top - bounds.top) / gdk_scale,
            width: (rect.width() / gdk_scale).max(1),
            height: (rect.height() / gdk_scale).max(1),
        };

        unsafe {
            let shape = if holes.is_empty() {
                ptr::null_mut()
            } else {
                let whole = to_gdk(bounds);
                let region = (functions.cairo_region_create_rectangle)(&whole);
                for hole in &holes {
                    for piece in rounded_rect_pieces(*hole, radius) {
                        (functions.cairo_region_subtract_rectangle)(region, &to_gdk(piece));
                    }
                }
                region
            };
            (functions.gdk_window_shape_combine_region)(gdk_window, shape, 0, 0);
            if input_blocked {
                let nothing = (functions.cairo_region_create)();
                (functions.gdk_window_input_shape_combine_region)(gdk_window, nothing, 0, 0);
                (functions.cairo_region_destroy)(nothing);
            } else {
                (functions.gdk_window_input_shape_combine_region)(gdk_window, shape, 0, 0);
            }
            if !shape.is_null() {
                (functions.cairo_region_destroy)(shape);
            }
        }
        self.state.borrow_mut().applied_shape = Some((bounds, holes, input_blocked));
        false
    }
}

type DecidePolicy = unsafe extern "C" fn(Gpointer, Gpointer, c_int, Gpointer) -> Gboolean;
type LoadChanged = unsafe extern "C" fn(Gpointer, c_int, Gpointer);
type LoadFailed =
    unsafe extern "C" fn(Gpointer, c_int, *const c_char, *mut Gerror, Gpointer) -> Gboolean;
type Notify = unsafe extern "C" fn(Gpointer, Gpointer, Gpointer);
type HistoryChanged = unsafe extern "C" fn(Gpointer, Gpointer, Gpointer, Gpointer);
type DownloadStarted = unsafe extern "C" fn(Gpointer, Gpointer, Gpointer);
type WidgetEvent = unsafe extern "C" fn(Gpointer, Gpointer, Gpointer) -> Gboolean;
type Print = unsafe extern "C" fn(Gpointer, Gpointer, Gpointer) -> Gboolean;
type Request = unsafe extern "C" fn(Gpointer, Gpointer, Gpointer) -> Gboolean;
type KeyPress = unsafe extern "C" fn(Gpointer, *const GdkEventKeyPrefix, Gpointer) -> Gboolean;

unsafe extern "C" fn release_signal_data(data: Gpointer, _closure: Gpointer) {
    drop(unsafe { Box::from_raw(data.cast::<Weak<Inner>>()) });
}

/// # Safety
///
/// `data` must be the signal data [`Inner::connect`] handed to GObject.
unsafe fn surface_of(data: Gpointer) -> Option<Rc<Inner>> {
    unsafe { &*data.cast::<Weak<Inner>>() }.upgrade()
}

unsafe extern "C" fn on_decide_policy(
    _view: Gpointer,
    decision: Gpointer,
    decision_type: c_int,
    data: Gpointer,
) -> Gboolean {
    match unsafe { surface_of(data) } {
        Some(surface) => surface.decide_policy(decision, decision_type),
        None => FALSE,
    }
}

unsafe extern "C" fn on_load_changed(_view: Gpointer, load_event: c_int, data: Gpointer) {
    if let Some(surface) = unsafe { surface_of(data) } {
        surface.load_changed(load_event);
    }
}

unsafe extern "C" fn on_load_failed(
    _view: Gpointer,
    _load_event: c_int,
    failing_url: *const c_char,
    error: *mut Gerror,
    data: Gpointer,
) -> Gboolean {
    if let Some(surface) = unsafe { surface_of(data) } {
        let message = (!error.is_null()).then(|| unsafe { string_from((*error).message) });
        surface.load_failed(unsafe { string_from(failing_url) }, message);
    }
    // Let WebKit show its own error page.
    FALSE
}

unsafe extern "C" fn on_title_changed(_view: Gpointer, _property: Gpointer, data: Gpointer) {
    if let Some(surface) = unsafe { surface_of(data) } {
        surface.title_changed();
    }
}

unsafe extern "C" fn on_uri_changed(_view: Gpointer, _property: Gpointer, data: Gpointer) {
    if let Some(surface) = unsafe { surface_of(data) } {
        surface.send(WebViewEvent::UrlChanged(surface.uri()));
    }
}

unsafe extern "C" fn on_history_changed(
    _list: Gpointer,
    _added: Gpointer,
    _removed: Gpointer,
    data: Gpointer,
) {
    if let Some(surface) = unsafe { surface_of(data) } {
        surface.publish_history();
    }
}

unsafe extern "C" fn on_download_started(_context: Gpointer, download: Gpointer, data: Gpointer) {
    if let Some(surface) = unsafe { surface_of(data) } {
        surface.download_started(download);
    }
}

unsafe extern "C" fn on_print(_view: Gpointer, _operation: Gpointer, _data: Gpointer) -> Gboolean {
    log::info!("WebKitGTK: ignored a print request from an AI tab");
    TRUE
}

unsafe extern "C" fn on_permission_request(
    _view: Gpointer,
    request: Gpointer,
    data: Gpointer,
) -> Gboolean {
    let Some(surface) = (unsafe { surface_of(data) }) else {
        return FALSE;
    };
    log::info!("WebKitGTK: denied a permission request in an AI tab");
    unsafe { (surface.functions().webkit_permission_request_deny)(request) };
    TRUE
}

unsafe extern "C" fn on_run_file_chooser(
    _view: Gpointer,
    request: Gpointer,
    data: Gpointer,
) -> Gboolean {
    if let Some(surface) = unsafe { surface_of(data) } {
        log::info!("WebKitGTK: cancelled a file chooser in an AI tab");
        unsafe { (surface.functions().webkit_file_chooser_request_cancel)(request) };
    }
    // Handled either way: WebKit's default would show the dialog.
    TRUE
}

unsafe extern "C" fn on_focus_in(_widget: Gpointer, _event: Gpointer, data: Gpointer) -> Gboolean {
    if let Some(surface) = unsafe { surface_of(data) } {
        surface.focus_changed(true);
    }
    FALSE
}

unsafe extern "C" fn on_focus_out(_widget: Gpointer, _event: Gpointer, data: Gpointer) -> Gboolean {
    if let Some(surface) = unsafe { surface_of(data) } {
        surface.focus_changed(false);
    }
    FALSE
}

unsafe extern "C" fn on_button_press(
    _widget: Gpointer,
    _event: Gpointer,
    data: Gpointer,
) -> Gboolean {
    if let Some(surface) = unsafe { surface_of(data) } {
        surface.button_pressed();
    }
    FALSE
}

unsafe extern "C" fn on_key_press(
    _widget: Gpointer,
    event: *const GdkEventKeyPrefix,
    data: Gpointer,
) -> Gboolean {
    if event.is_null() || unsafe { (*event).keyval } != GDK_KEY_ESCAPE {
        return FALSE;
    }
    match unsafe { surface_of(data) } {
        Some(surface) => {
            surface.escape_pressed();
            TRUE
        }
        None => FALSE,
    }
}

/// Closing the detached window only hides it: the surface belongs to the
/// panel's tab, which brings it back with [`WebView::raise_detached_window`].
unsafe extern "C" fn on_delete(widget: Gpointer, _event: Gpointer, data: Gpointer) -> Gboolean {
    if let Some(surface) = unsafe { surface_of(data) } {
        unsafe { (surface.functions().gtk_widget_hide)(widget) };
    }
    TRUE
}

/// A reply WebKit still owes. Counted so that the main loop keeps being pumped
/// until it arrives even after the last surface is gone: WebKit answers a
/// request for a destroyed view with an error, but only from the main loop,
/// and the caller would otherwise wait forever.
struct PendingReply<T> {
    sender: Option<oneshot::Sender<Result<T>>>,
}

impl<T> PendingReply<T> {
    fn new(sender: oneshot::Sender<Result<T>>) -> Self {
        PENDING_REPLIES.set(PENDING_REPLIES.get() + 1);
        Self {
            sender: Some(sender),
        }
    }

    fn send(mut self, outcome: Result<T>) {
        if let Some(sender) = self.sender.take() {
            // The caller may have stopped waiting.
            sender.send(outcome).ok();
        }
    }
}

impl<T> Drop for PendingReply<T> {
    fn drop(&mut self) {
        PENDING_REPLIES.set(PENDING_REPLIES.get().saturating_sub(1));
    }
}

type ScriptSender = PendingReply<String>;
type SnapshotSender = PendingReply<Vec<u8>>;

/// # Safety
///
/// `value` must be null or a live `JSCValue`.
unsafe fn json_of(functions: &Functions, value: Gpointer) -> String {
    if value.is_null() || unsafe { (functions.jsc_value_is_undefined)(value) } != FALSE {
        return "null".to_string();
    }
    let json = unsafe { (functions.jsc_value_to_json)(value, 0) };
    if json.is_null() {
        // Functions, symbols and the like, which `JSON.stringify` drops too.
        return "null".to_string();
    }
    let text = unsafe { string_from(json) };
    unsafe { (functions.g_free)(json.cast()) };
    text
}

/// # Safety
///
/// `error` must be null or a `GError` owned by the caller.
unsafe fn take_error(functions: &Functions, error: *mut Gerror) -> anyhow::Error {
    if error.is_null() {
        return anyhow!("WebKitGTK reported a failure without a reason");
    }
    let message = unsafe { string_from((*error).message) };
    unsafe { (functions.g_error_free)(error) };
    anyhow!(message)
}

unsafe extern "C" fn on_script_evaluated(source: Gpointer, result: Gpointer, request: Gpointer) {
    let sender = unsafe { Box::from_raw(request.cast::<ScriptSender>()) };
    let outcome = match loaded_runtime().map(|runtime| &runtime.functions) {
        Some(functions) => match functions.webkit_web_view_evaluate_javascript_finish {
            Some(finish) => unsafe {
                let mut error = ptr::null_mut();
                let value = finish(source, result, &mut error);
                if value.is_null() {
                    Err(take_error(functions, error))
                } else {
                    let json = json_of(functions, value);
                    (functions.g_object_unref)(value);
                    Ok(json)
                }
            },
            None => Err(anyhow!("this WebKitGTK cannot run scripts")),
        },
        None => Err(anyhow!("WebKitGTK is not loaded")),
    };
    sender.send(outcome);
}

unsafe extern "C" fn on_script_ran(source: Gpointer, result: Gpointer, request: Gpointer) {
    let sender = unsafe { Box::from_raw(request.cast::<ScriptSender>()) };
    let outcome = match loaded_runtime().map(|runtime| &runtime.functions) {
        Some(functions) => match (
            functions.webkit_web_view_run_javascript_finish,
            functions.webkit_javascript_result_get_js_value,
            functions.webkit_javascript_result_unref,
        ) {
            (Some(finish), Some(get_value), Some(unref)) => unsafe {
                let mut error = ptr::null_mut();
                let script_result = finish(source, result, &mut error);
                if script_result.is_null() {
                    Err(take_error(functions, error))
                } else {
                    let json = json_of(functions, get_value(script_result));
                    unref(script_result);
                    Ok(json)
                }
            },
            _ => Err(anyhow!("this WebKitGTK cannot run scripts")),
        },
        None => Err(anyhow!("WebKitGTK is not loaded")),
    };
    sender.send(outcome);
}

unsafe extern "C" fn on_snapshot_taken(source: Gpointer, result: Gpointer, request: Gpointer) {
    let sender = unsafe { Box::from_raw(request.cast::<SnapshotSender>()) };
    let outcome = match loaded_runtime().map(|runtime| &runtime.functions) {
        Some(functions) => unsafe {
            let mut error = ptr::null_mut();
            let surface =
                (functions.webkit_web_view_get_snapshot_finish)(source, result, &mut error);
            if surface.is_null() {
                Err(take_error(functions, error).context("WebKitGTK could not take a snapshot"))
            } else {
                let mut png = Vec::new();
                let status = (functions.cairo_surface_write_to_png_stream)(
                    surface,
                    append_png_bytes,
                    (&mut png as *mut Vec<u8>).cast(),
                );
                (functions.cairo_surface_destroy)(surface);
                if status == CAIRO_STATUS_SUCCESS {
                    Ok(png)
                } else {
                    Err(anyhow!(
                        "cairo could not encode the snapshot (status {status})"
                    ))
                }
            }
        },
        None => Err(anyhow!("WebKitGTK is not loaded")),
    };
    sender.send(outcome);
}

unsafe extern "C" fn append_png_bytes(closure: Gpointer, data: *const u8, length: c_uint) -> c_int {
    if closure.is_null() || (data.is_null() && length > 0) {
        return CAIRO_STATUS_WRITE_ERROR;
    }
    let png = unsafe { &mut *closure.cast::<Vec<u8>>() };
    if length > 0 {
        png.extend_from_slice(unsafe { std::slice::from_raw_parts(data, length as usize) });
    }
    CAIRO_STATUS_SUCCESS
}

/// # Safety
///
/// `pointer` must be null or a NUL-terminated string.
unsafe fn string_from(pointer: *const c_char) -> String {
    if pointer.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(pointer) }
        .to_string_lossy()
        .into_owned()
}

fn path_to_c_string(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("{} contains a NUL character", path.display()))
}

/// A rectangle in physical pixels relative to the host window's client area.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DeviceRect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl DeviceRect {
    fn width(&self) -> i32 {
        self.right - self.left
    }

    fn height(&self) -> i32 {
        self.bottom - self.top
    }

    fn is_empty(&self) -> bool {
        self.right <= self.left || self.bottom <= self.top
    }

    fn intersect(&self, other: &DeviceRect) -> Option<DeviceRect> {
        let intersection = DeviceRect {
            left: self.left.max(other.left),
            top: self.top.max(other.top),
            right: self.right.min(other.right),
            bottom: self.bottom.min(other.bottom),
        };
        (!intersection.is_empty()).then_some(intersection)
    }
}

/// The surface bounds and the overlay holes in physical pixels; the same
/// rounding and dock-handle rule as `webview_windows::viewport_geometry`.
fn viewport_geometry(viewport: &Viewport) -> (DeviceRect, Vec<DeviceRect>) {
    let to_device = |value: f32| (value * viewport.scale_factor).round() as i32;
    let left = to_device(viewport.origin_x);
    let top = to_device(viewport.origin_y);
    let bounds = DeviceRect {
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
            let overlay = DeviceRect {
                left: to_device(overlay.left),
                top: to_device(overlay.top),
                right: to_device(overlay.right),
                bottom: to_device(overlay.bottom),
            };
            let hole = bounds.intersect(&overlay)?;
            let is_edge_sliver = (overlay.left < bounds.left
                && hole.right - bounds.left <= sliver_depth)
                || (overlay.right > bounds.right && bounds.right - hole.left <= sliver_depth)
                || (overlay.top < bounds.top && hole.bottom - bounds.top <= sliver_depth)
                || (overlay.bottom > bounds.bottom && bounds.bottom - hole.top <= sliver_depth);
            (!is_edge_sliver).then_some(hole)
        })
        .collect();
    (bounds, holes)
}

/// Rectangles whose union is `rect` with its corners rounded by `radius`
/// (clamped to half the shorter side): X shapes are unions of rectangles, so
/// each corner row is its own one-pixel strip.
fn rounded_rect_pieces(rect: DeviceRect, radius: i32) -> Vec<DeviceRect> {
    let radius = radius.min(rect.width() / 2).min(rect.height() / 2).max(0);
    if radius == 0 {
        return vec![rect];
    }
    let mut pieces = vec![DeviceRect {
        top: rect.top + radius,
        bottom: rect.bottom - radius,
        ..rect
    }];
    let radius_f64 = f64::from(radius);
    for row in 0..radius {
        // Where the corner's quarter circle crosses the middle of this row.
        let distance_from_center = radius_f64 - f64::from(row) - 0.5;
        let inset = (radius_f64
            - (radius_f64 * radius_f64 - distance_from_center * distance_from_center).sqrt())
        .round() as i32;
        pieces.push(DeviceRect {
            left: rect.left + inset,
            right: rect.right - inset,
            top: rect.top + row,
            bottom: rect.top + row + 1,
        });
        pieces.push(DeviceRect {
            left: rect.left + inset,
            right: rect.right - inset,
            top: rect.bottom - row - 1,
            bottom: rect.bottom - row,
        });
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewport(overlays: Vec<OverlayRect>, scale_factor: f32) -> Viewport {
        Viewport {
            origin_x: 10.,
            origin_y: 20.,
            width: 300.,
            height: 200.,
            scale_factor,
            hidden_reason: None,
            input_blocked: false,
            overlays,
        }
    }

    #[test]
    fn geometry_scales_bounds_and_clips_overlays_to_them() {
        let (bounds, holes) = viewport_geometry(&viewport(
            vec![OverlayRect {
                left: 0.,
                top: 50.,
                right: 60.,
                bottom: 80.,
            }],
            1.5,
        ));
        assert_eq!(
            bounds,
            DeviceRect {
                left: 15,
                top: 30,
                right: 465,
                bottom: 330
            }
        );
        assert_eq!(
            holes,
            vec![DeviceRect {
                left: 15,
                top: 75,
                right: 90,
                bottom: 120
            }]
        );
    }

    #[test]
    fn geometry_ignores_a_dock_handle_reaching_in_across_an_edge() {
        let (_, holes) = viewport_geometry(&viewport(
            vec![
                OverlayRect {
                    left: 7.,
                    top: 0.,
                    right: 13.,
                    bottom: 400.,
                },
                OverlayRect {
                    left: 100.,
                    top: 100.,
                    right: 150.,
                    bottom: 120.,
                },
            ],
            1.,
        ));
        assert_eq!(
            holes,
            vec![DeviceRect {
                left: 100,
                top: 100,
                right: 150,
                bottom: 120
            }]
        );
    }

    #[test]
    fn rounded_pieces_cover_the_rect_except_its_corners() {
        let rect = DeviceRect {
            left: 0,
            top: 0,
            right: 40,
            bottom: 30,
        };
        let pieces = rounded_rect_pieces(rect, 8);
        let covered = |x: i32, y: i32| {
            pieces.iter().any(|piece| {
                x >= piece.left && x < piece.right && y >= piece.top && y < piece.bottom
            })
        };
        assert!(covered(20, 15));
        assert!(covered(20, 0));
        assert!(covered(0, 15));
        assert!(covered(39, 29 - 8));
        for (x, y) in [(0, 0), (39, 0), (0, 29), (39, 29)] {
            assert!(!covered(x, y), "corner ({x}, {y}) should be cut");
        }
        // The pieces never overlap, so a pixel is never subtracted twice.
        let area: i32 = pieces
            .iter()
            .map(|piece| piece.width() * piece.height())
            .sum();
        let covered_pixels = (0..rect.width())
            .flat_map(|x| (0..rect.height()).map(move |y| (x, y)))
            .filter(|(x, y)| covered(*x, *y))
            .count() as i32;
        assert_eq!(area, covered_pixels);
    }

    #[test]
    fn rounded_pieces_fall_back_to_the_rect_when_it_is_too_thin() {
        let rect = DeviceRect {
            left: 5,
            top: 5,
            right: 6,
            bottom: 50,
        };
        assert_eq!(rounded_rect_pieces(rect, 8), vec![rect]);
    }

    #[test]
    fn ai_tabs_need_a_usable_proxy_before_anything_is_loaded() {
        for proxy_endpoint in [
            None,
            Some(""),
            Some("direct://"),
            Some("socks5://127.0.0.1:1"),
        ] {
            let (events, _) = futures::channel::mpsc::unbounded();
            let error = WebView::new(
                DETACHED_HOST,
                WebViewOptions {
                    profile: WebViewProfile::Ai,
                    user_data_folder: std::env::temp_dir().join("fk-webkitgtk-never-created"),
                    proxy_endpoint: proxy_endpoint.map(str::to_string),
                    document_start_script: None,
                    navigation_filter: None,
                    block_downloads: true,
                },
                events,
            )
            .err()
            .unwrap_or_else(|| panic!("an AI tab started with the proxy {proxy_endpoint:?}"));
            // The check runs before GTK is touched, so it holds on machines
            // without WebKitGTK or a display as well.
            assert!(
                format!("{error:#}").contains("egress proxy"),
                "{proxy_endpoint:?}: {error:#}"
            );
            assert!(loaded_runtime().is_none());
        }
    }

    #[test]
    fn only_literal_loopback_addresses_skip_the_proxy() {
        let hosts = PROXY_BYPASS_HOSTS
            .iter()
            .map(|host| host.to_str().unwrap_or_default())
            .collect::<Vec<_>>();
        // GLib matches a name together with all its subdomains, and the egress
        // policy does not exempt `*.localhost`, so no name may be listed.
        assert_eq!(hosts, ["127.0.0.0/8", "::1"]);
    }

    /// Answers both as the origin (`GET /path`) and as an HTTP proxy
    /// (`GET http://host/path`), recording every request target, so one
    /// listener shows which requests went through the proxy.
    struct TestServer {
        port: u16,
        requests: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl TestServer {
        fn start() -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            std::thread::spawn({
                let requests = requests.clone();
                move || {
                    for stream in listener.incoming().flatten() {
                        let requests = requests.clone();
                        std::thread::spawn(move || serve(stream, &requests));
                    }
                }
            });
            Self { port, requests }
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }
    }

    fn serve(mut stream: std::net::TcpStream, requests: &std::sync::Mutex<Vec<String>>) {
        use std::io::{Read as _, Write as _};

        let mut head = Vec::new();
        let mut buffer = [0u8; 1024];
        while !head.windows(4).any(|window| window == b"\r\n\r\n") {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(read) => head.extend_from_slice(&buffer[..read]),
            }
        }
        let head = String::from_utf8_lossy(&head);
        let target = head.split_whitespace().nth(1).unwrap_or("/").to_string();
        requests.lock().unwrap().push(target.clone());
        let path = match target.strip_prefix("http://") {
            Some(rest) => rest
                .find('/')
                .map_or("/", |slash| &rest[slash..])
                .to_string(),
            None => target,
        };
        let (content_type, extra_headers, body) = if path.starts_with("/download") {
            (
                "application/octet-stream",
                "Content-Disposition: attachment; filename=payload.bin\r\n",
                "binary".to_string(),
            )
        } else if path.starts_with("/attachment") {
            // A type WebKit can display, so only the `download-started`
            // backstop can stop it.
            (
                "text/plain",
                "Content-Disposition: attachment; filename=notes.txt\r\n",
                "attached".to_string(),
            )
        } else {
            (
                "text/html; charset=utf-8",
                "",
                "<!doctype html><html><head><title>Harness</title></head>\
                 <body style=\"background:#3060c0\"><p id=\"text\">Hello from the harness</p>\
                 <a id=\"popup-denied\" target=\"_blank\" href=\"http://denied.test/popup\">a</a>\
                 <a id=\"popup-allowed\" target=\"_blank\" href=\"http://harness.test/popup\">b</a>\
                 <input id=\"upload\" type=\"file\">\
                 <iframe srcdoc=\"<p>frame</p>\"></iframe></body></html>"
                    .to_string(),
            )
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\n{extra_headers}\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).ok();
    }

    fn wait_for<T>(
        events: &mut futures::channel::mpsc::UnboundedReceiver<WebViewEvent>,
        what: &str,
        mut matches: impl FnMut(&WebViewEvent) -> Option<T>,
    ) -> T {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            pump_main_loop();
            while let Ok(event) = events.try_recv() {
                if let Some(value) = matches(&event) {
                    return value;
                }
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_result<T>(mut receiver: oneshot::Receiver<Result<T>>, what: &str) -> Result<T> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            pump_main_loop();
            if let Some(result) = receiver.try_recv().unwrap() {
                return result;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn pump_for(duration: Duration) {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            pump_main_loop();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn visible_viewport() -> Viewport {
        Viewport {
            origin_x: 40.,
            origin_y: 60.,
            width: 640.,
            height: 400.,
            scale_factor: 1.,
            hidden_reason: None,
            input_blocked: false,
            overlays: vec![OverlayRect {
                left: 100.,
                top: 100.,
                right: 260.,
                bottom: 180.,
            }],
        }
    }

    /// Asks the X server itself, rather than GDK, which still believes the
    /// reparented popup is a top-level window.
    fn x_parent_and_geometry(
        x_display: Gpointer,
        window: XWindow,
    ) -> (XWindow, (c_int, c_int, c_uint, c_uint)) {
        type QueryTree = unsafe extern "C" fn(
            Gpointer,
            XWindow,
            *mut XWindow,
            *mut XWindow,
            *mut *mut XWindow,
            *mut c_uint,
        ) -> c_int;
        type GetGeometry = unsafe extern "C" fn(
            Gpointer,
            XWindow,
            *mut XWindow,
            *mut c_int,
            *mut c_int,
            *mut c_uint,
            *mut c_uint,
            *mut c_uint,
            *mut c_uint,
        ) -> c_int;
        type Free = unsafe extern "C" fn(Gpointer) -> c_int;
        unsafe {
            let xlib = libloading::Library::new("libX11.so.6").unwrap();
            let query_tree = *xlib.get::<QueryTree>(b"XQueryTree").unwrap();
            let get_geometry = *xlib.get::<GetGeometry>(b"XGetGeometry").unwrap();
            let free = *xlib.get::<Free>(b"XFree").unwrap();
            let (mut root, mut parent, mut children, mut child_count) = (0, 0, ptr::null_mut(), 0);
            assert_ne!(
                query_tree(
                    x_display,
                    window,
                    &mut root,
                    &mut parent,
                    &mut children,
                    &mut child_count
                ),
                0
            );
            if !children.is_null() {
                free(children.cast());
            }
            let (mut x, mut y, mut width, mut height, mut border, mut depth) = (0, 0, 0, 0, 0, 0);
            assert_ne!(
                get_geometry(
                    x_display,
                    window,
                    &mut root,
                    &mut x,
                    &mut y,
                    &mut width,
                    &mut height,
                    &mut border,
                    &mut depth
                ),
                0
            );
            (parent, (x, y, width, height))
        }
    }

    type SetDataFull =
        unsafe extern "C" fn(Gpointer, *const c_char, Gpointer, unsafe extern "C" fn(Gpointer));

    /// Object data is released when the object is finalised; weak references
    /// would already fire when GTK merely disposes of it.
    unsafe extern "C" fn on_window_finalized(finalized: Gpointer) {
        unsafe { &*finalized.cast::<Cell<bool>>() }.set(true);
    }

    /// A function the backend itself has no use for. The library is already
    /// loaded by the runtime, so it stays open.
    ///
    /// # Safety
    ///
    /// `T` must match the C declaration of `name`.
    unsafe fn test_function<T: Copy>(library: &CStr, name: &CStr) -> T {
        unsafe {
            let library = libloading::Library::new(library.to_str().unwrap()).unwrap();
            *library.get::<T>(name.to_bytes()).unwrap()
        }
    }

    /// How many criticals and warnings GObject, GTK and GDK log while `work`
    /// runs. They are still printed.
    fn count_glib_complaints(work: impl FnOnce()) -> usize {
        type LogFunc = unsafe extern "C" fn(*const c_char, c_int, *const c_char, Gpointer);
        type SetHandler = unsafe extern "C" fn(*const c_char, c_int, LogFunc, Gpointer) -> c_uint;
        type RemoveHandler = unsafe extern "C" fn(*const c_char, c_uint);
        const CRITICAL_AND_WARNING: c_int = (1 << 3) | (1 << 4);

        unsafe extern "C" fn count(
            domain: *const c_char,
            level: c_int,
            message: *const c_char,
            complaints: Gpointer,
        ) {
            let complaints = unsafe { &*complaints.cast::<Cell<usize>>() };
            complaints.set(complaints.get() + 1);
            unsafe {
                test_function::<LogFunc>(c"libglib-2.0.so.0", c"g_log_default_handler")(
                    domain,
                    level,
                    message,
                    ptr::null_mut(),
                )
            };
        }

        let complaints = Cell::new(0usize);
        let data: Gpointer = (&complaints as *const Cell<usize>).cast_mut().cast();
        let domains = [c"GLib-GObject", c"Gtk", c"Gdk"];
        unsafe {
            let set_handler =
                test_function::<SetHandler>(c"libglib-2.0.so.0", c"g_log_set_handler");
            let remove_handler =
                test_function::<RemoveHandler>(c"libglib-2.0.so.0", c"g_log_remove_handler");
            let handlers = domains
                .map(|domain| set_handler(domain.as_ptr(), CRITICAL_AND_WARNING, count, data));
            work();
            for (domain, handler) in domains.iter().zip(handlers) {
                remove_handler(domain.as_ptr(), handler);
            }
        }
        complaints.get()
    }

    fn ai_setting(view: Gpointer, property: &CStr) -> bool {
        type GetSettings = unsafe extern "C" fn(Gpointer) -> Gpointer;
        type ObjectGet = unsafe extern "C" fn(Gpointer, *const c_char, ...);
        unsafe {
            let settings = test_function::<GetSettings>(
                c"libwebkit2gtk-4.1.so.0",
                c"webkit_web_view_get_settings",
            )(view);
            let mut value: Gboolean = TRUE;
            test_function::<ObjectGet>(c"libgobject-2.0.so.0", c"g_object_get")(
                settings,
                property.as_ptr(),
                &mut value as *mut Gboolean,
                ptr::null::<c_char>(),
            );
            value != FALSE
        }
    }

    fn load_finished(event: &WebViewEvent) -> Option<bool> {
        match event {
            WebViewEvent::LoadFinished { succeeded } => Some(*succeeded),
            _ => None,
        }
    }

    /// Drives real WebKitGTK surfaces against a loopback server. Embedded
    /// under X11 (`env -u WAYLAND_DISPLAY`), detached under Wayland:
    /// `cargo test -p browser_panel webkitgtk -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs a display and WebKitGTK 4.1"]
    fn webkitgtk_surfaces_load_filter_and_capture() {
        let server = TestServer::start();
        let origin = format!("http://127.0.0.1:{}", server.port);
        let folder = std::env::temp_dir().join(format!("fk-webkitgtk-test-{}", std::process::id()));
        let embedded = std::env::var_os("WAYLAND_DISPLAY").is_none();

        let new_host = || {
            let runtime = runtime(0).unwrap();
            let functions = &runtime.functions;
            unsafe {
                let window = (functions.gtk_window_new)(GTK_WINDOW_TOPLEVEL);
                (functions.gtk_window_set_default_size)(window, 900, 700);
                (functions.gtk_widget_show)(window);
                let gdk_window = (functions.gtk_widget_get_window)(window);
                (
                    window,
                    (functions.gdk_x11_window_get_xid.unwrap())(gdk_window) as isize,
                )
            }
        };
        let host = if embedded {
            new_host().1
        } else {
            DETACHED_HOST
        };

        let (user_events_tx, mut user_events) = futures::channel::mpsc::unbounded();
        let user = WebView::new(
            host,
            WebViewOptions {
                profile: WebViewProfile::User,
                user_data_folder: folder.join("user"),
                proxy_endpoint: None,
                document_start_script: None,
                navigation_filter: None,
                block_downloads: false,
            },
            user_events_tx,
        )
        .unwrap();
        assert_eq!(user.is_detached(), !embedded);
        wait_for(&mut user_events, "the user surface", |event| {
            matches!(event, WebViewEvent::Ready).then_some(())
        });
        // Only an embedded page has holes to cut, and a new one waits a frame.
        let deferred = user.sync_viewport(visible_viewport(), true);
        assert_eq!(deferred, embedded);
        user.apply_deferred_clip();
        user.set_background_color(0x20, 0x20, 0x20);
        user.set_dark_mode(true);
        user.navigate(&format!("{origin}/page"));
        assert!(wait_for(&mut user_events, "the user page", load_finished));

        assert_eq!(
            wait_result(user.execute_script("document.title"), "the title").unwrap(),
            "\"Harness\""
        );
        assert_eq!(
            wait_result(
                user.execute_script("var answer = 3; answer * 2"),
                "statements"
            )
            .unwrap(),
            "6"
        );
        assert_eq!(
            wait_result(user.execute_script("({a: [1, 'two']})"), "an object").unwrap(),
            "{\"a\":[1,\"two\"]}"
        );
        assert_eq!(
            wait_result(user.execute_script("undefined"), "undefined").unwrap(),
            "null"
        );
        assert_eq!(
            wait_result(
                user.execute_script("typeof window.__fkInjected"),
                "no injection"
            )
            .unwrap(),
            "\"undefined\""
        );
        let thrown = wait_result(
            user.execute_script("throw new Error('boom')"),
            "an exception",
        );
        assert!(format!("{:#}", thrown.unwrap_err()).contains("boom"));

        pump_for(Duration::from_millis(500));
        let png = wait_result(user.capture_png(), "a screenshot").unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"), "not a PNG");
        let screenshot = std::env::temp_dir().join(format!(
            "fk-webkitgtk-test-{}.png",
            if embedded { "embedded" } else { "detached" }
        ));
        std::fs::write(&screenshot, &png).unwrap();
        eprintln!(
            "screenshot: {} bytes, saved to {}",
            png.len(),
            screenshot.display()
        );

        if embedded {
            let embedding = user.inner.embedding.unwrap();
            let (parent, geometry) = x_parent_and_geometry(embedding.x_display, embedding.surface);
            assert_eq!(
                parent, host as XWindow,
                "the page window is not a child of the host"
            );
            assert_eq!(geometry, (40, 60, 640, 400));

            user.focus_page();
            let focused = wait_for(&mut user_events, "the page focus", |event| match event {
                WebViewEvent::PageFocusChanged(focused) => Some(*focused),
                _ => None,
            });
            assert!(focused);
            user.release_keyboard_focus();
            let focused = wait_for(
                &mut user_events,
                "the focus to go back",
                |event| match event {
                    WebViewEvent::PageFocusChanged(focused) => Some(*focused),
                    _ => None,
                },
            );
            assert!(!focused);

            // Asking an unmapped window for the keyboard is an X error, which
            // GDK answers by ending the process unless it is trapped.
            user.hide("the test hides it");
            user.focus_page();
            user.release_keyboard_focus();
            pump_for(Duration::from_millis(300));
            if user.sync_viewport(visible_viewport(), true) {
                user.apply_deferred_clip();
            }
        }

        let (ai_events_tx, mut ai_events) = futures::channel::mpsc::unbounded();
        let ai = WebView::new(
            host,
            WebViewOptions {
                profile: WebViewProfile::Ai,
                user_data_folder: folder.join("ai"),
                proxy_endpoint: Some(origin.clone()),
                document_start_script: Some("window.__fkInjected = 42;".to_string()),
                navigation_filter: Some(Rc::new(|url: &str| {
                    if url.contains("denied.test") {
                        NavigationDecision::Deny("denied.test is not allowed".to_string())
                    } else {
                        NavigationDecision::Allow
                    }
                })),
                block_downloads: true,
            },
            ai_events_tx,
        )
        .unwrap();
        wait_for(&mut ai_events, "the AI surface", |event| {
            matches!(event, WebViewEvent::Ready).then_some(())
        });
        if ai.sync_viewport(visible_viewport(), false) {
            ai.apply_deferred_clip();
        }

        ai.navigate("http://harness.test/page");
        assert!(wait_for(&mut ai_events, "the proxied page", load_finished));
        assert!(
            server
                .requests()
                .iter()
                .any(|target| target == "http://harness.test/page"),
            "the AI tab did not go through the proxy: {:?}",
            server.requests()
        );
        assert_eq!(
            wait_result(ai.execute_script("window.__fkInjected"), "the injection").unwrap(),
            "42"
        );
        assert_eq!(
            wait_result(ai.execute_script("typeof RTCPeerConnection"), "WebRTC").unwrap(),
            "\"undefined\"",
            "WebRTC's UDP would go around the proxy"
        );
        // Many builds lack WebRTC altogether, so the page alone cannot show
        // that the settings turn it off where it exists.
        for property in [c"enable-webrtc", c"enable-media-stream"] {
            assert!(
                !ai_setting(ai.inner.view, property),
                "{property:?} is on in an AI tab"
            );
        }

        // `target="_blank"` asks the filter too, and an allowed one opens in
        // place.
        wait_result(
            ai.execute_script("document.getElementById('popup-denied').click()"),
            "the denied popup click",
        )
        .unwrap();
        let blocked = wait_for(&mut ai_events, "the blocked popup", |event| match event {
            WebViewEvent::NavigationBlocked { url, .. } => Some(url.clone()),
            WebViewEvent::OpenInPlace(url) => panic!("a denied popup opened: {url}"),
            _ => None,
        });
        assert_eq!(blocked, "http://denied.test/popup");
        wait_result(
            ai.execute_script("document.getElementById('popup-allowed').click()"),
            "the allowed popup click",
        )
        .unwrap();
        let opened = wait_for(&mut ai_events, "the allowed popup", |event| match event {
            WebViewEvent::OpenInPlace(url) => Some(url.clone()),
            _ => None,
        });
        assert_eq!(opened, "http://harness.test/popup");

        // GTK's print dialog runs a nested main loop, which would stall the
        // editor until the user closes it.
        assert_eq!(
            wait_result(ai.execute_script("window.print(); 1"), "printing").unwrap(),
            "1"
        );

        // `*.localhost` is not loopback to the egress policy, so it goes to
        // the proxy, which decides, instead of straight to whatever it
        // resolves to.
        ai.navigate(&format!("http://sub.localhost:{}/page", server.port));
        assert!(wait_for(
            &mut ai_events,
            "the sub.localhost page",
            load_finished
        ));
        assert!(
            server
                .requests()
                .contains(&format!("http://sub.localhost:{}/page", server.port)),
            "*.localhost bypassed the proxy: {:?}",
            server.requests()
        );

        // A user surface on the AI profile's folder would share its web
        // context, and with it set the AI surfaces' proxy.
        let (shared_events, _) = futures::channel::mpsc::unbounded();
        let shared = WebView::new(
            host,
            WebViewOptions {
                profile: WebViewProfile::User,
                user_data_folder: folder.join("ai"),
                proxy_endpoint: None,
                document_start_script: None,
                navigation_filter: None,
                block_downloads: false,
            },
            shared_events,
        );
        assert!(shared.is_err(), "a user surface shared the AI profile");

        ai.navigate(&format!("{origin}/page?direct"));
        assert!(wait_for(&mut ai_events, "the loopback page", load_finished));
        assert!(
            server
                .requests()
                .iter()
                .any(|target| target == "/page?direct"),
            "loopback should bypass the proxy: {:?}",
            server.requests()
        );

        // Scripts the backend runs count as a user gesture, so this click
        // would open GTK's file dialog; an AI tab cancels it instead.
        wait_result(
            ai.execute_script(
                "(() => { window.__upload = 'pending'; \
                 const input = document.getElementById('upload'); \
                 input.oncancel = () => { window.__upload = 'cancelled'; }; \
                 input.click(); return 1; })()",
            ),
            "the file input click",
        )
        .unwrap();
        // A loopback page is a secure context, so geolocation asks for the
        // permission rather than failing outright.
        wait_result(
            ai.execute_script(
                "(() => { window.__geolocation = 'pending'; \
                 navigator.geolocation.getCurrentPosition(\
                 () => { window.__geolocation = 'granted'; }, \
                 (error) => { window.__geolocation = 'denied:' + error.code; }); return 1; })()",
            ),
            "the geolocation request",
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let (upload, geolocation) = loop {
            pump_for(Duration::from_millis(100));
            let upload = wait_result(ai.execute_script("window.__upload"), "the upload").unwrap();
            let geolocation =
                wait_result(ai.execute_script("window.__geolocation"), "the location").unwrap();
            let settled = upload != "\"pending\"" && geolocation != "\"pending\"";
            if settled || Instant::now() > deadline {
                break (upload, geolocation);
            }
        };
        assert_eq!(
            upload, "\"cancelled\"",
            "the file chooser was not cancelled"
        );
        assert_eq!(geolocation, "\"denied:1\"", "the permission was not denied");

        ai.navigate("http://denied.test/secret");
        let blocked = wait_for(
            &mut ai_events,
            "the blocked navigation",
            |event| match event {
                WebViewEvent::NavigationBlocked { url, reason } => {
                    Some((url.clone(), reason.clone()))
                }
                _ => None,
            },
        );
        assert_eq!(blocked.0, "http://denied.test/secret");
        assert!(blocked.1.contains("not allowed"));
        assert!(
            !server
                .requests()
                .iter()
                .any(|target| target.contains("denied.test")),
            "a denied navigation reached the network"
        );

        ai.navigate("http://harness.test/download");
        let downloaded = wait_for(
            &mut ai_events,
            "the blocked download",
            |event| match event {
                WebViewEvent::DownloadBlocked { url } => Some(url.clone()),
                _ => None,
            },
        );
        assert_eq!(downloaded, "http://harness.test/download");

        ai.navigate("http://harness.test/attachment");
        let downloaded = wait_for(
            &mut ai_events,
            "the blocked attachment",
            |event| match event {
                WebViewEvent::DownloadBlocked { url } => Some(url.clone()),
                _ => None,
            },
        );
        assert_eq!(downloaded, "http://harness.test/attachment");

        let png = wait_result(ai.capture_png(), "the AI screenshot").unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));

        if embedded {
            // gpui can destroy its window before the surface inside it is
            // dropped; X then destroys the page's window along with it, and
            // GTK destroys the `GtkWindow`.
            let (doomed_window, doomed_host) = new_host();
            let (doomed_events_tx, mut doomed_events) = futures::channel::mpsc::unbounded();
            let doomed = WebView::new(
                doomed_host,
                WebViewOptions {
                    profile: WebViewProfile::User,
                    user_data_folder: folder.join("user"),
                    proxy_endpoint: None,
                    document_start_script: None,
                    navigation_filter: None,
                    block_downloads: false,
                },
                doomed_events_tx,
            )
            .unwrap();
            let window_finalized = Box::into_raw(Box::new(Cell::new(false)));
            unsafe {
                test_function::<SetDataFull>(c"libgobject-2.0.so.0", c"g_object_set_data_full")(
                    doomed.inner.window,
                    c"fk-test-finalized".as_ptr(),
                    window_finalized.cast(),
                    on_window_finalized,
                )
            };
            assert!(!doomed.sync_viewport(viewport(Vec::new(), 1.), true));
            // X only lets a window take the focus once it is mapped.
            pump_for(Duration::from_millis(300));
            doomed.focus_page();
            assert!(wait_for(
                &mut doomed_events,
                "the doomed page's focus",
                |event| match event {
                    WebViewEvent::PageFocusChanged(focused) => Some(*focused),
                    _ => None,
                }
            ));
            unsafe { (doomed.inner.functions().gtk_widget_destroy)(doomed_window) };
            pump_for(Duration::from_millis(500));
            assert!(
                !unsafe { &*window_finalized }.get(),
                "GTK freed the page's window while the surface still uses it"
            );
            doomed.focus_page();
            // A pointer the surface uses after GTK freed it shows up as a
            // failed type check in GObject or GTK.
            let complaints = count_glib_complaints(|| drop(doomed));
            assert_eq!(complaints, 0, "dropping the surface touched freed objects");
            pump_for(Duration::from_millis(500));
            let window_finalized = unsafe { Box::from_raw(window_finalized) };
            assert!(window_finalized.get(), "the surface leaked its window");
        }

        // A reply still owed when the last surface goes away arrives anyway.
        let orphaned = ai.execute_script("document.title");
        drop(ai);
        drop(user);
        wait_result(orphaned, "the reply for a closed surface").ok();
        assert!(!pump_main_loop(), "pumping stops with the last surface");
        eprintln!("requests seen: {:?}", server.requests());
        std::fs::remove_dir_all(&folder).ok();
    }
}
