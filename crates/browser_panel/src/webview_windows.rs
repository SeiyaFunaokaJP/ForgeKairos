//! WebView2 host for [`crate::BrowserView`] (ADR-F19).
//!
//! The controller is created as a child of the GPUI top-level HWND, so every
//! COM object here lives on the UI thread and is deliberately `!Send`. WebView2
//! delivers its callbacks on that same thread, but without a GPUI context, so
//! everything the view needs to react to is forwarded through an unbounded
//! channel that the view drains from a GPUI task.
//!
//! Several browsing surfaces can exist at once (the dock panel plus any number
//! of editor tabs), and all surfaces of one profile share one
//! [`ICoreWebView2Environment`]: a user data folder can only be opened by one
//! environment, so a second one over the same folder fails outright. AI tabs
//! use a second environment of their own ([`Environments`]).

use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    ffi::{OsString, c_void},
    path::PathBuf,
    rc::{Rc, Weak},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow, bail};
use browser_control::{KeyModifier, MouseButton};
use futures::channel::{mpsc::UnboundedSender, oneshot};
use gpui::{SharedString, Window};
use util::ResultExt as _;
use webview2_com::{
    AcceleratorKeyPressedEventHandler, AddScriptToExecuteOnDocumentCreatedCompletedHandler,
    BasicAuthenticationRequestedEventHandler, CallDevToolsProtocolMethodCompletedHandler,
    CapturePreviewCompletedHandler, CoreWebView2EnvironmentOptions,
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    DocumentTitleChangedEventHandler, DownloadStartingEventHandler, ExecuteScriptCompletedHandler,
    FocusChangedEventHandler, HistoryChangedEventHandler,
    Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG, COREWEBVIEW2_COLOR,
        COREWEBVIEW2_KEY_EVENT_KIND, COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN,
        COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC, COREWEBVIEW2_PERMISSION_KIND,
        COREWEBVIEW2_PERMISSION_STATE_DENY, COREWEBVIEW2_PREFERRED_COLOR_SCHEME,
        COREWEBVIEW2_PREFERRED_COLOR_SCHEME_DARK, COREWEBVIEW2_PREFERRED_COLOR_SCHEME_LIGHT,
        COREWEBVIEW2_PROCESS_FAILED_KIND, COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED,
        COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED, COREWEBVIEW2_PROCESS_FAILED_REASON,
        COREWEBVIEW2_PROCESS_FAILED_REASON_CRASHED,
        COREWEBVIEW2_PROCESS_FAILED_REASON_LAUNCH_FAILED,
        COREWEBVIEW2_PROCESS_FAILED_REASON_OUT_OF_MEMORY,
        COREWEBVIEW2_PROCESS_FAILED_REASON_PROFILE_DELETED,
        COREWEBVIEW2_PROCESS_FAILED_REASON_TERMINATED,
        COREWEBVIEW2_PROCESS_FAILED_REASON_UNEXPECTED,
        COREWEBVIEW2_PROCESS_FAILED_REASON_UNRESPONSIVE, COREWEBVIEW2_SCRIPT_DIALOG_KIND,
        COREWEBVIEW2_SCRIPT_DIALOG_KIND_BEFOREUNLOAD, CreateCoreWebView2EnvironmentWithOptions,
        ICoreWebView2, ICoreWebView2_4, ICoreWebView2_10, ICoreWebView2_13,
        ICoreWebView2Controller, ICoreWebView2Controller2, ICoreWebView2Controller3,
        ICoreWebView2Environment, ICoreWebView2EnvironmentOptions,
        ICoreWebView2ProcessFailedEventArgs, ICoreWebView2ProcessFailedEventArgs2,
    },
    NavigationCompletedEventHandler, NavigationStartingEventHandler,
    NewWindowRequestedEventHandler, PermissionRequestedEventHandler, ProcessFailedEventHandler,
    ScriptDialogOpeningEventHandler, SourceChangedEventHandler, take_pwstr,
};
use windows::{
    Win32::{
        Foundation::{E_POINTER, HGLOBAL, HWND, LPARAM, POINT, RECT},
        Graphics::Gdi::{
            CombineRgn, CreateRectRgn, CreateRoundRectRgn, DeleteObject, RGN_DIFF, RGN_ERROR,
            ScreenToClient, SetWindowRgn,
        },
        System::Com::{IStream, STREAM_SEEK_SET, StructuredStorage::CreateStreamOnHGlobal},
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

/// Handed a profile's environment once it exists, or the reason it does not.
/// The error is a message rather than a `windows::core::Error` so that one
/// failure can be reported to every queued surface.
type EnvironmentWaiter = Box<dyn FnOnce(Result<ICoreWebView2Environment, String>)>;

#[derive(Default)]
enum EnvironmentSlot {
    /// Nothing has asked for an environment yet, or the last attempt failed.
    /// A failed attempt resets to this so a retry is possible once the user
    /// installs the runtime.
    #[default]
    Idle,
    Creating(Vec<EnvironmentWaiter>),
    Ready(ICoreWebView2Environment),
}

/// What a profile's environment was created with. WebView2 has no way to
/// change the user data folder or the browser arguments after creation, so a
/// later surface asking for something else can only be warned about (the
/// user's profile) or refused (the AI profile).
#[derive(Clone, Debug, PartialEq, Eq)]
struct EnvironmentConfiguration {
    user_data_folder: PathBuf,
    proxy_endpoint: Option<String>,
}

#[derive(Default)]
struct ProfileEnvironment {
    slot: EnvironmentSlot,
    configuration: Option<EnvironmentConfiguration>,
}

impl ProfileEnvironment {
    /// The configuration of an environment that exists or is being created;
    /// one whose creation failed no longer constrains anything.
    fn live_configuration(&self) -> Option<&EnvironmentConfiguration> {
        match self.slot {
            EnvironmentSlot::Idle => None,
            EnvironmentSlot::Creating(_) | EnvironmentSlot::Ready(_) => self.configuration.as_ref(),
        }
    }
}

/// One environment per profile: each has its own user data folder (cookies,
/// storage, cache) and its own browser process, which is what keeps the
/// user's sign-ins out of reach of AI tabs (ADR-F89). Within a profile every
/// surface shares the environment, because a user data folder can only be
/// opened by one environment and a second one over the same folder fails.
#[derive(Default)]
struct Environments {
    user: ProfileEnvironment,
    ai: ProfileEnvironment,
}

impl Environments {
    fn profile(&self, profile: WebViewProfile) -> &ProfileEnvironment {
        match profile {
            WebViewProfile::User => &self.user,
            WebViewProfile::Ai => &self.ai,
        }
    }

    fn profile_mut(&mut self, profile: WebViewProfile) -> &mut ProfileEnvironment {
        match profile {
            WebViewProfile::User => &mut self.user,
            WebViewProfile::Ai => &mut self.ai,
        }
    }
}

thread_local! {
    /// WebView2's COM objects are UI-thread affine, so the environments are
    /// thread local rather than process-wide globals.
    static ENVIRONMENTS: RefCell<Environments> = RefCell::new(Environments::default());
}

/// Outcome of queueing a waiter; `Ready` hands the waiter back so it can be
/// invoked outside the `RefCell` borrow.
enum EnvironmentRequest {
    Ready(ICoreWebView2Environment, EnvironmentWaiter),
    Pending,
    Start,
}

fn queue_environment_waiter(
    profile: WebViewProfile,
    waiter: EnvironmentWaiter,
) -> EnvironmentRequest {
    ENVIRONMENTS.with(|environments| {
        let mut environments = environments.borrow_mut();
        let slot = &mut environments.profile_mut(profile).slot;
        match slot {
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

/// Publishes the result of a profile's one in-flight creation to every queued
/// waiter. On failure the slot goes back to `Idle` so a later surface can try
/// again.
fn finish_environment(profile: WebViewProfile, outcome: Result<ICoreWebView2Environment, String>) {
    let waiters = ENVIRONMENTS.with(|environments| {
        let mut environments = environments.borrow_mut();
        let slot = &mut environments.profile_mut(profile).slot;
        let waiters = match std::mem::take(slot) {
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

/// Whether a surface asking for `requested` may use the `profile` environment
/// that was created with `created`. Only the AI profile refuses: its proxy is
/// the network restriction itself, so an AI tab must never run on an
/// environment that points somewhere else than the proxy it was asked to use.
fn check_configuration_against_existing(
    profile: WebViewProfile,
    created: &EnvironmentConfiguration,
    requested: &EnvironmentConfiguration,
) -> Result<()> {
    if created.user_data_folder != requested.user_data_folder {
        log::warn!(
            "WebView2: the {profile:?} environment already uses {}; ignoring the request for {}",
            created.user_data_folder.display(),
            requested.user_data_folder.display()
        );
    }
    if created.proxy_endpoint != requested.proxy_endpoint {
        match profile {
            WebViewProfile::User => log::warn!(
                "WebView2: the {profile:?} environment was created with proxy {:?}; \
                 restart Zed for browser.use_egress_proxy changes to take effect",
                created.proxy_endpoint
            ),
            WebViewProfile::Ai => bail!(
                "the AI browser profile was started with the egress proxy at {}, \
                 but the egress proxy is now at {}; restart the app to open AI tabs again",
                created.proxy_endpoint.as_deref().unwrap_or("none"),
                requested.proxy_endpoint.as_deref().unwrap_or("none"),
            ),
        }
    }
    Ok(())
}

fn check_environment_configuration(
    profile: WebViewProfile,
    requested: &EnvironmentConfiguration,
) -> Result<()> {
    ENVIRONMENTS.with(|environments| {
        let environments = environments.borrow();
        let other_profile = match profile {
            WebViewProfile::User => WebViewProfile::Ai,
            WebViewProfile::Ai => WebViewProfile::User,
        };
        // Two environments over one folder with different arguments fail in
        // WebView2 anyway; refusing here keeps the reason readable, and keeps
        // the two profiles' cookies apart even if WebView2 ever stopped
        // refusing.
        if let Some(other) = environments.profile(other_profile).live_configuration()
            && other.user_data_folder == requested.user_data_folder
        {
            bail!(
                "the {profile:?} and {other_profile:?} browser profiles cannot share the user data folder {}",
                requested.user_data_folder.display()
            );
        }
        match environments.profile(profile).live_configuration() {
            Some(created) => check_configuration_against_existing(profile, created, requested),
            None => Ok(()),
        }
    })
}

/// Hands `waiter` the WebView2 environment of `profile`, creating it on first
/// use. Errors returned here are synchronous rejections of the request; a
/// failure that arrives later is delivered to the waiter instead.
fn with_profile_environment(
    profile: WebViewProfile,
    configuration: EnvironmentConfiguration,
    waiter: EnvironmentWaiter,
) -> Result<()> {
    check_environment_configuration(profile, &configuration)?;
    // Computed before queueing, so that a refusal leaves no waiter behind.
    let arguments = command_line(&browser_arguments(
        profile,
        configuration.proxy_endpoint.as_deref(),
    )?);
    match queue_environment_waiter(profile, waiter) {
        EnvironmentRequest::Ready(environment, waiter) => {
            waiter(Ok(environment));
            Ok(())
        }
        EnvironmentRequest::Pending => Ok(()),
        EnvironmentRequest::Start => {
            ENVIRONMENTS.with(|environments| {
                environments.borrow_mut().profile_mut(profile).configuration =
                    Some(configuration.clone());
            });
            let result = create_environment(profile, &configuration, &arguments);
            if let Err(error) = &result {
                finish_environment(profile, Err(format!("{error:#}")));
            }
            result
        }
    }
}

/// Chromium's implicit proxy bypass covers not only loopback but also
/// link-local space (169.254.0.0/16, fe80::/10), which would let an AI tab
/// reach, say, a cloud metadata endpoint without asking the egress proxy.
/// `<-loopback>` drops the implicit rules, and the explicit ones put loopback
/// back: dev servers on this machine are always allowed by the policy, and
/// their websockets keep working without a detour through the proxy.
///
/// Exactly what the egress policy calls loopback (`http_client::egress`), and
/// no more: `*.localhost` goes through the proxy like any other name, so that
/// the allowlist decides about it in an AI tab just as it does everywhere
/// else.
const AI_PROXY_BYPASS_LIST: &str = "<-loopback>;localhost;127.0.0.1/8;[::1]";

/// Everything but loopback goes through the proxy, which resolves the names
/// itself, so the AI profile's browser has nothing to resolve. Chromium does
/// anyway: its automatic DNS-over-HTTPS upgrade looks up the system
/// resolver's DoH server and connects to it straight, because DoH requests
/// always bypass the proxy. With every other name "not found", no lookup and
/// no such connection leaves the machine.
const AI_HOST_RESOLVER_RULES: &str =
    "MAP * ~NOTFOUND, EXCLUDE localhost, EXCLUDE 127.*, EXCLUDE ::1, EXCLUDE [::1]";

/// The browser arguments of a profile's environment. Chromium reads them once
/// at browser-process start; there is no runtime equivalent in the WebView2
/// API.
fn browser_arguments(profile: WebViewProfile, proxy_endpoint: Option<&str>) -> Result<Vec<String>> {
    match profile {
        WebViewProfile::User => Ok(proxy_endpoint
            .map(|proxy_endpoint| format!("--proxy-server={proxy_endpoint}"))
            .into_iter()
            .collect()),
        WebViewProfile::Ai => {
            let Some(proxy_endpoint) = proxy_endpoint else {
                bail!("AI tabs need the egress proxy, and it is not running");
            };
            let arguments = vec![
                format!("--proxy-server={proxy_endpoint}"),
                format!("--proxy-bypass-list={AI_PROXY_BYPASS_LIST}"),
                format!("--host-resolver-rules={AI_HOST_RESOLVER_RULES}"),
                // WebRTC would otherwise open UDP sockets straight to the
                // peers, which no HTTP proxy sees. Chrome's
                // `--force-webrtc-ip-handling-policy` has no effect in
                // WebView2; this content-level switch does.
                "--webrtc-ip-handling-policy=disable_non_proxied_udp".to_string(),
                // QUIC is UDP as well. Chromium does not speak it through an
                // HTTP proxy, so it is off already; switched off outright so
                // that no later change of that rule can open a path around the
                // proxy.
                "--disable-quic".to_string(),
                // An AI tab works while nobody looks at it: a background tab,
                // a closed panel, a minimised or covered window (its page is
                // parked out of sight rather than hidden, see `placement`).
                // These are Chromium's switches against slowing down a page
                // it thinks nobody sees. WebView2 154 kept parked, covered and
                // minimised pages at full speed without them too; they keep it
                // that way should a runtime start to track occlusion.
                "--disable-background-timer-throttling".to_string(),
                "--disable-backgrounding-occluded-windows".to_string(),
                "--disable-renderer-backgrounding".to_string(),
            ];
            #[cfg(test)]
            let arguments = [
                arguments,
                TEST_BROWSER_ARGUMENTS.with(|extra| extra.borrow().clone()),
            ]
            .concat();
            Ok(arguments)
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Extra arguments for the AI profile's browser, for the live tests (a net log).
    static TEST_BROWSER_ARGUMENTS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// WebView2 splits its additional browser arguments like a Windows command
/// line, so an argument with spaces in it has to be quoted to stay one.
fn command_line(arguments: &[String]) -> String {
    arguments
        .iter()
        .map(|argument| {
            if argument.contains(' ') {
                format!("\"{argument}\"")
            } else {
                argument.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Environment variables that WebView2 reads at environment creation and that
/// would replace what an AI tab's environment is created with: another user
/// data folder (the user's own profile, for instance), other browser
/// arguments (no proxy), or another browser build. Returns the names of those
/// that are set.
fn environment_overrides(read_variable: impl Fn(&str) -> Option<OsString>) -> Vec<&'static str> {
    [
        "WEBVIEW2_USER_DATA_FOLDER",
        "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
        "WEBVIEW2_BROWSER_EXECUTABLE_FOLDER",
    ]
    .into_iter()
    .filter(|name| read_variable(name).is_some_and(|value| !value.is_empty()))
    .collect()
}

fn check_environment_overrides(
    profile: WebViewProfile,
    read_variable: impl Fn(&str) -> Option<OsString>,
) -> Result<()> {
    if profile != WebViewProfile::Ai {
        return Ok(());
    }
    let overrides = environment_overrides(read_variable);
    if !overrides.is_empty() {
        bail!(
            "AI tabs are unavailable while {} is set: WebView2 would use it in place of \
             the AI profile's own folder and network restriction",
            overrides.join(", ")
        );
    }
    Ok(())
}

fn create_environment(
    profile: WebViewProfile,
    configuration: &EnvironmentConfiguration,
    arguments: &str,
) -> Result<()> {
    let options = CoreWebView2EnvironmentOptions::default();
    if !arguments.is_empty() {
        unsafe {
            options.set_additional_browser_arguments(arguments.to_string());
        }
    }
    let options: ICoreWebView2EnvironmentOptions = options.into();
    log::info!(
        "WebView2: requesting the {profile:?} environment (user data folder {}, arguments {arguments:?})",
        configuration.user_data_folder.display(),
    );
    let user_data_folder = HSTRING::from(configuration.user_data_folder.as_os_str());

    let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(
        move |result, environment| {
            match result.and_then(|()| environment.ok_or_else(|| E_POINTER.into())) {
                Ok(environment) => {
                    log::info!("WebView2: the {profile:?} environment is ready");
                    finish_environment(profile, Ok(environment));
                }
                Err(error) => {
                    log::error!("WebView2 {profile:?} environment creation failed: {error:?}");
                    finish_environment(profile, Err(format!("{error}")));
                }
            }
            Ok(())
        },
    ));

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
    profile: WebViewProfile,
    document_start_script: Option<String>,
    navigation_filter: Option<Rc<dyn Fn(&str) -> NavigationDecision>>,
    block_downloads: bool,
    /// The answers owed for scripts, captures and inputs WebView2 is still
    /// working on. Weak: the completion handler holds the answer, and once it
    /// has run nothing is owed. Answered with the reason when the page's
    /// process goes away, since WebView2 may never call those handlers then.
    in_flight: RefCell<Vec<Weak<dyn PendingAnswer>>>,
    state: RefCell<State>,
}

/// Work that needs the page: run with the webview once it may be used, or with
/// the reason it never will be.
type PageOperation = Box<dyn FnOnce(Result<ICoreWebView2, String>)>;

#[derive(Default)]
struct State {
    controller: Option<ICoreWebView2Controller>,
    webview: Option<ICoreWebView2>,
    /// The environment the controller was created in, so that a browser
    /// process that went away can take that environment with it.
    environment: Option<ICoreWebView2Environment>,
    /// Whether the page may be navigated and scripted: the controller exists
    /// and the document start script, if any, is registered. A page loaded
    /// before that would run without the script, so until then navigations
    /// and page operations wait.
    page_ready: bool,
    /// Why the surface will never have a page, once that is known; later page
    /// operations fail with it right away instead of waiting.
    failure: Option<String>,
    pending_operations: Vec<PageOperation>,
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
    /// Whether the page is on screen. Off screen, a user tab's page is hidden
    /// and an AI tab's page is parked (see [`placement`]).
    visible: bool,
    /// The size, in physical pixels, the surface last had: where an AI tab's
    /// page keeps being laid out while it is parked.
    last_size: Option<(i32, i32)>,
    applied_bounds: Option<RECT>,
    /// The `(bounds, holes)` the current window regions were built from. The
    /// holes are relative to the host window, so a move of the surface
    /// invalidates them just as much as a change of the overlays does.
    applied_clip: Option<(RECT, Vec<RECT>)>,
    applied_scale: Option<f64>,
    /// The last `visible` acted on.
    applied_visible: Option<bool>,
    /// The last `ICoreWebView2Controller::IsVisible` set: whether WebView2
    /// renders the page at all.
    applied_rendering: Option<bool>,
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
        check_environment_overrides(options.profile, |name| std::env::var_os(name))?;

        let composited = host_uses_direct_composition(host_hwnd);
        let profile = options.profile;
        let restricted = options.uses_ai_tab_features();
        let inner = Rc::new(Inner {
            host_hwnd: HWND(host_hwnd as *mut c_void),
            composited,
            events,
            profile,
            document_start_script: options.document_start_script,
            navigation_filter: options.navigation_filter,
            block_downloads: options.block_downloads,
            in_flight: RefCell::new(Vec::new()),
            state: RefCell::new(State {
                scale: 1.0,
                hidden_reason: Some("the panel has not been painted yet"),
                ..Default::default()
            }),
        });

        log::info!(
            "WebView2: requesting a {profile:?} surface (host hwnd {host_hwnd:#x}, direct composition on the host window: {composited}, AI tab features: {restricted})",
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
        with_profile_environment(
            profile,
            EnvironmentConfiguration {
                user_data_folder: options.user_data_folder,
                proxy_endpoint: options.proxy_endpoint,
            },
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
            if let Some(size) = rect_size(bounds) {
                state.last_size = Some(size);
            }
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
    /// paint can show it again in place. An AI tab's page is parked instead:
    /// out of sight and out of reach of the mouse, but still rendering.
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
        match self.inner.ready_webview() {
            Some(webview) => {
                log::info!("WebView2: navigating to {url}");
                let wide = HSTRING::from(url);
                unsafe { webview.Navigate(PCWSTR(wide.as_ptr())) }.log_err();
            }
            None => {
                log::info!("WebView2: queueing a navigation to {url} until the page is ready");
                self.inner.state.borrow_mut().pending_url = Some(url.to_string());
            }
        }
    }

    pub(crate) fn go_back(&self) {
        if let Some(webview) = self.inner.ready_webview() {
            unsafe { webview.GoBack() }.log_err();
        }
    }

    pub(crate) fn go_forward(&self) {
        if let Some(webview) = self.inner.ready_webview() {
            unsafe { webview.GoForward() }.log_err();
        }
    }

    pub(crate) fn reload(&self) {
        if let Some(webview) = self.inner.ready_webview() {
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

impl WebView {
    /// Runs `script` in the page's main frame and hands back the JSON of its
    /// completion value. WebView2 reports a script that throws as `null`, so
    /// callers that need the exception have to catch it in the script.
    /// Waits for the page to be ready when asked earlier.
    pub(crate) fn execute_script(&self, script: &str) -> oneshot::Receiver<Result<String>> {
        let (reply, receiver) = Reply::new();
        self.inner.track(&reply);
        let script = HSTRING::from(script);
        self.inner.when_page_ready(Box::new(move |webview| {
            let webview = match webview {
                Ok(webview) => webview,
                Err(message) => return reply.send(Err(anyhow!(message))),
            };
            let handler =
                ExecuteScriptCompletedHandler::create(Box::new({
                    let reply = reply.clone();
                    move |result, json| {
                        reply.send(result.map(|()| json).map_err(|error| {
                            anyhow!("WebView2 could not run the script: {error}")
                        }));
                        Ok(())
                    }
                }));
            if let Err(error) = unsafe { webview.ExecuteScript(PCWSTR(script.as_ptr()), &handler) }
            {
                reply.send(Err(anyhow!("WebView2 could not run the script: {error}")));
            }
        }));
        receiver
    }

    /// A PNG of what the surface shows, at its size in physical pixels.
    pub(crate) fn capture_png(&self) -> oneshot::Receiver<Result<Vec<u8>>> {
        let (reply, receiver) = Reply::new();
        self.inner.track(&reply);
        self.inner.when_page_ready(Box::new(move |webview| {
            let webview = match webview {
                Ok(webview) => webview,
                Err(message) => return reply.send(Err(anyhow!(message))),
            };
            let stream = match unsafe { CreateStreamOnHGlobal(HGLOBAL::default(), true) } {
                Ok(stream) => stream,
                Err(error) => {
                    return reply.send(Err(anyhow!(
                        "could not create a buffer for the screenshot: {error}"
                    )));
                }
            };
            let handler = CapturePreviewCompletedHandler::create(Box::new({
                let reply = reply.clone();
                let stream = stream.clone();
                move |result| {
                    reply.send(
                        result
                            .map_err(|error| {
                                anyhow!("WebView2 could not take a screenshot: {error}")
                            })
                            .and_then(|()| read_stream(&stream)),
                    );
                    Ok(())
                }
            }));
            if let Err(error) = unsafe {
                webview.CapturePreview(
                    COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG,
                    &stream,
                    &handler,
                )
            } {
                reply.send(Err(anyhow!(
                    "WebView2 could not take a screenshot: {error}"
                )));
            }
        }));
        receiver
    }

    /// Trusted input through the DevTools protocol, which delivers it to the
    /// renderer as if it came from the user (`isTrusted`), whether or not the
    /// page holds the Win32 keyboard focus.
    pub(crate) fn dispatch_input(&self, input: NativeInput) -> oneshot::Receiver<Result<()>> {
        let (reply, receiver) = Reply::new();
        self.inner.track(&reply);
        let calls = devtools_calls(&input);
        self.inner
            .when_page_ready(Box::new(move |webview| match webview {
                Ok(webview) => call_devtools_methods(
                    webview,
                    calls.into(),
                    Box::new(move |result| reply.send(result)),
                ),
                Err(message) => reply.send(Err(anyhow!(message))),
            }));
        receiver
    }

    pub(crate) fn open_developer_tools(&self) {
        match self.inner.webview() {
            Some(webview) => {
                unsafe { webview.OpenDevToolsWindow() }.log_err();
            }
            None => log::info!("WebView2: no page to open the developer tools for yet"),
        }
    }

    pub(crate) fn is_detached(&self) -> bool {
        false
    }

    pub(crate) fn raise_detached_window(&self) {}
}

/// The sending half of a backend method's answer, shared between the
/// completion handler and the code that starts the call: when starting fails,
/// WebView2 never calls the handler, so the error has to go out from there.
struct Reply<T>(Rc<Cell<Option<oneshot::Sender<Result<T>>>>>);

impl<T> Clone for Reply<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

/// An answer that may still be owed; see [`Inner::in_flight`].
trait PendingAnswer {
    fn fail(&self, message: &str);
}

impl<T> PendingAnswer for Cell<Option<oneshot::Sender<Result<T>>>> {
    fn fail(&self, message: &str) {
        if let Some(sender) = self.take() {
            sender.send(Err(anyhow!("{message}"))).ok();
        }
    }
}

impl<T> Reply<T> {
    fn new() -> (Self, oneshot::Receiver<Result<T>>) {
        let (sender, receiver) = oneshot::channel();
        (Self(Rc::new(Cell::new(Some(sender)))), receiver)
    }

    /// Only the first answer counts. The caller may have stopped waiting,
    /// which leaves nobody to tell.
    fn send(&self, value: Result<T>) {
        if let Some(sender) = self.0.take() {
            sender.send(value).ok();
        }
    }
}

fn read_stream(stream: &IStream) -> Result<Vec<u8>> {
    unsafe { stream.Seek(0, STREAM_SEEK_SET, None) }
        .context("could not rewind the screenshot buffer")?;
    let mut bytes = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let mut read = 0u32;
        unsafe {
            stream.Read(
                chunk.as_mut_ptr().cast(),
                chunk.len() as u32,
                Some(&mut read),
            )
        }
        .ok()
        .context("could not read the screenshot buffer")?;
        if read == 0 {
            return Ok(bytes);
        }
        let Some(read_bytes) = chunk.get(..read as usize) else {
            bail!("the screenshot buffer reported more bytes than it was asked for");
        };
        bytes.extend_from_slice(read_bytes);
    }
}

/// Calls the DevTools protocol methods one after the other, each once the
/// previous one has completed, so that a press is never overtaken by its
/// release. `finish` gets the first failure, or success after the last call.
fn call_devtools_methods(
    webview: ICoreWebView2,
    mut calls: VecDeque<(&'static str, serde_json::Value)>,
    finish: Box<dyn FnOnce(Result<()>)>,
) {
    let Some((method, parameters)) = calls.pop_front() else {
        finish(Ok(()));
        return;
    };
    // Shared with the handler, so that a synchronous failure - after which the
    // handler is never called - can still finish here.
    let finish = Rc::new(Cell::new(Some(finish)));
    let handler = CallDevToolsProtocolMethodCompletedHandler::create(Box::new({
        let webview = webview.clone();
        let finish = finish.clone();
        move |result, response| {
            if let Some(finish) = finish.take() {
                match result {
                    Ok(()) => call_devtools_methods(webview, calls, finish),
                    Err(error) => finish(Err(anyhow!("{method} failed: {error} {response}"))),
                }
            }
            Ok(())
        }
    }));
    let method_name = HSTRING::from(method);
    let parameters = HSTRING::from(parameters.to_string());
    if let Err(error) = unsafe {
        webview.CallDevToolsProtocolMethod(
            PCWSTR(method_name.as_ptr()),
            PCWSTR(parameters.as_ptr()),
            &handler,
        )
    } && let Some(finish) = finish.take()
    {
        finish(Err(anyhow!("{method} failed: {error}")));
    }
}

const DISPATCH_MOUSE_EVENT: &str = "Input.dispatchMouseEvent";
const DISPATCH_KEY_EVENT: &str = "Input.dispatchKeyEvent";

/// The DevTools protocol calls that deliver `input`. Coordinates stay in CSS
/// pixels, which is what the protocol takes.
fn devtools_calls(input: &NativeInput) -> Vec<(&'static str, serde_json::Value)> {
    match input {
        NativeInput::MouseMove { x, y } => vec![(
            DISPATCH_MOUSE_EVENT,
            serde_json::json!({ "type": "mouseMoved", "x": x, "y": y }),
        )],
        NativeInput::MouseClick {
            x,
            y,
            button,
            click_count,
            modifiers,
        } => {
            let modifiers = modifier_mask(modifiers);
            let (button, buttons) = match button {
                MouseButton::Left => ("left", 1),
                MouseButton::Right => ("right", 2),
                MouseButton::Middle => ("middle", 4),
            };
            let mut calls = vec![(
                DISPATCH_MOUSE_EVENT,
                serde_json::json!({
                    "type": "mouseMoved", "x": x, "y": y, "modifiers": modifiers,
                }),
            )];
            // A double click is two press/release pairs whose second one
            // carries `clickCount: 2`, the way a real mouse reports it.
            for click_count in 1..=(*click_count).max(1) {
                for (event_type, buttons) in [("mousePressed", buttons), ("mouseReleased", 0)] {
                    calls.push((
                        DISPATCH_MOUSE_EVENT,
                        serde_json::json!({
                            "type": event_type,
                            "x": x,
                            "y": y,
                            "button": button,
                            "buttons": buttons,
                            "clickCount": click_count,
                            "modifiers": modifiers,
                        }),
                    ));
                }
            }
            calls
        }
        NativeInput::Wheel {
            x,
            y,
            delta_x,
            delta_y,
        } => vec![(
            DISPATCH_MOUSE_EVENT,
            serde_json::json!({
                "type": "mouseWheel", "x": x, "y": y, "deltaX": delta_x, "deltaY": delta_y,
            }),
        )],
        NativeInput::Key {
            key,
            code,
            modifiers,
        } => {
            let text = key_text(key, modifiers);
            let key_event = |event_type: &str, text: Option<&str>| {
                let mut event = serde_json::Map::new();
                event.insert("type".into(), event_type.into());
                event.insert("key".into(), key.as_str().into());
                event.insert("code".into(), code.as_str().into());
                event.insert("modifiers".into(), modifier_mask(modifiers).into());
                if let Some(virtual_key) = windows_virtual_key_code(key, code) {
                    event.insert("windowsVirtualKeyCode".into(), virtual_key.into());
                    event.insert("nativeVirtualKeyCode".into(), virtual_key.into());
                }
                if let Some(text) = text {
                    event.insert("text".into(), text.into());
                    event.insert("unmodifiedText".into(), text.into());
                }
                serde_json::Value::Object(event)
            };
            // `keyDown` with text also types the character; `rawKeyDown` is
            // the press alone, for keys that type nothing or carry a shortcut
            // modifier.
            let key_down = match text {
                Some(text) => key_event("keyDown", Some(text)),
                None => key_event("rawKeyDown", None),
            };
            vec![
                (DISPATCH_KEY_EVENT, key_down),
                (DISPATCH_KEY_EVENT, key_event("keyUp", None)),
            ]
        }
        NativeInput::InsertText(text) => {
            vec![("Input.insertText", serde_json::json!({ "text": text }))]
        }
    }
}

/// The DevTools protocol's modifier bit field.
fn modifier_mask(modifiers: &[KeyModifier]) -> u32 {
    modifiers.iter().fold(0, |mask, modifier| {
        mask | match modifier {
            KeyModifier::Alt => 1,
            KeyModifier::Control => 2,
            KeyModifier::Meta => 4,
            KeyModifier::Shift => 8,
        }
    })
}

/// What a key press types, if anything. `key` already reflects Shift; with
/// Control, Alt or Meta held the press is a shortcut and types nothing.
fn key_text<'a>(key: &'a str, modifiers: &[KeyModifier]) -> Option<&'a str> {
    if modifiers
        .iter()
        .any(|modifier| !matches!(modifier, KeyModifier::Shift))
    {
        return None;
    }
    match key {
        "Enter" => Some("\r"),
        _ if key.chars().count() == 1 => Some(key),
        _ => None,
    }
}

/// The Windows virtual-key code Chromium expects with a key event, from the
/// physical `code` when it is known and otherwise from the `key` value on a US
/// layout. Pages and Chromium's own editing commands read `keyCode`, which
/// comes from this.
fn windows_virtual_key_code(key: &str, code: &str) -> Option<u32> {
    code_virtual_key(code).or_else(|| key_virtual_key(key))
}

fn code_virtual_key(code: &str) -> Option<u32> {
    let single_suffix = |prefix: &str| {
        let rest = code.strip_prefix(prefix)?;
        let mut characters = rest.chars();
        let character = characters.next()?;
        characters.next().is_none().then_some(character)
    };
    if let Some(letter) = single_suffix("Key").filter(char::is_ascii_uppercase) {
        return Some(letter as u32);
    }
    if let Some(digit) = single_suffix("Digit").filter(char::is_ascii_digit) {
        return Some(digit as u32);
    }
    if let Some(digit) = single_suffix("Numpad").and_then(|digit| digit.to_digit(10)) {
        return Some(0x60 + digit);
    }
    if let Some(number) = code
        .strip_prefix('F')
        .and_then(|number| number.parse::<u32>().ok())
        .filter(|number| (1..=24).contains(number))
    {
        return Some(0x70 + number - 1);
    }
    Some(match code {
        "Backspace" => 0x08,
        "Tab" => 0x09,
        "Enter" | "NumpadEnter" => 0x0D,
        "ShiftLeft" | "ShiftRight" => 0x10,
        "ControlLeft" | "ControlRight" => 0x11,
        "AltLeft" | "AltRight" => 0x12,
        "Pause" => 0x13,
        "CapsLock" => 0x14,
        "Escape" => 0x1B,
        "Space" => 0x20,
        "PageUp" => 0x21,
        "PageDown" => 0x22,
        "End" => 0x23,
        "Home" => 0x24,
        "ArrowLeft" => 0x25,
        "ArrowUp" => 0x26,
        "ArrowRight" => 0x27,
        "ArrowDown" => 0x28,
        "PrintScreen" => 0x2C,
        "Insert" => 0x2D,
        "Delete" => 0x2E,
        "MetaLeft" => 0x5B,
        "MetaRight" => 0x5C,
        "ContextMenu" => 0x5D,
        "NumpadMultiply" => 0x6A,
        "NumpadAdd" => 0x6B,
        "NumpadSubtract" => 0x6D,
        "NumpadDecimal" => 0x6E,
        "NumpadDivide" => 0x6F,
        "NumLock" => 0x90,
        "ScrollLock" => 0x91,
        "Semicolon" => 0xBA,
        "Equal" => 0xBB,
        "Comma" => 0xBC,
        "Minus" => 0xBD,
        "Period" => 0xBE,
        "Slash" => 0xBF,
        "Backquote" => 0xC0,
        "BracketLeft" => 0xDB,
        "Backslash" => 0xDC,
        "BracketRight" => 0xDD,
        "Quote" => 0xDE,
        _ => return None,
    })
}

fn key_virtual_key(key: &str) -> Option<u32> {
    let mut characters = key.chars();
    if let (Some(character), None) = (characters.next(), characters.next()) {
        if character.is_ascii_alphanumeric() {
            return Some(character.to_ascii_uppercase() as u32);
        }
        return Some(match character {
            ' ' => 0x20,
            ')' => 0x30,
            '!' => 0x31,
            '@' => 0x32,
            '#' => 0x33,
            '$' => 0x34,
            '%' => 0x35,
            '^' => 0x36,
            '&' => 0x37,
            '*' => 0x38,
            '(' => 0x39,
            ';' | ':' => 0xBA,
            '=' | '+' => 0xBB,
            ',' | '<' => 0xBC,
            '-' | '_' => 0xBD,
            '.' | '>' => 0xBE,
            '/' | '?' => 0xBF,
            '`' | '~' => 0xC0,
            '[' | '{' => 0xDB,
            '\\' | '|' => 0xDC,
            ']' | '}' => 0xDD,
            '\'' | '"' => 0xDE,
            _ => return None,
        });
    }
    match key {
        "Shift" => Some(0x10),
        "Control" => Some(0x11),
        "Alt" => Some(0x12),
        "Meta" => Some(0x5B),
        // The remaining named keys share their name with their `code`.
        _ => code_virtual_key(key),
    }
}

impl Drop for WebView {
    fn drop(&mut self) {
        let (controller, children, operations) = {
            let mut state = self.inner.state.borrow_mut();
            state.closed = true;
            (
                state.controller.take(),
                state.owned_children.clone(),
                std::mem::take(&mut state.pending_operations),
            )
        };
        for operation in operations {
            operation(Err("the browser tab was closed".to_string()));
        }
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

    /// The webview, once it may be navigated and scripted.
    fn ready_webview(&self) -> Option<ICoreWebView2> {
        let state = self.state.borrow();
        if state.page_ready {
            state.webview.clone()
        } else {
            None
        }
    }

    /// Runs `operation` now when the page is ready, or once it is.
    fn when_page_ready(&self, operation: PageOperation) {
        let outcome = {
            let mut state = self.state.borrow_mut();
            if let Some(failure) = &state.failure {
                Some(Err(failure.clone()))
            } else if let (true, Some(webview)) = (state.page_ready, &state.webview) {
                Some(Ok(webview.clone()))
            } else {
                state.pending_operations.push(operation);
                return;
            }
        };
        // Outside the borrow: the operation talks to WebView2, which can
        // re-enter our handlers.
        if let Some(outcome) = outcome {
            operation(outcome);
        }
    }

    /// Lets the page be used: replays what waited for it.
    fn open_page(&self) {
        let (webview, pending_url, operations) = {
            let mut state = self.state.borrow_mut();
            if state.closed || state.failure.is_some() {
                return;
            }
            state.page_ready = true;
            (
                state.webview.clone(),
                state.pending_url.take(),
                std::mem::take(&mut state.pending_operations),
            )
        };
        let Some(webview) = webview else {
            return;
        };
        for operation in operations {
            operation(Ok(webview.clone()));
        }
        if let Some(url) = pending_url {
            log::info!("WebView2: navigating to the queued {url}");
            let url = HSTRING::from(url.as_str());
            unsafe { webview.Navigate(PCWSTR(url.as_ptr())) }.log_err();
        }
    }

    /// Gives up on a controller that attached but cannot be used safely (the
    /// navigation filter or the download block could not be installed, file
    /// dialogs could not be suppressed, or the document start script could not
    /// be registered): a page without them would be an AI tab without its
    /// restrictions.
    fn abandon_controller(&self, message: String) {
        let controller = {
            let mut state = self.state.borrow_mut();
            state.webview = None;
            state.page_ready = false;
            state.controller.take()
        };
        if let Some(controller) = controller {
            unsafe { controller.Close() }.log_err();
        }
        self.report_failure_message(&message);
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
        {
            let mut state = self.state.borrow_mut();
            state.children_before_controller = children_before_controller;
            state.environment = Some(environment.clone());
        }
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

        // First, before anything can navigate the page.
        if let Err(error) = self.register_policy_handlers(&webview) {
            unsafe { controller.Close() }.log_err();
            self.report_failure_message(&format!(
                "could not install the page's navigation and download restrictions: {error}"
            ));
            return;
        }

        // WebView2 would otherwise rescale itself from the monitor DPI, which
        // disagrees with GPUI's own scale factor while a window straddles two
        // monitors.
        if let Ok(controller) = controller.cast::<ICoreWebView2Controller3>() {
            unsafe { controller.SetShouldDetectMonitorScaleChanges(false) }.log_err();
        }

        // On by default, but a runtime policy can turn it off, and the
        // "Open Developer Tools" command has to work in every tab.
        if let Some(settings) = unsafe { webview.Settings() }.log_err() {
            unsafe { settings.SetAreDevToolsEnabled(true) }.log_err();
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

        let (pending_dark_mode, pending_background_color) = {
            let mut state = self.state.borrow_mut();
            state.controller = Some(controller);
            state.webview = Some(webview.clone());
            state.owned_children = owned_children;
            state.children_before_controller = Vec::new();
            // Force the first apply: the surface was never configured.
            state.applied_bounds = None;
            state.applied_scale = None;
            state.applied_visible = None;
            state.applied_rendering = None;
            state.applied_input_blocked = None;
            (
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
        if self.profile == WebViewProfile::Ai {
            self.suppress_file_choosers(&webview);
        } else {
            self.prepare_page(&webview);
        }
        // The surface has already reported itself failed.
        if self.state.borrow().failure.is_some() {
            return;
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

    /// A click the AI makes is trusted, so an upload button would open the
    /// native file dialog: modal to the editor window, and a way for the AI to
    /// pick a local file. Intercepted file choosers are not shown at all, and
    /// `cancel` tells the page the choice was cancelled. The page only opens
    /// once this holds.
    fn suppress_file_choosers(self: &Rc<Self>, webview: &ICoreWebView2) {
        call_devtools_methods(
            webview.clone(),
            VecDeque::from([
                // Chromium only intercepts for a session with the domain on.
                ("Page.enable", serde_json::json!({})),
                (
                    "Page.setInterceptFileChooserDialog",
                    serde_json::json!({ "enabled": true, "cancel": true }),
                ),
            ]),
            Box::new({
                let surface = Rc::downgrade(self);
                let webview = webview.clone();
                move |result| {
                    if let Some(surface) = surface.upgrade() {
                        match result {
                            Ok(()) => surface.prepare_page(&webview),
                            Err(error) => surface.abandon_controller(format!(
                                "could not keep file dialogs from opening: {error:#}"
                            )),
                        }
                    }
                }
            }),
        );
    }

    /// Registers the document start script, if any, and opens the page once
    /// WebView2 has confirmed it: a script added later than the first
    /// navigation would miss that document.
    fn prepare_page(self: &Rc<Self>, webview: &ICoreWebView2) {
        let Some(script) = &self.document_start_script else {
            self.open_page();
            return;
        };
        let handler = AddScriptToExecuteOnDocumentCreatedCompletedHandler::create(Box::new({
            let surface = Rc::downgrade(self);
            move |result, _script_id| {
                if let Some(surface) = surface.upgrade() {
                    match result {
                        Ok(()) => surface.open_page(),
                        Err(error) => surface.abandon_controller(format!(
                            "could not install the page script: {error}"
                        )),
                    }
                }
                Ok(())
            }
        }));
        let script = HSTRING::from(script.as_str());
        if let Err(error) = unsafe {
            webview.AddScriptToExecuteOnDocumentCreated(PCWSTR(script.as_ptr()), &handler)
        } {
            self.abandon_controller(format!("could not install the page script: {error}"));
        }
    }

    /// The handlers that restrict what the page may do. Unlike the others,
    /// failing to install one fails the surface: a page without them would
    /// navigate or download unchecked.
    fn register_policy_handlers(&self, webview: &ICoreWebView2) -> windows::core::Result<()> {
        let mut token = 0i64;

        if let Some(filter) = self.navigation_filter.clone() {
            let events = self.events.clone();
            // Raised for every top-level navigation, including redirects,
            // history navigations and reloads; frames are left to the proxy.
            let handler = NavigationStartingEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()) };
                let mut uri = windows::core::PWSTR::null();
                let decision = match unsafe { args.Uri(&mut uri) } {
                    Ok(()) => {
                        let uri = take_pwstr(uri);
                        let decision = filter(&uri);
                        (uri, decision)
                    }
                    Err(error) => (
                        String::new(),
                        NavigationDecision::Deny(format!("its address could not be read: {error}")),
                    ),
                };
                if let (url, NavigationDecision::Deny(reason)) = decision {
                    log::info!("WebView2: blocked a navigation to {url}: {reason}");
                    unsafe { args.SetCancel(true) }?;
                    send_on(&events, WebViewEvent::NavigationBlocked { url, reason });
                }
                Ok(())
            }));
            unsafe { webview.add_NavigationStarting(&handler, &mut token) }?;
        }

        if self.block_downloads {
            let events = self.events.clone();
            let handler = DownloadStartingEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()) };
                unsafe { args.SetCancel(true) }?;
                // Keeps WebView2's download flyout from appearing for it.
                unsafe { args.SetHandled(true) }.log_err();
                let url = unsafe { args.DownloadOperation() }
                    .log_err()
                    .and_then(|operation| {
                        let mut uri = windows::core::PWSTR::null();
                        unsafe { operation.Uri(&mut uri) }
                            .log_err()
                            .map(|()| take_pwstr(uri))
                    })
                    .unwrap_or_default();
                log::info!("WebView2: blocked a download of {url}");
                send_on(&events, WebViewEvent::DownloadBlocked { url });
                Ok(())
            }));
            let webview = webview.cast::<ICoreWebView2_4>()?;
            unsafe { webview.add_DownloadStarting(&handler, &mut token) }?;
        }

        // AI tabs never get the camera, the microphone, the location, the
        // clipboard, notifications or any other permission: nobody is there to
        // answer WebView2's prompt for the AI, and the AI must not get them.
        if self.profile == WebViewProfile::Ai {
            let handler = PermissionRequestedEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()) };
                let mut kind = COREWEBVIEW2_PERMISSION_KIND::default();
                unsafe { args.PermissionKind(&mut kind) }.log_err();
                log::info!(
                    "WebView2: denied a permission request (kind {}) in an AI tab",
                    kind.0
                );
                unsafe { args.SetState(COREWEBVIEW2_PERMISSION_STATE_DENY) }
            }));
            unsafe { webview.add_PermissionRequested(&handler, &mut token) }?;

            // Nobody is there to answer a dialog for the AI, and WebView2's own would pop up
            // over the editor from a page the user may not even see (a parked tab). The page
            // script answers alert/confirm/prompt itself; this answers what gets past it (a
            // frame it did not run in, `beforeunload`) without showing anything.
            let settings = unsafe { webview.Settings() }?;
            unsafe { settings.SetAreDefaultScriptDialogsEnabled(false) }?;
            let handler = ScriptDialogOpeningEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()) };
                let mut kind = COREWEBVIEW2_SCRIPT_DIALOG_KIND::default();
                unsafe { args.Kind(&mut kind) }?;
                let accept = accepts_script_dialog(kind);
                log::info!(
                    "WebView2: answered a script dialog (kind {}) in an AI tab: {}",
                    kind.0,
                    if accept { "accepted" } else { "dismissed" }
                );
                if accept {
                    unsafe { args.Accept() }?;
                }
                Ok(())
            }));
            unsafe { webview.add_ScriptDialogOpening(&handler, &mut token) }?;

            // The sign-in prompt for HTTP authentication is a dialog of the same kind. The AI
            // gets the server's 401 page instead.
            let handler =
                BasicAuthenticationRequestedEventHandler::create(Box::new(move |_, args| {
                    let Some(args) = args else { return Ok(()) };
                    log::info!("WebView2: cancelled an HTTP sign-in prompt in an AI tab");
                    unsafe { args.SetCancel(true) }
                }));
            let webview = webview.cast::<ICoreWebView2_10>()?;
            unsafe { webview.add_BasicAuthenticationRequested(&handler, &mut token) }?;
        }

        {
            let events = self.events.clone();
            let filter = self.navigation_filter.clone();
            let handler = NewWindowRequestedEventHandler::create(Box::new(move |_, args| {
                if let Some(args) = args {
                    let mut uri = windows::core::PWSTR::null();
                    if unsafe { args.Uri(&mut uri) }.log_err().is_some()
                        && let Some(event) = new_window_event(filter.as_deref(), take_pwstr(uri))
                    {
                        if let WebViewEvent::NavigationBlocked { url, reason } = &event {
                            log::info!("WebView2: blocked a new window for {url}: {reason}");
                        }
                        send_on(&events, event);
                    }
                    unsafe { args.SetHandled(true) }.log_err();
                }
                Ok(())
            }));
            let registered = unsafe { webview.add_NewWindowRequested(&handler, &mut token) };
            // Unhandled, WebView2 opens the popup as a window of its own,
            // which has none of the handlers above.
            if self.restricts_page() {
                registered?;
            } else {
                registered.log_err();
            }
        }

        Ok(())
    }

    fn restricts_page(&self) -> bool {
        self.navigation_filter.is_some()
            || self.block_downloads
            || self.document_start_script.is_some()
    }

    fn register_event_handlers(
        self: &Rc<Self>,
        controller: &ICoreWebView2Controller,
        webview: &ICoreWebView2,
    ) {
        let mut token = 0i64;

        // Weak: the controller holds the handler, and the surface holds the
        // controller.
        {
            let surface = Rc::downgrade(self);
            let handler = ProcessFailedEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()) };
                let failure = read_process_failure(&args);
                if let Some(surface) = surface.upgrade() {
                    surface.process_failed(failure);
                }
                Ok(())
            }));
            unsafe { webview.add_ProcessFailed(&handler, &mut token) }.log_err();
        }

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
        let (controller, scale, bounds, input_blocked, visible, rendering, hidden_reason) = {
            let state = self.state.borrow();
            let Some(controller) = state.controller.clone() else {
                return false;
            };
            let placement = placement(
                self.parks_out_of_sight(),
                state.visible,
                state.bounds,
                state.last_size,
                state.scale,
            );
            let showing = state.visible && state.applied_visible != Some(true);
            (
                controller,
                (state.applied_scale != Some(state.scale)).then_some(state.scale),
                (state.applied_bounds != Some(placement.bounds)).then_some(placement.bounds),
                // Also on every show, so that a window whose enabled state was
                // changed behind this surface's back gets its own state back.
                (showing || state.applied_input_blocked != Some(state.input_blocked))
                    .then_some(state.input_blocked),
                (state.applied_visible != Some(state.visible)).then_some(state.visible),
                (state.applied_rendering != Some(placement.rendering))
                    .then_some(placement.rendering),
                state.hidden_reason,
            )
        };

        // A parked page keeps its windows shown, so unlike a hidden one it
        // would keep the keyboard, and typing would go to a page nobody sees.
        if visible == Some(false) && self.parks_out_of_sight() {
            let owned_children = self.state.borrow().owned_children.clone();
            let children = direct_children(self.host_hwnd, &owned_children).collect::<Vec<_>>();
            if self.page_owns_keyboard_focus(&children) {
                self.return_keyboard_focus();
            }
        }

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

        if let Some(rendering) = rendering
            && unsafe { controller.SetIsVisible(rendering) }
                .log_err()
                .is_some()
        {
            // The borrow has to be gone before the Win32 calls below, and
            // holding it across the `if` would deadlock this cell.
            let first_show = {
                let mut state = self.state.borrow_mut();
                state.applied_rendering = Some(rendering);
                let first_show = rendering && !state.nudged_child_windows;
                state.nudged_child_windows |= rendering;
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

        if let Some(visible) = visible {
            match (hidden_reason, self.parks_out_of_sight()) {
                (None, _) => log::info!("WebView2: surface shown"),
                (Some(reason), true) => {
                    log::info!("WebView2: surface parked out of sight because {reason}")
                }
                (Some(reason), false) => log::info!("WebView2: surface hidden because {reason}"),
            }
            self.state.borrow_mut().applied_visible = Some(visible);
            if self.parks_out_of_sight() {
                self.allow_context_menus(visible);
            }
        }

        // After the visibility, so that a page shown in this same pass can take
        // it.
        if input_blocked == Some(false) {
            self.move_pending_focus();
        }

        self.apply_clip(defer_cuts)
    }

    /// Whether the page keeps rendering out of sight instead of being hidden:
    /// an AI tab has to load, run scripts, take input and be captured while
    /// nobody looks at it (ADR-F89).
    fn parks_out_of_sight(&self) -> bool {
        self.profile == WebViewProfile::Ai
    }

    /// Unlike a `<select>`'s list, a context menu is moved onto the nearest
    /// monitor, so a right click on a parked page would show one to the user,
    /// outside the window.
    fn allow_context_menus(&self, allowed: bool) {
        let Some(webview) = self.webview() else {
            return;
        };
        if let Some(settings) = unsafe { webview.Settings() }.log_err() {
            unsafe { settings.SetAreDefaultContextMenusEnabled(allowed) }.log_err();
        }
    }

    fn track<T: 'static>(&self, reply: &Reply<T>) {
        let mut in_flight = self.in_flight.borrow_mut();
        in_flight.retain(|answer| answer.strong_count() > 0);
        let answer: Weak<dyn PendingAnswer> = Rc::downgrade(&reply.0) as Weak<_>;
        in_flight.push(answer);
    }

    /// A process the page needs went away. When that was the page's own
    /// renderer or the browser process, the surface is dead for good: every
    /// answer still owed and every later page operation fails with the reason,
    /// and the view is told to replace the surface.
    fn process_failed(&self, failure: ProcessFailure) {
        if !failure.loses_the_page() {
            log::warn!("WebView2: {}; the page carries on", failure.describe());
            return;
        }
        let detail = failure.describe();
        log::error!("WebView2: {detail}; the surface is replaced");
        let message = crashed_page_message(&detail);
        let (operations, environment) = {
            let mut state = self.state.borrow_mut();
            if state.failure.is_some() {
                return;
            }
            state.failure = Some(message.clone());
            state.page_ready = false;
            (
                std::mem::take(&mut state.pending_operations),
                state.environment.clone(),
            )
        };
        if failure.kind == COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED
            && let Some(environment) = environment
        {
            forget_environment(self.profile, &environment);
        }
        let in_flight = std::mem::take(&mut *self.in_flight.borrow_mut());
        for answer in in_flight.iter().filter_map(Weak::upgrade) {
            answer.fail(&message);
        }
        for operation in operations {
            operation(Err(message.clone()));
        }
        self.send(WebViewEvent::Crashed(detail));
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
        self.fail(format!("{error}"));
    }

    fn report_failure_message(&self, message: &str) {
        log::error!("WebView2 creation failed: {message}");
        self.fail(message.to_string());
    }

    fn fail(&self, message: String) {
        let operations = {
            let mut state = self.state.borrow_mut();
            state.failure = Some(message.clone());
            std::mem::take(&mut state.pending_operations)
        };
        for operation in operations {
            operation(Err(message.clone()));
        }
        self.send(WebViewEvent::Failed(message));
    }

    fn send(&self, event: WebViewEvent) {
        send_on(&self.events, event);
    }
}

/// How far outside the host's client area a parked page is put, in physical
/// pixels. The child window is clipped to the host whatever the distance, but
/// the lists a `<select>`, a colour or a date input open are windows of their
/// own, placed at the page's screen position: a page parked just outside the
/// host would open them on a monitor next to or above the window, where the
/// user sees and clicks them. This far out, no monitor arrangement reaches
/// them, and the coordinates still fit the 16 bits of `WM_MOVE`.
const PARKING_MARGIN: i32 = 16_000;

/// Where the controller goes and whether WebView2 renders it.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Placement {
    bounds: RECT,
    rendering: bool,
}

/// Out of sight, a user tab's page is hidden (`IsVisible(false)`: WebView2
/// stops rendering it) and keeps its rectangle for when it comes back. An AI
/// tab's page is parked instead: it stays visible to WebView2, at the size it
/// last had on screen (or the default size of a page that was never shown),
/// but moved above and to the left of the host's client area, where a child
/// window shows nothing and takes no clicks. Chromium then keeps loading,
/// running timers and `requestAnimationFrame`, and painting frames that a
/// capture can read. Kept pure so that the geometry can be tested.
fn placement(
    parks: bool,
    visible: bool,
    bounds: RECT,
    last_size: Option<(i32, i32)>,
    scale: f64,
) -> Placement {
    if visible {
        return Placement {
            bounds,
            rendering: true,
        };
    }
    if !parks {
        return Placement {
            bounds,
            rendering: false,
        };
    }
    let (width, height) = last_size.unwrap_or_else(|| {
        let to_device = |value: f32| ((f64::from(value) * scale).round() as i32).max(1);
        (
            to_device(BACKGROUND_VIEWPORT_WIDTH),
            to_device(BACKGROUND_VIEWPORT_HEIGHT),
        )
    });
    Placement {
        bounds: RECT {
            left: -(width + PARKING_MARGIN),
            top: -(height + PARKING_MARGIN),
            right: -PARKING_MARGIN,
            bottom: -PARKING_MARGIN,
        },
        rendering: true,
    }
}

/// The size of a rectangle that has one.
fn rect_size(rect: RECT) -> Option<(i32, i32)> {
    let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
    (width > 0 && height > 0).then_some((width, height))
}

/// What a `ProcessFailed` event said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProcessFailure {
    kind: COREWEBVIEW2_PROCESS_FAILED_KIND,
    reason: Option<COREWEBVIEW2_PROCESS_FAILED_REASON>,
    exit_code: Option<i32>,
}

impl ProcessFailure {
    /// The browser process takes every page of its environment with it, and
    /// the renderer of the page's main frame takes the page (WebView2 shows
    /// an error page in its place). A frame's renderer, the GPU process or a
    /// utility process going away is something WebView2 recovers from on its
    /// own, and a renderer that is merely unresponsive may still come back.
    fn loses_the_page(&self) -> bool {
        self.kind == COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED
            || self.kind == COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED
    }

    fn describe(&self) -> String {
        let process = match self.kind {
            COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED => {
                "the browser process exited".to_string()
            }
            COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED => {
                "the page's renderer process exited".to_string()
            }
            other => format!("a browser helper process failed (kind {})", other.0),
        };
        let reason = self.reason.map(|reason| match reason {
            COREWEBVIEW2_PROCESS_FAILED_REASON_CRASHED => "crashed".to_string(),
            COREWEBVIEW2_PROCESS_FAILED_REASON_UNEXPECTED => "ended unexpectedly".to_string(),
            COREWEBVIEW2_PROCESS_FAILED_REASON_TERMINATED => "terminated".to_string(),
            COREWEBVIEW2_PROCESS_FAILED_REASON_OUT_OF_MEMORY => "out of memory".to_string(),
            COREWEBVIEW2_PROCESS_FAILED_REASON_LAUNCH_FAILED => "failed to start".to_string(),
            COREWEBVIEW2_PROCESS_FAILED_REASON_UNRESPONSIVE => "not responding".to_string(),
            COREWEBVIEW2_PROCESS_FAILED_REASON_PROFILE_DELETED => {
                "its profile was deleted".to_string()
            }
            other => format!("reason {}", other.0),
        });
        match (reason, self.exit_code) {
            (Some(reason), Some(exit_code)) => {
                format!("{process}: {reason}, exit code {exit_code:#x}")
            }
            (Some(reason), None) => format!("{process}: {reason}"),
            (None, Some(exit_code)) => format!("{process}: exit code {exit_code:#x}"),
            (None, None) => process,
        }
    }
}

fn read_process_failure(args: &ICoreWebView2ProcessFailedEventArgs) -> ProcessFailure {
    let mut kind = COREWEBVIEW2_PROCESS_FAILED_KIND::default();
    unsafe { args.ProcessFailedKind(&mut kind) }.log_err();
    // Runtimes from 2021 on; older ones only say which process it was.
    let (reason, exit_code) = match args.cast::<ICoreWebView2ProcessFailedEventArgs2>() {
        Ok(args) => {
            let mut reason = COREWEBVIEW2_PROCESS_FAILED_REASON::default();
            let mut exit_code = 0i32;
            (
                unsafe { args.Reason(&mut reason) }
                    .log_err()
                    .map(|()| reason),
                unsafe { args.ExitCode(&mut exit_code) }
                    .log_err()
                    .map(|()| exit_code),
            )
        }
        Err(_) => (None, None),
    };
    ProcessFailure {
        kind,
        reason,
        exit_code,
    }
}

/// Lets the next surface of `profile` start from a new environment: the
/// browser process behind `environment` is gone, and so is every page it held.
/// WebView2 154 also starts a new browser process for a controller created in
/// the old environment; a fresh one does not depend on that. Leaves alone an
/// environment that already replaced it.
fn forget_environment(profile: WebViewProfile, environment: &ICoreWebView2Environment) {
    ENVIRONMENTS.with(|environments| {
        let mut environments = environments.borrow_mut();
        let slot = &mut environments.profile_mut(profile).slot;
        if matches!(slot, EnvironmentSlot::Ready(current) if current == environment) {
            log::info!("WebView2: the {profile:?} environment's browser process is gone");
            *slot = EnvironmentSlot::Idle;
        }
    });
}

/// The receiver is dropped together with the panel, and WebView2 keeps firing
/// callbacks until the controller is closed, so a closed channel is expected.
fn send_on(events: &UnboundedSender<WebViewEvent>, event: WebViewEvent) {
    events.unbounded_send(event).ok();
}

/// How an AI tab answers a script dialog nobody sees. Leaving a page is what the AI asked for
/// (`beforeunload` would otherwise keep the tab on the page), and every other dialog gets the
/// answer the page script gives: Cancel.
fn accepts_script_dialog(kind: COREWEBVIEW2_SCRIPT_DIALOG_KIND) -> bool {
    kind == COREWEBVIEW2_SCRIPT_DIALOG_KIND_BEFOREUNLOAD
}

/// What a page's request for a new window becomes: the view hosts a single
/// page, so an allowed one is opened in place. The retargeted navigation meets
/// the filter again in `NavigationStarting`; asking here as well reports a
/// denied popup without first handing it to the view.
fn new_window_event(
    navigation_filter: Option<&dyn Fn(&str) -> NavigationDecision>,
    url: String,
) -> Option<WebViewEvent> {
    match navigation_filter.map_or(NavigationDecision::Allow, |filter| filter(&url)) {
        NavigationDecision::Deny(reason) => Some(WebViewEvent::NavigationBlocked { url, reason }),
        NavigationDecision::Allow if url.is_empty() => None,
        NavigationDecision::Allow => Some(WebViewEvent::OpenInPlace(url)),
    }
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
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_SCRIPT_DIALOG_KIND_ALERT, COREWEBVIEW2_SCRIPT_DIALOG_KIND_CONFIRM,
        COREWEBVIEW2_SCRIPT_DIALOG_KIND_PROMPT,
    };

    #[test]
    fn ai_tabs_leave_pages_but_cancel_every_other_dialog() {
        assert!(accepts_script_dialog(
            COREWEBVIEW2_SCRIPT_DIALOG_KIND_BEFOREUNLOAD
        ));
        for kind in [
            COREWEBVIEW2_SCRIPT_DIALOG_KIND_ALERT,
            COREWEBVIEW2_SCRIPT_DIALOG_KIND_CONFIRM,
            COREWEBVIEW2_SCRIPT_DIALOG_KIND_PROMPT,
        ] {
            assert!(!accepts_script_dialog(kind), "{kind:?}");
        }
    }

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

    #[test]
    fn the_user_profile_only_gets_a_proxy_when_asked() {
        assert!(
            browser_arguments(WebViewProfile::User, None)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            browser_arguments(WebViewProfile::User, Some("http://127.0.0.1:4100")).unwrap(),
            ["--proxy-server=http://127.0.0.1:4100"]
        );
    }

    #[test]
    fn out_of_sight_an_ai_tab_is_parked_and_a_user_tab_is_hidden() {
        let bounds = RECT {
            left: 200,
            top: 100,
            right: 1000,
            bottom: 700,
        };
        let shown = Placement {
            bounds,
            rendering: true,
        };
        assert_eq!(placement(false, true, bounds, None, 2.), shown);
        assert_eq!(placement(true, true, bounds, None, 2.), shown);
        assert_eq!(
            placement(false, false, bounds, Some((800, 600)), 2.),
            Placement {
                bounds,
                rendering: false,
            },
            "a user tab stops rendering where it was"
        );

        let parked = placement(true, false, bounds, Some((800, 600)), 2.);
        assert!(parked.rendering, "a parked page keeps rendering");
        assert_eq!(rect_size(parked.bounds), Some((800, 600)));
        assert!(
            parked.bounds.right <= -PARKING_MARGIN && parked.bounds.bottom <= -PARKING_MARGIN,
            "the parked page is outside the client area: {:?}",
            parked.bounds
        );

        // Never on screen: the default size, in physical pixels.
        let never_shown = placement(true, false, RECT::default(), None, 1.5);
        assert!(never_shown.rendering);
        assert_eq!(rect_size(never_shown.bounds), Some((1920, 1200)));
        assert!(never_shown.bounds.right < 0 && never_shown.bounds.bottom < 0);
    }

    #[test]
    fn only_rectangles_with_an_area_have_a_size() {
        let rect = |left, top, right, bottom| RECT {
            left,
            top,
            right,
            bottom,
        };
        assert_eq!(rect_size(rect(10, 20, 110, 70)), Some((100, 50)));
        assert_eq!(rect_size(rect(10, 20, 10, 70)), None);
        assert_eq!(rect_size(rect(10, 20, 110, 20)), None);
        assert_eq!(rect_size(rect(10, 20, 0, 70)), None);
    }

    #[test]
    fn only_the_browser_or_the_page_s_renderer_going_away_loses_the_page() {
        use webview2_com::Microsoft::Web::WebView2::Win32::{
            COREWEBVIEW2_PROCESS_FAILED_KIND_FRAME_RENDER_PROCESS_EXITED,
            COREWEBVIEW2_PROCESS_FAILED_KIND_GPU_PROCESS_EXITED,
            COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE,
        };
        let failure = |kind| ProcessFailure {
            kind,
            reason: Some(COREWEBVIEW2_PROCESS_FAILED_REASON_CRASHED),
            exit_code: Some(-1_073_741_819),
        };
        assert!(failure(COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED).loses_the_page());
        assert!(failure(COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED).loses_the_page());
        for recovers in [
            COREWEBVIEW2_PROCESS_FAILED_KIND_FRAME_RENDER_PROCESS_EXITED,
            COREWEBVIEW2_PROCESS_FAILED_KIND_GPU_PROCESS_EXITED,
            COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE,
        ] {
            assert!(!failure(recovers).loses_the_page(), "{recovers:?}");
        }

        assert_eq!(
            failure(COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED).describe(),
            "the page's renderer process exited: crashed, exit code 0xc0000005"
        );
        assert_eq!(
            ProcessFailure {
                kind: COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED,
                reason: None,
                exit_code: None,
            }
            .describe(),
            "the browser process exited"
        );
    }

    #[test]
    fn the_ai_profile_cannot_start_without_the_egress_proxy() {
        assert!(browser_arguments(WebViewProfile::Ai, None).is_err());
    }

    #[test]
    fn the_ai_profile_routes_everything_but_loopback_through_the_proxy() {
        let arguments =
            browser_arguments(WebViewProfile::Ai, Some("http://127.0.0.1:4100")).unwrap();
        assert_eq!(
            arguments,
            [
                "--proxy-server=http://127.0.0.1:4100",
                // Only what the egress policy calls loopback; `*.localhost`
                // is not.
                "--proxy-bypass-list=<-loopback>;localhost;127.0.0.1/8;[::1]",
                "--host-resolver-rules=MAP * ~NOTFOUND, EXCLUDE localhost, EXCLUDE 127.*, EXCLUDE ::1, EXCLUDE [::1]",
                "--webrtc-ip-handling-policy=disable_non_proxied_udp",
                "--disable-quic",
                // An AI tab keeps working out of sight.
                "--disable-background-timer-throttling",
                "--disable-backgrounding-occluded-windows",
                "--disable-renderer-backgrounding",
            ]
        );
        assert_eq!(
            command_line(&arguments),
            "--proxy-server=http://127.0.0.1:4100 \
             --proxy-bypass-list=<-loopback>;localhost;127.0.0.1/8;[::1] \
             \"--host-resolver-rules=MAP * ~NOTFOUND, EXCLUDE localhost, EXCLUDE 127.*, EXCLUDE ::1, EXCLUDE [::1]\" \
             --webrtc-ip-handling-policy=disable_non_proxied_udp --disable-quic \
             --disable-background-timer-throttling --disable-backgrounding-occluded-windows \
             --disable-renderer-backgrounding"
        );
    }

    fn configuration(folder: &str, proxy_endpoint: Option<&str>) -> EnvironmentConfiguration {
        EnvironmentConfiguration {
            user_data_folder: PathBuf::from(folder),
            proxy_endpoint: proxy_endpoint.map(str::to_string),
        }
    }

    #[test]
    fn a_changed_proxy_is_only_refused_for_the_ai_profile() {
        let created = configuration("user", Some("http://127.0.0.1:1"));
        let moved = configuration("user", Some("http://127.0.0.1:2"));
        assert!(
            check_configuration_against_existing(WebViewProfile::User, &created, &moved).is_ok()
        );
        assert!(
            check_configuration_against_existing(WebViewProfile::User, &created, &created).is_ok()
        );

        let created = configuration("ai", Some("http://127.0.0.1:1"));
        let moved = configuration("ai", Some("http://127.0.0.1:2"));
        assert!(
            check_configuration_against_existing(WebViewProfile::Ai, &created, &moved).is_err()
        );
        assert!(
            check_configuration_against_existing(WebViewProfile::Ai, &created, &created).is_ok()
        );
    }

    #[test]
    fn the_two_profiles_never_share_a_user_data_folder() {
        ENVIRONMENTS.with(|environments| {
            let mut environments = environments.borrow_mut();
            environments.user.slot = EnvironmentSlot::Creating(Vec::new());
            environments.user.configuration = Some(configuration("shared", None));
        });
        let ai = configuration("shared", Some("http://127.0.0.1:1"));
        assert!(check_environment_configuration(WebViewProfile::Ai, &ai).is_err());
        let ai = configuration("separate", Some("http://127.0.0.1:1"));
        assert!(check_environment_configuration(WebViewProfile::Ai, &ai).is_ok());

        // A failed creation leaves nothing behind to clash with.
        ENVIRONMENTS.with(|environments| {
            environments.borrow_mut().user.slot = EnvironmentSlot::Idle;
        });
        let ai = configuration("shared", Some("http://127.0.0.1:1"));
        assert!(check_environment_configuration(WebViewProfile::Ai, &ai).is_ok());
    }

    #[test]
    fn each_profile_creates_its_own_environment() {
        let failures = Rc::new(RefCell::new(Vec::new()));
        let waiter = |name: &'static str| {
            let failures = failures.clone();
            Box::new(move |outcome: Result<ICoreWebView2Environment, String>| {
                if let Err(message) = outcome {
                    failures.borrow_mut().push((name, message));
                }
            }) as EnvironmentWaiter
        };

        assert!(matches!(
            queue_environment_waiter(WebViewProfile::User, waiter("user tab")),
            EnvironmentRequest::Start
        ));
        // The AI profile does not wait for the user's environment.
        assert!(matches!(
            queue_environment_waiter(WebViewProfile::Ai, waiter("ai tab")),
            EnvironmentRequest::Start
        ));
        assert!(matches!(
            queue_environment_waiter(WebViewProfile::User, waiter("second user tab")),
            EnvironmentRequest::Pending
        ));

        finish_environment(WebViewProfile::User, Err("no runtime".to_string()));
        assert_eq!(
            *failures.borrow(),
            [
                ("user tab", "no runtime".to_string()),
                ("second user tab", "no runtime".to_string()),
            ]
        );
        // The AI profile's creation is still in flight; the user's can retry.
        assert!(matches!(
            queue_environment_waiter(WebViewProfile::Ai, waiter("second ai tab")),
            EnvironmentRequest::Pending
        ));
        assert!(matches!(
            queue_environment_waiter(WebViewProfile::User, waiter("retry")),
            EnvironmentRequest::Start
        ));
    }

    #[test]
    fn webview2_environment_overrides_are_detected() {
        let overrides = environment_overrides(|name| match name {
            "WEBVIEW2_USER_DATA_FOLDER" => Some(OsString::from("C:\\elsewhere")),
            "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS" => Some(OsString::new()),
            _ => None,
        });
        assert_eq!(overrides, ["WEBVIEW2_USER_DATA_FOLDER"]);
        assert!(environment_overrides(|_| None).is_empty());

        let browser_arguments_set = |name: &str| {
            (name == "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS")
                .then(|| OsString::from("--no-proxy-server"))
        };
        assert!(check_environment_overrides(WebViewProfile::Ai, browser_arguments_set).is_err());
        assert!(check_environment_overrides(WebViewProfile::User, browser_arguments_set).is_ok());
        assert!(check_environment_overrides(WebViewProfile::Ai, |_| None).is_ok());
    }

    #[test]
    fn a_new_window_is_opened_in_place_only_when_the_filter_allows_it() {
        let deny_remote = |url: &str| {
            if url.starts_with("http://127.0.0.1") {
                NavigationDecision::Allow
            } else {
                NavigationDecision::Deny("remote".to_string())
            }
        };
        assert!(matches!(
            new_window_event(Some(&deny_remote), "https://example.com/".to_string()),
            Some(WebViewEvent::NavigationBlocked { url, reason })
                if url == "https://example.com/" && reason == "remote"
        ));
        assert!(matches!(
            new_window_event(Some(&deny_remote), "http://127.0.0.1:1/".to_string()),
            Some(WebViewEvent::OpenInPlace(url)) if url == "http://127.0.0.1:1/"
        ));
        // A denied empty address is still reported, never dropped.
        assert!(matches!(
            new_window_event(Some(&deny_remote), String::new()),
            Some(WebViewEvent::NavigationBlocked { .. })
        ));
        assert!(matches!(
            new_window_event(None, "https://example.com/".to_string()),
            Some(WebViewEvent::OpenInPlace(_))
        ));
        assert!(new_window_event(None, String::new()).is_none());
    }

    #[test]
    fn modifiers_map_to_the_devtools_bit_field() {
        assert_eq!(modifier_mask(&[]), 0);
        assert_eq!(modifier_mask(&[KeyModifier::Alt]), 1);
        assert_eq!(modifier_mask(&[KeyModifier::Control]), 2);
        assert_eq!(modifier_mask(&[KeyModifier::Meta]), 4);
        assert_eq!(modifier_mask(&[KeyModifier::Shift]), 8);
        assert_eq!(
            modifier_mask(&[
                KeyModifier::Control,
                KeyModifier::Shift,
                KeyModifier::Control
            ]),
            10
        );
    }

    #[test]
    fn keys_map_to_windows_virtual_key_codes() {
        assert_eq!(windows_virtual_key_code("a", "KeyA"), Some(0x41));
        assert_eq!(windows_virtual_key_code("A", ""), Some(0x41));
        assert_eq!(windows_virtual_key_code("z", ""), Some(0x5A));
        assert_eq!(windows_virtual_key_code("7", "Digit7"), Some(0x37));
        assert_eq!(windows_virtual_key_code("&", ""), Some(0x37));
        assert_eq!(windows_virtual_key_code("7", "Numpad7"), Some(0x67));
        assert_eq!(windows_virtual_key_code("Enter", "Enter"), Some(0x0D));
        assert_eq!(windows_virtual_key_code("Enter", ""), Some(0x0D));
        assert_eq!(windows_virtual_key_code("Backspace", ""), Some(0x08));
        assert_eq!(windows_virtual_key_code("Tab", ""), Some(0x09));
        assert_eq!(windows_virtual_key_code("Escape", ""), Some(0x1B));
        assert_eq!(windows_virtual_key_code(" ", ""), Some(0x20));
        assert_eq!(windows_virtual_key_code("ArrowDown", ""), Some(0x28));
        assert_eq!(windows_virtual_key_code("Delete", ""), Some(0x2E));
        assert_eq!(windows_virtual_key_code("F1", ""), Some(0x70));
        assert_eq!(windows_virtual_key_code("F12", "F12"), Some(0x7B));
        assert_eq!(windows_virtual_key_code("F25", ""), None);
        assert_eq!(
            windows_virtual_key_code("Control", "ControlLeft"),
            Some(0x11)
        );
        assert_eq!(windows_virtual_key_code("Meta", ""), Some(0x5B));
        assert_eq!(windows_virtual_key_code("?", ""), Some(0xBF));
        assert_eq!(windows_virtual_key_code("é", ""), None);
        assert_eq!(windows_virtual_key_code("Dead", ""), None);
        // The physical key wins over the character it typed.
        assert_eq!(windows_virtual_key_code("q", "KeyA"), Some(0x41));
    }

    #[test]
    fn only_unmodified_or_shifted_presses_type_text() {
        assert_eq!(key_text("a", &[]), Some("a"));
        assert_eq!(key_text("A", &[KeyModifier::Shift]), Some("A"));
        assert_eq!(key_text("é", &[]), Some("é"));
        assert_eq!(key_text("Enter", &[]), Some("\r"));
        assert_eq!(key_text("a", &[KeyModifier::Control]), None);
        assert_eq!(key_text("a", &[KeyModifier::Alt]), None);
        assert_eq!(key_text("Backspace", &[]), None);
        assert_eq!(key_text("Tab", &[]), None);
    }

    #[test]
    fn a_double_click_is_a_move_and_two_numbered_press_release_pairs() {
        let calls = devtools_calls(&NativeInput::MouseClick {
            x: 10.5,
            y: 20.,
            button: MouseButton::Right,
            click_count: 2,
            modifiers: vec![KeyModifier::Shift],
        });
        let expected = [
            serde_json::json!({ "type": "mouseMoved", "x": 10.5, "y": 20.0, "modifiers": 8 }),
            serde_json::json!({ "type": "mousePressed", "x": 10.5, "y": 20.0, "button": "right", "buttons": 2, "clickCount": 1, "modifiers": 8 }),
            serde_json::json!({ "type": "mouseReleased", "x": 10.5, "y": 20.0, "button": "right", "buttons": 0, "clickCount": 1, "modifiers": 8 }),
            serde_json::json!({ "type": "mousePressed", "x": 10.5, "y": 20.0, "button": "right", "buttons": 2, "clickCount": 2, "modifiers": 8 }),
            serde_json::json!({ "type": "mouseReleased", "x": 10.5, "y": 20.0, "button": "right", "buttons": 0, "clickCount": 2, "modifiers": 8 }),
        ];
        assert_eq!(calls.len(), expected.len());
        for ((method, parameters), expected) in calls.iter().zip(expected) {
            assert_eq!(*method, "Input.dispatchMouseEvent");
            assert_eq!(*parameters, expected);
        }

        // A click count of zero still clicks once.
        let calls = devtools_calls(&NativeInput::MouseClick {
            x: 0.,
            y: 0.,
            button: MouseButton::Left,
            click_count: 0,
            modifiers: Vec::new(),
        });
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[1].1["button"], "left");
        assert_eq!(calls[1].1["buttons"], 1);
    }

    #[test]
    fn moves_wheels_and_text_are_single_calls() {
        assert_eq!(
            devtools_calls(&NativeInput::MouseMove { x: 1., y: 2. }),
            [(
                "Input.dispatchMouseEvent",
                serde_json::json!({ "type": "mouseMoved", "x": 1.0, "y": 2.0 })
            )]
        );
        assert_eq!(
            devtools_calls(&NativeInput::Wheel {
                x: 1.,
                y: 2.,
                delta_x: 0.,
                delta_y: 300.,
            }),
            [(
                "Input.dispatchMouseEvent",
                serde_json::json!({
                    "type": "mouseWheel", "x": 1.0, "y": 2.0, "deltaX": 0.0, "deltaY": 300.0,
                })
            )]
        );
        assert_eq!(
            devtools_calls(&NativeInput::InsertText("日本語".to_string())),
            [("Input.insertText", serde_json::json!({ "text": "日本語" }))]
        );
    }

    #[test]
    fn a_printable_key_types_its_text_and_a_shortcut_does_not() {
        assert_eq!(
            devtools_calls(&NativeInput::Key {
                key: "a".to_string(),
                code: "KeyA".to_string(),
                modifiers: Vec::new(),
            }),
            [
                (
                    "Input.dispatchKeyEvent",
                    serde_json::json!({
                        "type": "keyDown", "key": "a", "code": "KeyA", "modifiers": 0,
                        "windowsVirtualKeyCode": 65, "nativeVirtualKeyCode": 65,
                        "text": "a", "unmodifiedText": "a",
                    })
                ),
                (
                    "Input.dispatchKeyEvent",
                    serde_json::json!({
                        "type": "keyUp", "key": "a", "code": "KeyA", "modifiers": 0,
                        "windowsVirtualKeyCode": 65, "nativeVirtualKeyCode": 65,
                    })
                ),
            ]
        );
        assert_eq!(
            devtools_calls(&NativeInput::Key {
                key: "a".to_string(),
                code: "KeyA".to_string(),
                modifiers: vec![KeyModifier::Control],
            })[0]
                .1,
            serde_json::json!({
                "type": "rawKeyDown", "key": "a", "code": "KeyA", "modifiers": 2,
                "windowsVirtualKeyCode": 65, "nativeVirtualKeyCode": 65,
            })
        );
        // A key without a known virtual-key code goes without one.
        assert_eq!(
            devtools_calls(&NativeInput::Key {
                key: "Unidentified".to_string(),
                code: String::new(),
                modifiers: Vec::new(),
            })[0]
                .1,
            serde_json::json!({
                "type": "rawKeyDown", "key": "Unidentified", "code": "", "modifiers": 0,
            })
        );
    }
}

/// Drives a real WebView2 against servers on this machine. Needs the WebView2
/// Runtime and a desktop session (a small window shows for a few seconds), so
/// it only runs when asked for:
/// `cargo test -p browser_panel live_ai_tab -- --ignored --nocapture`.
#[cfg(test)]
mod live_tests {
    use super::*;
    use std::{
        io::{Read as _, Write as _},
        net::{TcpListener, TcpStream},
        sync::{Arc, Mutex},
        thread,
    };
    use windows::{
        Win32::{
            Foundation::{LRESULT, WPARAM},
            Graphics::Gdi::{MONITOR_DEFAULTTONULL, MonitorFromRect},
            System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx},
            UI::WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EnumWindows,
                GetWindowTextW, GetWindowThreadProcessId, MSG, PM_REMOVE, PeekMessageW,
                RegisterClassW, SW_MINIMIZE, SW_RESTORE, SWP_NOZORDER, TranslateMessage, WNDCLASSW,
                WS_CLIPCHILDREN, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
                WS_VISIBLE,
            },
        },
        core::w,
    };

    const PAGE: &str = r#"<!doctype html><html><head><title>smoke</title></head>
<body style="margin:0">
<button id="target" style="position:absolute;left:20px;top:20px;width:200px;height:60px"
  onclick="window.__clicked = event.isTrusted">Click</button>
<input id="field" style="position:absolute;left:20px;top:120px;width:200px;height:30px">
<a id="popup" href="https://example.com/" target="_blank"
  style="position:absolute;left:20px;top:200px;width:200px;height:30px;display:block">popup</a>
<button id="opener" style="position:absolute;left:20px;top:250px;width:200px;height:30px"
  onclick="window.__popup = window.open('/ping')">open</button>
<input id="upload" type="file" style="position:absolute;left:20px;top:320px;width:200px;height:30px"
  oncancel="window.__upload = 'cancelled'" onchange="window.__upload = 'chosen'">
<div style="height:3000px"></div>
<script>addEventListener('beforeprint', () => { window.__printed = true; });</script>
</body></html>"#;

    /// Everything a server saw: the request line of each request.
    type RequestLog = Arc<Mutex<Vec<String>>>;

    fn serve(listener: TcpListener, respond: fn(&str) -> Vec<u8>) -> RequestLog {
        let log = RequestLog::default();
        thread::spawn({
            let log = log.clone();
            move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    // One thread per connection: Chromium opens speculative
                    // connections that send nothing until they are used.
                    let log = log.clone();
                    thread::spawn(move || {
                        let request_line = read_request_line(&mut stream);
                        if request_line.is_empty() {
                            return;
                        }
                        log.lock().unwrap().push(request_line.clone());
                        stream.write_all(&respond(&request_line)).ok();
                    });
                }
            }
        });
        log
    }

    fn read_request_line(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0u8; 4096];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => request.extend_from_slice(&buffer[..read]),
            }
        }
        String::from_utf8_lossy(&request)
            .lines()
            .next()
            .unwrap_or_default()
            .to_string()
    }

    fn response(status: &str, headers: &[&str], body: &[u8]) -> Vec<u8> {
        let mut response = format!("HTTP/1.1 {status}\r\nConnection: close\r\n");
        for header in headers {
            response.push_str(header);
            response.push_str("\r\n");
        }
        response.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
        let mut response = response.into_bytes();
        response.extend_from_slice(body);
        response
    }

    fn respond_as_site(request_line: &str) -> Vec<u8> {
        let path = request_line.split(' ').nth(1).unwrap_or("/");
        match path {
            "/" => response("200 OK", &["Content-Type: text/html"], PAGE.as_bytes()),
            "/ping" => response(
                "200 OK",
                &["Content-Type: text/plain", "Access-Control-Allow-Origin: *"],
                b"pong",
            ),
            "/download" => response(
                "200 OK",
                &[
                    "Content-Type: application/octet-stream",
                    "Content-Disposition: attachment; filename=\"smoke.bin\"",
                ],
                &[0u8; 64],
            ),
            "/auth" => response(
                "401 Unauthorized",
                &[
                    "Content-Type: text/plain",
                    "WWW-Authenticate: Basic realm=\"live test\"",
                ],
                b"sign-in required",
            ),
            _ => response("404 Not Found", &[], b""),
        }
    }

    fn respond_as_proxy(_request_line: &str) -> Vec<u8> {
        response("403 Forbidden", &[], b"denied by the test proxy")
    }

    unsafe extern "system" fn window_procedure(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }

    fn create_host_window() -> HWND {
        let class_name = w!("BrowserPanelLiveTestHost");
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_procedure),
            lpszClassName: class_name,
            ..Default::default()
        };
        unsafe { RegisterClassW(&class) };
        unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                class_name,
                w!("browser panel live test"),
                WS_POPUP | WS_VISIBLE | WS_CLIPCHILDREN,
                0,
                0,
                800,
                600,
                None,
                None,
                None,
                None,
            )
        }
        .expect("the host window could not be created")
    }

    fn pump_messages() {
        let mut message = MSG::default();
        while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            let _translated = unsafe { TranslateMessage(&message) };
            unsafe { DispatchMessageW(&message) };
        }
    }

    /// Pumps the UI thread's messages, which is where WebView2 delivers its
    /// callbacks, until `poll` has an answer.
    fn pump_until<T>(what: &str, timeout: Duration, mut poll: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + timeout;
        loop {
            pump_messages();
            if let Some(value) = poll() {
                return value;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn pump_for(duration: Duration) {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            pump_messages();
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait<T>(what: &str, mut receiver: oneshot::Receiver<Result<T>>) -> T {
        pump_until(what, Duration::from_secs(20), || {
            receiver
                .try_recv()
                .expect("the answer was dropped")
                .map(|answer| answer.unwrap_or_else(|error| panic!("{what} failed: {error:#}")))
        })
    }

    #[derive(Debug, PartialEq)]
    enum Observed {
        Ready,
        Failed(String),
        Crashed(String),
        LoadFinished(bool),
        NavigationBlocked(String),
        DownloadBlocked(String),
        OpenInPlace(String),
    }

    struct Surface {
        webview: WebView,
        events: futures::channel::mpsc::UnboundedReceiver<WebViewEvent>,
        observed: Vec<Observed>,
    }

    impl Surface {
        fn new(host: HWND, options: WebViewOptions) -> Self {
            Self::with_viewport(host, options, on_screen_viewport())
        }

        /// What the view does for a command that reaches an AI tab which was
        /// never painted (`BrowserView::ensure_background_surface`).
        fn never_shown(host: HWND, options: WebViewOptions) -> Self {
            Self::with_viewport(
                host,
                options,
                Viewport {
                    origin_x: 0.,
                    origin_y: 0.,
                    width: BACKGROUND_VIEWPORT_WIDTH,
                    height: BACKGROUND_VIEWPORT_HEIGHT,
                    scale_factor: 1.,
                    hidden_reason: Some(PARKED_IN_BACKGROUND),
                    input_blocked: false,
                    overlays: Vec::new(),
                },
            )
        }

        fn with_viewport(host: HWND, options: WebViewOptions, viewport: Viewport) -> Self {
            let (sender, events) = futures::channel::mpsc::unbounded();
            let webview = WebView::new(host.0 as isize, options, sender)
                .expect("the surface could not be requested");
            let deferred = webview.sync_viewport(viewport, false);
            assert!(!deferred);
            Self {
                webview,
                events,
                observed: Vec::new(),
            }
        }

        fn drain(&mut self) {
            while let Ok(event) = self.events.try_recv() {
                self.observed.push(match event {
                    WebViewEvent::Ready => Observed::Ready,
                    WebViewEvent::Failed(message) => Observed::Failed(message),
                    WebViewEvent::Crashed(message) => Observed::Crashed(message),
                    WebViewEvent::LoadFinished { succeeded } => Observed::LoadFinished(succeeded),
                    WebViewEvent::NavigationBlocked { url, .. } => Observed::NavigationBlocked(url),
                    WebViewEvent::DownloadBlocked { url } => Observed::DownloadBlocked(url),
                    WebViewEvent::OpenInPlace(url) => Observed::OpenInPlace(url),
                    _ => continue,
                });
            }
        }

        /// Waits for an event matching `wanted` that arrives after this call.
        /// Starts a new step: events seen so far no longer count.
        fn forget(&mut self) {
            self.drain();
            self.observed.clear();
        }

        /// Waits for an event matching `wanted` since the last `forget`.
        fn wait_for(&mut self, what: &str, wanted: impl Fn(&Observed) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                pump_messages();
                self.drain();
                if let Some(failure) = self
                    .observed
                    .iter()
                    .find(|observed| matches!(observed, Observed::Failed(_)))
                {
                    panic!("the surface failed: {failure:?}");
                }
                if self.observed.iter().any(&wanted) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for {what}; saw {:?}",
                    self.observed
                );
                thread::sleep(Duration::from_millis(5));
            }
        }

        /// Whether an event matching `wanted` arrives (since the last
        /// `forget`) within `timeout`.
        fn saw(&mut self, timeout: Duration, wanted: impl Fn(&Observed) -> bool) -> bool {
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                pump_messages();
                self.drain();
                if self.observed.iter().any(&wanted) {
                    return true;
                }
                thread::sleep(Duration::from_millis(5));
            }
            false
        }

        fn script(&mut self, script: &str) -> String {
            wait(script, self.webview.execute_script(script))
        }

        fn input(&mut self, input: NativeInput) {
            wait(&format!("{input:?}"), self.webview.dispatch_input(input));
        }

        fn load(&mut self, url: &str) {
            self.forget();
            self.webview.navigate(url);
            self.wait_for(url, |observed| *observed == Observed::LoadFinished(true));
        }

        fn ice_candidate_count(&mut self) -> usize {
            self.script(
                "(() => { window.__candidates = []; const connection = new RTCPeerConnection(); \
                 connection.createDataChannel('probe'); \
                 connection.onicecandidate = (event) => { if (event.candidate && event.candidate.candidate) window.__candidates.push(event.candidate.candidate); }; \
                 connection.createOffer().then((offer) => connection.setLocalDescription(offer)); \
                 window.__connection = connection; return true; })()",
            );
            pump_until("ICE gathering", Duration::from_secs(20), || {
                (self.script("window.__connection.iceGatheringState") == "\"complete\"")
                    .then_some(())
            });
            self.script("window.__candidates.length").parse().unwrap()
        }
    }

    fn on_screen_viewport() -> Viewport {
        Viewport {
            origin_x: 0.,
            origin_y: 0.,
            width: 800.,
            height: 600.,
            scale_factor: 1.,
            hidden_reason: None,
            input_blocked: false,
            overlays: Vec::new(),
        }
    }

    unsafe extern "system" fn collect_window_title(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let titles = unsafe { &mut *(lparam.0 as *mut Vec<String>) };
        let mut title = [0u16; 256];
        let length = unsafe { GetWindowTextW(hwnd, &mut title) };
        titles.push(String::from_utf16_lossy(&title[..length.max(0) as usize]));
        true.into()
    }

    unsafe extern "system" fn collect_visible_dialog(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let dialogs = unsafe { &mut *(lparam.0 as *mut Vec<isize>) };
        let mut class_name = [0u16; 64];
        let length = unsafe { GetClassNameW(hwnd, &mut class_name) };
        let is_dialog = String::from_utf16_lossy(&class_name[..length.max(0) as usize]) == "#32770";
        if is_dialog && unsafe { IsWindowVisible(hwnd) }.as_bool() {
            dialogs.push(hwnd.0 as isize);
        }
        true.into()
    }

    /// The visible standard dialogs on the desktop, such as a file dialog.
    fn visible_dialogs() -> Vec<isize> {
        let mut dialogs: Vec<isize> = Vec::new();
        unsafe {
            EnumWindows(
                Some(collect_visible_dialog),
                LPARAM(&mut dialogs as *mut _ as isize),
            )
        }
        .unwrap();
        dialogs
    }

    fn top_level_window_titles() -> Vec<String> {
        let mut titles: Vec<String> = Vec::new();
        unsafe {
            EnumWindows(
                Some(collect_window_title),
                LPARAM(&mut titles as *mut _ as isize),
            )
        }
        .unwrap();
        titles
    }

    fn scratch_folder(name: &str) -> PathBuf {
        let folder =
            std::env::temp_dir().join(format!("browser_panel_live_{}_{name}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        folder
    }

    fn loopback_only(url: &str) -> NavigationDecision {
        let allowed = ["http://127.0.0.1:", "http://localhost:", "http://[::1]:"]
            .iter()
            .any(|prefix| url.starts_with(prefix))
            || url == "about:blank"
            || url.starts_with("data:");
        if allowed {
            NavigationDecision::Allow
        } else {
            NavigationDecision::Deny("not on the test allowlist".to_string())
        }
    }

    #[test]
    #[ignore = "drives a real WebView2; run with --ignored on a Windows desktop"]
    fn live_ai_tab() {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .unwrap();

        let site = TcpListener::bind("127.0.0.1:0").unwrap();
        let site_port = site.local_addr().unwrap().port();
        let site_requests = serve(site, respond_as_site);
        let ipv6_site = TcpListener::bind("[::1]:0").ok().map(|listener| {
            let port = listener.local_addr().unwrap().port();
            (port, serve(listener, respond_as_site))
        });
        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_endpoint = format!("http://127.0.0.1:{}", proxy.local_addr().unwrap().port());
        let proxy_requests = serve(proxy, respond_as_proxy);
        let base = format!("http://127.0.0.1:{site_port}");

        let host = create_host_window();
        let mut ai = Surface::new(
            host,
            WebViewOptions {
                profile: WebViewProfile::Ai,
                user_data_folder: scratch_folder("ai"),
                proxy_endpoint: Some(proxy_endpoint),
                document_start_script: Some("window.__injected = 'yes';".to_string()),
                navigation_filter: Some(Rc::new(loopback_only)),
                block_downloads: true,
            },
        );
        ai.wait_for("the AI surface", |observed| *observed == Observed::Ready);
        // A download that got through would otherwise land in the user's own
        // Downloads folder, where the test could not tell it from theirs.
        let downloads = scratch_folder("downloads");
        {
            let webview = ai
                .webview
                .inner
                .webview()
                .unwrap()
                .cast::<ICoreWebView2_13>()
                .unwrap();
            let profile = unsafe { webview.Profile() }.unwrap();
            let folder = HSTRING::from(downloads.as_os_str());
            unsafe { profile.SetDefaultDownloadFolderPath(PCWSTR(folder.as_ptr())) }.unwrap();
        }

        ai.load(&format!("{base}/"));
        assert_eq!(ai.script("1+1"), "2");
        assert_eq!(ai.script("window.__injected"), "\"yes\"");
        println!("document start script and execute_script: ok");

        let png = wait("the screenshot", ai.webview.capture_png());
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"), "not a PNG");
        let width = u32::from_be_bytes(png[16..20].try_into().unwrap());
        let height = u32::from_be_bytes(png[20..24].try_into().unwrap());
        assert_eq!((width, height), (800, 600));
        println!("capture_png: {} bytes, {width}x{height}", png.len());

        ai.input(NativeInput::MouseClick {
            x: 120.,
            y: 50.,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: Vec::new(),
        });
        assert_eq!(ai.script("window.__clicked"), "true");
        println!("trusted click: ok");

        ai.input(NativeInput::MouseClick {
            x: 120.,
            y: 135.,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: Vec::new(),
        });
        let key = |key: &str, code: &str, modifiers: Vec<KeyModifier>| NativeInput::Key {
            key: key.to_string(),
            code: code.to_string(),
            modifiers,
        };
        ai.input(key("a", "KeyA", Vec::new()));
        ai.input(key("B", "KeyB", vec![KeyModifier::Shift]));
        ai.input(NativeInput::InsertText("-insert".to_string()));
        ai.input(key("Backspace", "Backspace", Vec::new()));
        assert_eq!(
            ai.script("document.getElementById('field').value"),
            "\"aB-inser\""
        );
        ai.input(key("a", "KeyA", vec![KeyModifier::Control]));
        ai.input(key("Backspace", "Backspace", Vec::new()));
        assert_eq!(ai.script("document.getElementById('field').value"), "\"\"");
        println!("keys and inserted text: ok");

        ai.script(
            "(() => { window.__geolocation = 'pending'; navigator.geolocation.getCurrentPosition(\
             () => { window.__geolocation = 'granted'; }, \
             (error) => { window.__geolocation = 'denied:' + error.code; }); return true; })()",
        );
        let geolocation = pump_until("the geolocation answer", Duration::from_secs(20), || {
            let answer = ai.script("window.__geolocation");
            (answer != "\"pending\"").then_some(answer)
        });
        assert_eq!(geolocation, "\"denied:1\"", "the permission was not denied");
        println!("denied permission: ok");

        let dialogs_before = visible_dialogs();
        ai.input(NativeInput::MouseClick {
            x: 120.,
            y: 335.,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: Vec::new(),
        });
        pump_for(Duration::from_secs(2));
        let new_dialogs = visible_dialogs()
            .into_iter()
            .filter(|dialog| !dialogs_before.contains(dialog))
            .collect::<Vec<_>>();
        for dialog in &new_dialogs {
            unsafe {
                windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(HWND(*dialog as *mut c_void)),
                    windows::Win32::UI::WindowsAndMessaging::WM_CLOSE,
                    WPARAM(0),
                    LPARAM(0),
                )
            }
            .unwrap();
        }
        assert!(
            new_dialogs.is_empty(),
            "a dialog opened: {:?}",
            new_dialogs
                .iter()
                .map(|dialog| {
                    let mut title = [0u16; 256];
                    let length =
                        unsafe { GetWindowTextW(HWND(*dialog as *mut c_void), &mut title) };
                    String::from_utf16_lossy(&title[..length.max(0) as usize])
                })
                .collect::<Vec<_>>()
        );
        println!(
            "file chooser suppressed: ok (page saw {})",
            ai.script("window.__upload")
        );

        ai.input(key("p", "KeyP", vec![KeyModifier::Control]));
        pump_for(Duration::from_secs(2));
        // Trusted keys reach the page, not the browser's own shortcuts: the
        // AI cannot open the print preview (and print) with them.
        assert_eq!(ai.script("window.__printed === true"), "false");
        println!("ctrl+p does not print: ok");

        ai.input(NativeInput::Wheel {
            x: 400.,
            y: 300.,
            delta_x: 0.,
            delta_y: 300.,
        });
        pump_until("the wheel scroll", Duration::from_secs(5), || {
            (ai.script("window.scrollY") != "0").then_some(())
        });
        ai.script("window.scrollTo(0, 0)");
        println!("wheel: ok");

        ai.forget();
        ai.webview.navigate("https://example.com/");
        ai.wait_for("the blocked navigation", |observed| {
            *observed == Observed::NavigationBlocked("https://example.com/".to_string())
        });
        assert_eq!(ai.script("location.href"), format!("\"{base}/\""));
        println!("denied navigation: ok");

        ai.forget();
        ai.input(NativeInput::MouseClick {
            x: 120.,
            y: 215.,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: Vec::new(),
        });
        ai.wait_for("the blocked popup", |observed| {
            *observed == Observed::NavigationBlocked("https://example.com/".to_string())
        });
        assert!(
            !ai.observed
                .iter()
                .any(|observed| matches!(observed, Observed::OpenInPlace(_)))
        );
        println!("denied popup: ok");

        ai.forget();
        ai.input(NativeInput::MouseClick {
            x: 120.,
            y: 265.,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: Vec::new(),
        });
        ai.wait_for("the popup opened in place", |observed| {
            *observed == Observed::OpenInPlace(format!("{base}/ping"))
        });
        // No window of its own, which would have none of the restrictions.
        assert_eq!(ai.script("window.__popup === null"), "true");
        println!("allowed popup kept in place: ok");

        let mut probes = vec![
            "http://169.254.1.1/link-local".to_string(),
            "http://example.invalid/remote".to_string(),
            format!("http://probe.localhost:{site_port}/ping"),
            format!("http://localhost:{site_port}/ping"),
        ];
        if let Some((port, _)) = &ipv6_site {
            probes.push(format!("http://[::1]:{port}/ping"));
        }
        ai.script(&format!(
            "(() => {{ for (const url of {}) fetch(url).catch(() => {{}}); return true; }})()",
            serde_json::to_string(&probes).unwrap()
        ));
        pump_until("the probes", Duration::from_secs(20), || {
            let proxied = proxy_requests.lock().unwrap().clone();
            (proxied.iter().any(|line| line.contains("169.254.1.1"))
                && proxied.iter().any(|line| line.contains("example.invalid"))
                && proxied.iter().any(|line| line.contains("probe.localhost"))
                && site_requests
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|line| line.starts_with("GET /ping")))
            .then_some(())
        });
        if let Some((_, ipv6_requests)) = &ipv6_site {
            pump_until("the IPv6 loopback probe", Duration::from_secs(20), || {
                ipv6_requests
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|line| line.starts_with("GET /ping"))
                    .then_some(())
            });
        }
        let proxied = proxy_requests.lock().unwrap().clone();
        assert!(
            !proxied
                .iter()
                .any(|line| line.contains("//localhost:") || line.contains("[::1]")),
            "loopback went through the proxy: {proxied:?}"
        );
        println!("proxy routing: {proxied:?}");

        let ai_candidates = ai.ice_candidate_count();
        println!("WebRTC candidates in the AI profile: {ai_candidates}");
        assert_eq!(ai_candidates, 0);

        ai.forget();
        ai.webview.navigate(&format!("{base}/download"));
        ai.wait_for("the blocked download", |observed| {
            matches!(observed, Observed::DownloadBlocked(url) if url.ends_with("/download"))
        });
        pump_for(Duration::from_secs(2));
        let saved = std::fs::read_dir(&downloads)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert!(saved.is_empty(), "the download was saved: {saved:?}");
        println!("blocked download: ok");

        // Dialogs the page script did not answer (this test injects none) are answered without
        // being shown: a native one would sit over the editor and block the page and the
        // script asking for it.
        let dialogs_before = visible_dialogs();
        assert_eq!(ai.script("confirm('Delete everything?')"), "false");
        assert_eq!(ai.script("prompt('Name?', 'default')"), "null");
        assert_eq!(ai.script("(alert('Hello'), 'answered')"), "\"answered\"");
        assert!(
            visible_dialogs()
                .iter()
                .all(|dialog| dialogs_before.contains(dialog)),
            "a script dialog was shown"
        );
        println!("script dialogs answered unseen: ok");

        // A page only gets to ask before it is left once the user has interacted with it.
        ai.script(
            "(() => { addEventListener('beforeunload', (event) => { event.preventDefault(); \
             event.returnValue = ''; }); return true; })()",
        );
        ai.input(NativeInput::MouseClick {
            x: 120.,
            y: 50.,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: Vec::new(),
        });
        ai.load(&format!("{base}/ping"));
        assert_eq!(ai.script("location.pathname"), "\"/ping\"");
        println!("beforeunload leaves the page: ok");

        ai.forget();
        ai.webview.navigate(&format!("{base}/auth"));
        // A sign-in prompt would hold the navigation until someone answered it.
        ai.wait_for("the sign-in page", |observed| {
            matches!(observed, Observed::LoadFinished(_))
        });
        assert!(
            site_requests
                .lock()
                .unwrap()
                .iter()
                .any(|line| line.starts_with("GET /auth")),
            "the server was not asked"
        );
        // Cancelled, the server's own 401 page shows (unanswered, WebView2 shows an error page
        // under its prompt).
        pump_until("the 401 page", Duration::from_secs(10), || {
            (ai.script("document.body ? document.body.innerText : ''") == "\"sign-in required\"")
                .then_some(())
        });
        assert!(
            visible_dialogs()
                .iter()
                .all(|dialog| dialogs_before.contains(dialog)),
            "a sign-in dialog was shown"
        );
        println!("HTTP sign-in prompt cancelled: ok");

        // The user's profile runs beside it, in its own environment, without
        // any of the restrictions.
        let mut user = Surface::new(
            host,
            WebViewOptions {
                profile: WebViewProfile::User,
                user_data_folder: scratch_folder("user"),
                proxy_endpoint: None,
                document_start_script: None,
                navigation_filter: None,
                block_downloads: false,
            },
        );
        user.wait_for("the user surface", |observed| *observed == Observed::Ready);

        // Closed before its page existed: whoever asked still gets an answer.
        let (sender, _events) = futures::channel::mpsc::unbounded();
        let closed_early = WebView::new(
            host.0 as isize,
            WebViewOptions {
                profile: WebViewProfile::User,
                user_data_folder: scratch_folder("user"),
                proxy_endpoint: None,
                document_start_script: None,
                navigation_filter: None,
                block_downloads: false,
            },
            sender,
        )
        .unwrap();
        let mut answer = closed_early.execute_script("1");
        drop(closed_early);
        assert!(
            matches!(answer.try_recv(), Ok(Some(Err(_)))),
            "a page operation of a closed tab was left unanswered"
        );
        println!("closed tab answers: ok");

        user.load(&format!("{base}/"));
        assert_eq!(user.script("typeof window.__injected"), "\"undefined\"");
        let user_candidates = user.ice_candidate_count();
        println!("WebRTC candidates in the user profile: {user_candidates}");
        assert!(
            user_candidates > 0,
            "the WebRTC probe gathers nothing at all"
        );

        // Put away, a user tab's page stops rendering where it was; only an AI
        // tab's page is parked.
        let shown_at = user.webview.inner.state.borrow().applied_bounds;
        user.webview.hide("the test put the tab away");
        pump_for(Duration::from_millis(300));
        {
            let state = user.webview.inner.state.borrow();
            assert_eq!(
                state.applied_rendering,
                Some(false),
                "the user tab kept rendering"
            );
            assert_eq!(state.applied_bounds, shown_at, "the user tab was moved");
        }
        assert_eq!(user.script("document.visibilityState"), "\"hidden\"");
        println!("user tab hidden, not parked: ok");

        ai.webview.open_developer_tools();
        pump_until(
            "the developer tools window",
            Duration::from_secs(20),
            || {
                top_level_window_titles()
                    .iter()
                    .any(|title| title.starts_with("DevTools"))
                    .then_some(())
            },
        );
        println!("developer tools: ok");

        drop(user);
        drop(ai);
        pump_for(Duration::from_millis(500));
        unsafe { DestroyWindow(host) }.unwrap();
    }

    const BUSY_PAGE: &str = r#"<!doctype html><html><head><title>busy</title></head>
<body style="margin:0;background:linear-gradient(90deg,#d02020,#2020d0)">
<h1 style="color:#fff;font:bold 64px sans-serif;margin:40px">Background page</h1>
<div id="box" style="width:300px;height:200px;background:#20c020"></div>
<button style="position:absolute;left:20px;top:420px;width:200px;height:60px"
  onclick="window.__clicked = event.isTrusted">Click</button>
</body></html>"#;

    fn respond_as_busy_site(request_line: &str) -> Vec<u8> {
        let path = request_line.split(' ').nth(1).unwrap_or("/");
        match path {
            "/" => response("200 OK", &["Content-Type: text/html"], BUSY_PAGE.as_bytes()),
            _ => response("404 Not Found", &[], b""),
        }
    }

    /// The width, height and the standard deviation of the luminance: a page
    /// that rendered nothing (blank, all one colour) has a spread near zero.
    fn image_spread(png: &[u8]) -> (u32, u32, f64) {
        let image = image::load_from_memory_with_format(png, image::ImageFormat::Png)
            .expect("the capture is a PNG")
            .to_rgba8();
        let luminances = image
            .pixels()
            .map(|pixel| {
                0.299 * f64::from(pixel[0])
                    + 0.587 * f64::from(pixel[1])
                    + 0.114 * f64::from(pixel[2])
            })
            .collect::<Vec<_>>();
        let count = luminances.len().max(1) as f64;
        let mean = luminances.iter().sum::<f64>() / count;
        let variance = luminances
            .iter()
            .map(|luminance| (luminance - mean).powi(2))
            .sum::<f64>()
            / count;
        (image.width(), image.height(), variance.sqrt())
    }

    /// Counts timer ticks and animation frames for a while, and reads what
    /// the page thinks of its own visibility.
    fn liveliness(surface: &mut Surface, duration: Duration) -> (u32, u32, String) {
        surface.script(
            "(() => { window.__ticks = 0; window.__frames = 0; \
             clearInterval(window.__timer); window.__timer = setInterval(() => window.__ticks++, 50); \
             const frame = () => { window.__frames++; requestAnimationFrame(frame); }; \
             if (!window.__animating) { window.__animating = true; requestAnimationFrame(frame); } \
             return true; })()",
        );
        pump_for(duration);
        let ticks = surface.script("window.__ticks").parse().unwrap();
        let frames = surface.script("window.__frames").parse().unwrap();
        let visibility = surface.script("document.visibilityState");
        (ticks, frames, visibility)
    }

    fn assert_alive(surface: &mut Surface, what: &str, size: (u32, u32)) {
        let (ticks, frames, visibility) = liveliness(surface, Duration::from_millis(1500));
        println!("{what}: {ticks} timer ticks, {frames} frames, visibility {visibility}");
        assert_eq!(
            visibility, "\"visible\"",
            "{what}: the page thinks it is hidden"
        );
        assert!(ticks >= 20, "{what}: timers were throttled ({ticks} ticks)");
        assert!(frames >= 20, "{what}: rendering stopped ({frames} frames)");
        let png = wait(what, surface.webview.capture_png());
        let (width, height, spread) = image_spread(&png);
        println!("{what}: capture {width}x{height}, luminance spread {spread:.1}");
        assert_eq!(
            (width, height),
            size,
            "{what}: the capture has the wrong size"
        );
        assert!(
            spread > 20.,
            "{what}: the capture is blank (spread {spread:.1})"
        );
    }

    fn ai_options(name: &str, proxy_endpoint: &str) -> WebViewOptions {
        WebViewOptions {
            profile: WebViewProfile::Ai,
            user_data_folder: scratch_folder(name),
            proxy_endpoint: Some(proxy_endpoint.to_string()),
            document_start_script: None,
            navigation_filter: Some(Rc::new(loopback_only)),
            block_downloads: true,
        }
    }

    fn browser_process_id(surface: &Surface) -> u32 {
        let webview = surface.webview.inner.webview().unwrap();
        let mut process_id = 0u32;
        unsafe { webview.BrowserProcessId(&mut process_id) }.unwrap();
        process_id
    }

    /// An AI tab works while nobody sees it: never shown, shown and then put
    /// away, in a minimised window; and it comes back after its renderer or
    /// its browser process dies.
    #[test]
    #[ignore = "drives a real WebView2; run with --ignored on a Windows desktop"]
    fn live_ai_tab_in_the_background() {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .unwrap();
        let site = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", site.local_addr().unwrap().port());
        let _site_requests = serve(site, respond_as_busy_site);
        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_endpoint = format!("http://127.0.0.1:{}", proxy.local_addr().unwrap().port());
        let _proxy_requests = serve(proxy, respond_as_proxy);
        let host = create_host_window();

        // A command reached an AI tab that was never painted.
        let mut ai = Surface::never_shown(host, ai_options("background", &proxy_endpoint));
        ai.wait_for("the AI surface", |observed| *observed == Observed::Ready);
        ai.load(&format!("{base}/"));
        assert_alive(&mut ai, "never shown", (1280, 800));
        let parked = ai.webview.inner.state.borrow().applied_bounds.unwrap();
        assert!(
            parked.right <= 0 && parked.bottom <= 0,
            "the parked page reaches into the window: {parked:?}"
        );

        // Painted once, then put away (another tab, a closed panel): it keeps
        // the size it had on screen, and gives the keyboard back.
        let deferred = ai.webview.sync_viewport(on_screen_viewport(), false);
        assert!(!deferred);
        pump_for(Duration::from_millis(300));
        assert_alive(&mut ai, "on screen", (800, 600));
        ai.webview.focus_page();
        pump_until(
            "the page takes the keyboard",
            Duration::from_secs(5),
            || ai.webview.inner.state.borrow().page_has_focus.then_some(()),
        );
        ai.webview.hide("the test put the tab away");
        assert_eq!(
            unsafe { GetFocus() },
            host,
            "the parked page kept the keyboard"
        );
        assert_alive(&mut ai, "put away", (800, 600));
        ai.input(NativeInput::MouseClick {
            x: 120.,
            y: 450.,
            button: MouseButton::Left,
            click_count: 1,
            modifiers: Vec::new(),
        });
        assert_eq!(ai.script("window.__clicked"), "true");
        println!("trusted click on a parked page: ok");

        // Another window covers the editor's window entirely: Chromium's
        // occlusion tracking would call both pages hidden.
        let deferred = ai.webview.sync_viewport(on_screen_viewport(), false);
        assert!(!deferred);
        let cover = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                w!("BrowserPanelLiveTestHost"),
                w!("browser panel live test cover"),
                WS_POPUP | WS_VISIBLE,
                -100,
                -100,
                1100,
                900,
                None,
                None,
                None,
                None,
            )
        }
        .unwrap();
        pump_for(Duration::from_secs(3));
        assert_alive(&mut ai, "covered while shown", (800, 600));
        ai.webview.hide("the test put the tab away");
        assert_alive(&mut ai, "covered and put away", (800, 600));
        unsafe { DestroyWindow(cover) }.unwrap();

        // The whole window minimised, with the page on screen and parked.
        let deferred = ai.webview.sync_viewport(on_screen_viewport(), false);
        assert!(!deferred);
        let _was_visible = unsafe { ShowWindow(host, SW_MINIMIZE) };
        pump_for(Duration::from_millis(500));
        assert!(unsafe { IsIconic(host) }.as_bool());
        assert_alive(&mut ai, "minimised while shown", (800, 600));
        ai.webview.hide("the test put the tab away");
        assert_alive(&mut ai, "minimised and put away", (800, 600));
        let _was_visible = unsafe { ShowWindow(host, SW_RESTORE) };
        pump_for(Duration::from_millis(300));

        // The renderer dies: the surface reports it, owes nothing, and a new
        // one takes its place.
        ai.forget();
        let mut after_crash = None;
        // `Page.crash` now and then goes unanswered right after the window was
        // restored; asked again, it lands.
        for attempt in 1..=3 {
            call_devtools_methods(
                ai.webview.inner.webview().unwrap(),
                VecDeque::from([("Page.crash", serde_json::json!({}))]),
                Box::new(|result| println!("Page.crash answered: {result:?}")),
            );
            after_crash.get_or_insert_with(|| ai.webview.execute_script("1"));
            if ai.saw(Duration::from_secs(8), |observed| {
                matches!(observed, Observed::Crashed(_))
            }) {
                break;
            }
            assert!(attempt < 3, "the renderer never crashed");
            println!("Page.crash attempt {attempt} did nothing; asking again");
        }
        let mut after_crash = after_crash.unwrap();
        println!("renderer crash reported: {:?}", ai.observed);
        let answer = pump_until(
            "the script sent with the crash",
            Duration::from_secs(20),
            || after_crash.try_recv().expect("the answer was dropped"),
        );
        println!("the script sent with the crash: {answer:?}");
        let later = wait_error("a script after the crash", ai.webview.execute_script("2"));
        assert!(later.contains("browser stopped"), "{later}");
        drop(ai);
        let mut ai = Surface::never_shown(host, ai_options("background", &proxy_endpoint));
        ai.wait_for("the replaced surface", |observed| {
            *observed == Observed::Ready
        });
        ai.load(&format!("{base}/"));
        assert_eq!(ai.script("1+1"), "2");
        println!("renderer crash recovery: ok");

        // The browser process dies: the environment goes with it, and the next
        // surface starts a new one.
        let old_process = browser_process_id(&ai);
        let old_environment = ai.webview.inner.state.borrow().environment.clone();
        assert!(old_environment.is_some());
        ai.forget();
        #[allow(clippy::disallowed_methods, reason = "a live test on its own thread")]
        let killed = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &old_process.to_string()])
            .output()
            .unwrap();
        println!(
            "taskkill: {}",
            String::from_utf8_lossy(&killed.stdout).trim()
        );
        ai.wait_for("the browser crash", |observed| {
            matches!(observed, Observed::Crashed(_))
        });
        println!("browser crash reported: {:?}", ai.observed);
        drop(ai);
        let mut ai = Surface::never_shown(host, ai_options("background", &proxy_endpoint));
        ai.wait_for("the surface in a new browser", |observed| {
            *observed == Observed::Ready
        });
        assert_ne!(browser_process_id(&ai), old_process);
        assert_ne!(
            ai.webview.inner.state.borrow().environment,
            old_environment,
            "the surface was created in the environment whose browser went away"
        );
        ai.load(&format!("{base}/"));
        assert_eq!(ai.script("1+1"), "2");
        assert_alive(&mut ai, "after the browser restarted", (1280, 800));
        println!("browser crash recovery: ok");

        drop(ai);
        pump_for(Duration::from_millis(500));
        unsafe { DestroyWindow(host) }.unwrap();
    }

    fn wait_error<T: std::fmt::Debug>(
        what: &str,
        receiver: oneshot::Receiver<Result<T>>,
    ) -> String {
        let mut receiver = receiver;
        let answer = pump_until(what, Duration::from_secs(20), || {
            receiver.try_recv().expect("the answer was dropped")
        });
        match answer {
            Ok(value) => panic!("{what} succeeded: {value:?}"),
            Err(error) => format!("{error:#}"),
        }
    }

    /// Nothing an AI tab's page does sends UDP past the proxy: WebTransport
    /// (HTTP/3, QUIC) to an address literal, which no name resolution stops,
    /// and fetches of sites that would offer HTTP/3. Read from Chromium's own
    /// net log of the AI profile's browser.
    #[test]
    #[ignore = "drives a real WebView2; run with --ignored on a Windows desktop"]
    fn live_ai_tab_network_stays_proxied() {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .unwrap();
        let net_log = scratch_folder("netlog").join("netlog.json");
        std::fs::remove_file(&net_log).ok();
        TEST_BROWSER_ARGUMENTS.with(|arguments| {
            *arguments.borrow_mut() = vec![
                format!("--log-net-log={}", net_log.display()),
                "--net-log-capture-mode=Everything".to_string(),
            ];
        });
        let site = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", site.local_addr().unwrap().port());
        let _site_requests = serve(site, respond_as_busy_site);
        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_endpoint = format!("http://127.0.0.1:{}", proxy.local_addr().unwrap().port());
        let proxy_requests = serve(proxy, respond_as_proxy);
        let host = create_host_window();

        let mut ai = Surface::new(host, ai_options("netlog", &proxy_endpoint));
        ai.wait_for("the AI surface", |observed| *observed == Observed::Ready);
        ai.load(&format!("{base}/"));
        ai.script(
            "(() => { window.__outcomes = {}; \
             const note = (name) => (value) => { window.__outcomes[name] = String(value && (value.message || value)); }; \
             try { const transport = new WebTransport('https://1.1.1.1:443/'); \
               transport.ready.then(note('webtransport ready'), note('webtransport')); } \
             catch (error) { note('webtransport')(error); } \
             try { const transport = new WebTransport('https://[2606:4700:4700::1111]:443/'); \
               transport.ready.then(note('webtransport6 ready'), note('webtransport6')); } \
             catch (error) { note('webtransport6')(error); } \
             fetch('https://1.1.1.1/').then(note('fetch ip'), note('fetch ip')); \
             fetch('https://cloudflare-quic.com/').then(note('fetch h3'), note('fetch h3')); \
             return true; })()",
        );
        pump_until("the page's attempts", Duration::from_secs(30), || {
            let outcomes = ai.script("Object.keys(window.__outcomes).length");
            (outcomes == "4").then_some(())
        });
        println!(
            "page outcomes: {}",
            ai.script("JSON.stringify(window.__outcomes)")
        );
        println!("proxy saw: {:?}", proxy_requests.lock().unwrap());
        drop(ai);
        // The net log is complete once the browser process has shut down.
        pump_for(Duration::from_secs(3));
        unsafe { DestroyWindow(host) }.unwrap();

        let log = std::fs::read_to_string(&net_log).expect("the net log was written");
        let leaks = udp_leaks(&log);
        println!("UDP sockets to other hosts: {leaks:?}");
        assert!(leaks.is_empty(), "UDP left the machine: {leaks:?}");
    }

    /// The UDP datagrams in a Chromium net log that went anywhere but
    /// loopback, and any QUIC session. A `UDP_CONNECT` alone sends nothing:
    /// Chromium's IPv6 reachability check connects a UDP socket only to ask
    /// the system for a route, so those are printed but not counted. Reads
    /// event by event, so that a log cut short by the browser's exit still
    /// counts.
    fn udp_leaks(log: &str) -> Vec<String> {
        let constants_start = log
            .find("\"logEventTypes\"")
            .expect("the log has its constants");
        let types_start = log[constants_start..].find('{').unwrap() + constants_start;
        let types_end = log[types_start..].find('}').unwrap() + types_start;
        let types: std::collections::HashMap<String, u64> =
            serde_json::from_str(&log[types_start..=types_end]).unwrap();
        let wanted = [
            "UDP_CONNECT",
            "UDP_BYTES_SENT",
            "QUIC_SESSION_POOL_JOB",
            "QUIC_SESSION",
        ]
        .iter()
        .filter_map(|name| types.get(*name).map(|id| (*id, *name)))
        .collect::<std::collections::HashMap<_, _>>();
        let mut leaks = Vec::new();
        for line in log.lines() {
            let line = line.trim().trim_end_matches(',');
            if !line.starts_with("{\"params\"") && !line.starts_with("{\"phase\"") {
                continue;
            }
            let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let Some(name) = event
                .get("type")
                .and_then(serde_json::Value::as_u64)
                .and_then(|id| wanted.get(&id))
            else {
                continue;
            };
            let address = event
                .pointer("/params/address")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            let loopback = address.starts_with("127.") || address.starts_with("[::1]");
            if *name == "UDP_CONNECT" {
                println!("UDP socket connected (nothing sent by that): {address}");
            } else if name.starts_with("QUIC") || !loopback {
                leaks.push(format!(
                    "{name} {address} {}",
                    event.get("params").unwrap_or(&serde_json::Value::Null)
                ));
            }
        }
        leaks
    }

    const POPUP_PAGE: &str = r#"<!doctype html><html><head><title>popups</title></head>
<body style="margin:0">
<select style="position:absolute;left:20px;top:20px;width:200px;height:30px">
<option>one</option><option>two</option><option>three</option><option>four</option>
</select>
</body></html>"#;

    fn respond_as_popup_site(request_line: &str) -> Vec<u8> {
        let path = request_line.split(' ').nth(1).unwrap_or("/");
        match path {
            "/" => response(
                "200 OK",
                &["Content-Type: text/html"],
                POPUP_PAGE.as_bytes(),
            ),
            _ => response("404 Not Found", &[], b""),
        }
    }

    unsafe extern "system" fn collect_process_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let found = unsafe { &mut *(lparam.0 as *mut (u32, Vec<(RECT, bool)>)) };
        let mut process_id = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
        if process_id == found.0 && unsafe { IsWindowVisible(hwnd) }.as_bool() {
            let mut rect = RECT::default();
            if unsafe { GetWindowRect(hwnd, &mut rect) }.is_ok() {
                let monitor = unsafe { MonitorFromRect(&rect, MONITOR_DEFAULTTONULL) };
                found.1.push((rect, !monitor.is_invalid()));
            }
        }
        true.into()
    }

    /// The visible top-level windows of a process, and whether each reaches a
    /// monitor.
    fn visible_windows_of(process_id: u32) -> Vec<(RECT, bool)> {
        let mut found = (process_id, Vec::new());
        unsafe {
            EnumWindows(
                Some(collect_process_window),
                LPARAM(&mut found as *mut _ as isize),
            )
        }
        .unwrap();
        found.1
    }

    /// What a parked page opens on its own (a `<select>`'s list, a context
    /// menu) never shows up on a monitor, wherever the editor's window is.
    #[test]
    #[ignore = "drives a real WebView2; run with --ignored on a Windows desktop"]
    fn live_ai_tab_in_the_background_keeps_popups_off_screen() {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .unwrap();
        let site = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", site.local_addr().unwrap().port());
        let _site_requests = serve(site, respond_as_popup_site);
        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_endpoint = format!("http://127.0.0.1:{}", proxy.local_addr().unwrap().port());
        let _proxy_requests = serve(proxy, respond_as_proxy);
        let host = create_host_window();
        // Far enough from the primary monitor's top-left corner that a page
        // parked just outside the window would open its lists on that monitor.
        unsafe {
            SetWindowPos(
                host,
                None,
                1400,
                900,
                800,
                600,
                SWP_NOACTIVATE | SWP_NOZORDER,
            )
        }
        .unwrap();

        let mut ai = Surface::never_shown(host, ai_options("popups", &proxy_endpoint));
        ai.wait_for("the AI surface", |observed| *observed == Observed::Ready);
        ai.load(&format!("{base}/"));
        let browser = browser_process_id(&ai);
        let on_a_monitor = |what: &str| {
            let windows = visible_windows_of(browser);
            println!("{what}: {windows:?}");
            assert!(
                windows.iter().all(|(_, on_a_monitor)| !on_a_monitor),
                "{what}: a window of the parked page is on a monitor: {windows:?}"
            );
            windows.len()
        };
        assert_eq!(on_a_monitor("parked"), 0);

        let click = |x: f64, y: f64, button: MouseButton| NativeInput::MouseClick {
            x,
            y,
            button,
            click_count: 1,
            modifiers: Vec::new(),
        };
        ai.input(click(60., 35., MouseButton::Left));
        pump_for(Duration::from_millis(1500));
        assert!(
            on_a_monitor("the list of a <select>") > 0,
            "the <select> opened no list, so nothing was checked"
        );
        ai.input(NativeInput::Key {
            key: "Escape".to_string(),
            code: "Escape".to_string(),
            modifiers: Vec::new(),
        });
        pump_for(Duration::from_millis(500));
        ai.input(click(300., 300., MouseButton::Right));
        pump_for(Duration::from_millis(1500));
        assert_eq!(
            on_a_monitor("a right click"),
            0,
            "a context menu opened for the parked page"
        );

        drop(ai);
        pump_for(Duration::from_millis(500));
        unsafe { DestroyWindow(host) }.unwrap();
    }
}
