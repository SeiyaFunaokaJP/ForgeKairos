//! The C functions of GTK 3, WebKitGTK 4.1 and their dependencies that
//! [`super`] calls, resolved at runtime with `dlopen` so that the editor binary
//! never links against them: a machine without WebKitGTK still starts, and the
//! browser panel says what to install instead (ADR-F89).
//!
//! Every GObject type is passed around as an untyped pointer; the wrappers in
//! [`super`] are the only place that knows which pointer is which.

use std::ffi::{c_char, c_double, c_int, c_uint, c_ulong, c_void};

use libloading::Library;

pub(crate) type Gboolean = c_int;
pub(crate) type Gtype = usize;
pub(crate) type Gpointer = *mut c_void;
/// `GCallback`: every signal handler is cast to this before it is connected.
pub(crate) type Gcallback = unsafe extern "C" fn();
pub(crate) type GclosureNotify = unsafe extern "C" fn(data: Gpointer, closure: Gpointer);
pub(crate) type GasyncReadyCallback =
    unsafe extern "C" fn(source: Gpointer, result: Gpointer, user_data: Gpointer);
pub(crate) type CairoWriteFunc =
    unsafe extern "C" fn(closure: Gpointer, data: *const u8, length: c_uint) -> c_int;
/// An X11 `Window` (XID).
pub(crate) type XWindow = c_ulong;

pub(crate) const FALSE: Gboolean = 0;
pub(crate) const TRUE: Gboolean = 1;

pub(crate) const GTK_WINDOW_TOPLEVEL: c_int = 0;
pub(crate) const GTK_WINDOW_POPUP: c_int = 1;

pub(crate) const WEBKIT_POLICY_DECISION_TYPE_NAVIGATION_ACTION: c_int = 0;
pub(crate) const WEBKIT_POLICY_DECISION_TYPE_NEW_WINDOW_ACTION: c_int = 1;
pub(crate) const WEBKIT_POLICY_DECISION_TYPE_RESPONSE: c_int = 2;

pub(crate) const WEBKIT_LOAD_STARTED: c_int = 0;
pub(crate) const WEBKIT_LOAD_COMMITTED: c_int = 2;
pub(crate) const WEBKIT_LOAD_FINISHED: c_int = 3;

pub(crate) const WEBKIT_NETWORK_PROXY_MODE_CUSTOM: c_int = 2;
pub(crate) const WEBKIT_COOKIE_PERSISTENT_STORAGE_SQLITE: c_int = 1;
pub(crate) const WEBKIT_USER_CONTENT_INJECT_ALL_FRAMES: c_int = 0;
pub(crate) const WEBKIT_USER_SCRIPT_INJECT_AT_DOCUMENT_START: c_int = 0;
pub(crate) const WEBKIT_SNAPSHOT_REGION_VISIBLE: c_int = 0;
pub(crate) const WEBKIT_SNAPSHOT_OPTIONS_NONE: c_int = 0;

pub(crate) const CAIRO_STATUS_SUCCESS: c_int = 0;
pub(crate) const CAIRO_STATUS_WRITE_ERROR: c_int = 11;

/// `RevertToParent` of `XSetInputFocus`.
pub(crate) const X_REVERT_TO_PARENT: c_int = 2;
/// `CurrentTime` of the X protocol.
pub(crate) const X_CURRENT_TIME: c_ulong = 0;

pub(crate) const GDK_KEY_ESCAPE: c_uint = 0xff1b;

#[repr(C)]
pub(crate) struct Gerror {
    pub(crate) domain: u32,
    pub(crate) code: c_int,
    pub(crate) message: *mut c_char,
}

#[repr(C)]
pub(crate) struct GdkRgba {
    pub(crate) red: c_double,
    pub(crate) green: c_double,
    pub(crate) blue: c_double,
    pub(crate) alpha: c_double,
}

#[repr(C)]
pub(crate) struct CairoRectangleInt {
    pub(crate) x: c_int,
    pub(crate) y: c_int,
    pub(crate) width: c_int,
    pub(crate) height: c_int,
}

/// The leading fields of `GdkEventKey`, as far as `keyval`; GDK only ever hands
/// out pointers to the full struct, so reading this prefix is sound.
#[repr(C)]
pub(crate) struct GdkEventKeyPrefix {
    pub(crate) event_type: c_int,
    pub(crate) window: Gpointer,
    pub(crate) send_event: i8,
    pub(crate) time: u32,
    pub(crate) state: c_uint,
    pub(crate) keyval: c_uint,
}

/// The libraries in the order they are opened. WebKitGTK first: it pulls in
/// most of the others, and it is the one whose absence the user has to fix.
const LIBRARIES: &[&str] = &[
    "libwebkit2gtk-4.1.so.0",
    "libjavascriptcoregtk-4.1.so.0",
    "libgtk-3.so.0",
    "libgdk-3.so.0",
    "libgobject-2.0.so.0",
    "libglib-2.0.so.0",
    "libcairo.so.2",
];

/// Only needed to embed the page into an X11 window; a GTK built without X11
/// still runs the detached window.
const X11_LIBRARY: &str = "libX11.so.6";

macro_rules! functions {
    (
        required { $($name:ident: $type:ty,)* }
        optional { $($optional_name:ident: $optional_type:ty,)* }
    ) => {
        /// Function pointers into the loaded libraries. The libraries are kept
        /// open for the life of the process (see [`Functions::load`]), so the
        /// pointers never dangle.
        #[allow(non_snake_case)]
        pub(crate) struct Functions {
            $(pub(crate) $name: $type,)*
            $(pub(crate) $optional_name: Option<$optional_type>,)*
            _libraries: Vec<Library>,
        }

        impl Functions {
            /// Opens the libraries and resolves every function. A missing
            /// library or required function is reported by name.
            ///
            /// The libraries are never closed: GTK and WebKit register types,
            /// threads and atexit handlers that must outlive any unload.
            pub(crate) fn load() -> Result<Self, String> {
                let mut libraries = Vec::new();
                for name in LIBRARIES {
                    // SAFETY: these are the system's GTK / WebKitGTK libraries,
                    // whose initialisers are safe to run in any process.
                    let library = unsafe { Library::new(name) }
                        .map_err(|error| format!("could not load {name}: {error}"))?;
                    libraries.push(library);
                }
                // SAFETY: as above.
                match unsafe { Library::new(X11_LIBRARY) } {
                    Ok(library) => libraries.push(library),
                    Err(error) => log::info!(
                        "browser (WebKitGTK): {X11_LIBRARY} is unavailable, so pages cannot be embedded into X11 windows: {error}"
                    ),
                }
                Ok(Self {
                    $($name: unsafe { find::<$type>(&libraries, stringify!($name)) }
                        .ok_or_else(|| format!("WebKitGTK is missing the function {}", stringify!($name)))?,)*
                    $($optional_name: unsafe { find::<$optional_type>(&libraries, stringify!($optional_name)) },)*
                    _libraries: libraries,
                })
            }
        }
    };
}

/// # Safety
///
/// `T` must be the function pointer type that matches the C declaration of
/// `name`.
unsafe fn find<T: Copy>(libraries: &[Library], name: &str) -> Option<T> {
    libraries.iter().find_map(|library| {
        unsafe { library.get::<T>(name.as_bytes()) }
            .ok()
            .map(|symbol| *symbol)
    })
}

functions! {
    required {
        // GLib
        g_main_context_iteration: unsafe extern "C" fn(Gpointer, Gboolean) -> Gboolean,
        g_free: unsafe extern "C" fn(Gpointer),
        g_error_free: unsafe extern "C" fn(*mut Gerror),

        // GObject
        g_signal_connect_data: unsafe extern "C" fn(Gpointer, *const c_char, Gcallback, Gpointer, Option<GclosureNotify>, c_int) -> c_ulong,
        g_signal_handler_disconnect: unsafe extern "C" fn(Gpointer, c_ulong),
        g_signal_handler_is_connected: unsafe extern "C" fn(Gpointer, c_ulong) -> Gboolean,
        g_object_ref: unsafe extern "C" fn(Gpointer) -> Gpointer,
        g_object_unref: unsafe extern "C" fn(Gpointer),
        g_object_ref_sink: unsafe extern "C" fn(Gpointer) -> Gpointer,
        g_object_new: unsafe extern "C" fn(Gtype, *const c_char, ...) -> Gpointer,
        g_object_set: unsafe extern "C" fn(Gpointer, *const c_char, ...),
        g_object_class_find_property: unsafe extern "C" fn(Gpointer, *const c_char) -> Gpointer,
        g_type_check_instance_is_a: unsafe extern "C" fn(Gpointer, Gtype) -> Gboolean,

        // GDK
        gdk_set_allowed_backends: unsafe extern "C" fn(*const c_char),
        gdk_display_get_default: unsafe extern "C" fn() -> Gpointer,
        gdk_window_get_scale_factor: unsafe extern "C" fn(Gpointer) -> c_int,
        gdk_window_shape_combine_region: unsafe extern "C" fn(Gpointer, Gpointer, c_int, c_int),
        gdk_window_input_shape_combine_region: unsafe extern "C" fn(Gpointer, Gpointer, c_int, c_int),

        // GTK
        gtk_disable_setlocale: unsafe extern "C" fn(),
        gtk_init_check: unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char) -> Gboolean,
        gtk_settings_get_default: unsafe extern "C" fn() -> Gpointer,
        gtk_window_new: unsafe extern "C" fn(c_int) -> Gpointer,
        gtk_window_set_title: unsafe extern "C" fn(Gpointer, *const c_char),
        gtk_window_set_default_size: unsafe extern "C" fn(Gpointer, c_int, c_int),
        gtk_window_move: unsafe extern "C" fn(Gpointer, c_int, c_int),
        gtk_window_resize: unsafe extern "C" fn(Gpointer, c_int, c_int),
        gtk_window_present: unsafe extern "C" fn(Gpointer),
        gtk_window_set_focus_on_map: unsafe extern "C" fn(Gpointer, Gboolean),
        gtk_container_add: unsafe extern "C" fn(Gpointer, Gpointer),
        gtk_widget_show: unsafe extern "C" fn(Gpointer),
        gtk_widget_hide: unsafe extern "C" fn(Gpointer),
        gtk_widget_realize: unsafe extern "C" fn(Gpointer),
        gtk_widget_destroy: unsafe extern "C" fn(Gpointer),
        gtk_widget_get_window: unsafe extern "C" fn(Gpointer) -> Gpointer,
        gtk_widget_grab_focus: unsafe extern "C" fn(Gpointer),
        gtk_widget_set_size_request: unsafe extern "C" fn(Gpointer, c_int, c_int),

        // WebKitGTK
        webkit_web_view_get_type: unsafe extern "C" fn() -> Gtype,
        webkit_settings_new: unsafe extern "C" fn() -> Gpointer,
        webkit_user_content_manager_new: unsafe extern "C" fn() -> Gpointer,
        webkit_user_script_new: unsafe extern "C" fn(*const c_char, c_int, c_int, *const *const c_char, *const *const c_char) -> Gpointer,
        webkit_user_script_unref: unsafe extern "C" fn(Gpointer),
        webkit_user_content_manager_add_script: unsafe extern "C" fn(Gpointer, Gpointer),
        webkit_website_data_manager_new: unsafe extern "C" fn(*const c_char, ...) -> Gpointer,
        webkit_website_data_manager_get_cookie_manager: unsafe extern "C" fn(Gpointer) -> Gpointer,
        webkit_cookie_manager_set_persistent_storage: unsafe extern "C" fn(Gpointer, *const c_char, c_int),
        webkit_web_context_new_with_website_data_manager: unsafe extern "C" fn(Gpointer) -> Gpointer,
        webkit_web_view_load_uri: unsafe extern "C" fn(Gpointer, *const c_char),
        webkit_web_view_go_back: unsafe extern "C" fn(Gpointer),
        webkit_web_view_go_forward: unsafe extern "C" fn(Gpointer),
        webkit_web_view_reload: unsafe extern "C" fn(Gpointer),
        webkit_web_view_get_uri: unsafe extern "C" fn(Gpointer) -> *const c_char,
        webkit_web_view_get_title: unsafe extern "C" fn(Gpointer) -> *const c_char,
        webkit_web_view_can_go_back: unsafe extern "C" fn(Gpointer) -> Gboolean,
        webkit_web_view_can_go_forward: unsafe extern "C" fn(Gpointer) -> Gboolean,
        webkit_web_view_set_background_color: unsafe extern "C" fn(Gpointer, *const GdkRgba),
        webkit_web_view_set_zoom_level: unsafe extern "C" fn(Gpointer, c_double),
        webkit_web_view_get_back_forward_list: unsafe extern "C" fn(Gpointer) -> Gpointer,
        webkit_web_view_get_inspector: unsafe extern "C" fn(Gpointer) -> Gpointer,
        webkit_web_inspector_show: unsafe extern "C" fn(Gpointer),
        webkit_web_view_get_snapshot: unsafe extern "C" fn(Gpointer, c_int, c_int, Gpointer, GasyncReadyCallback, Gpointer),
        webkit_web_view_get_snapshot_finish: unsafe extern "C" fn(Gpointer, Gpointer, *mut *mut Gerror) -> Gpointer,
        webkit_policy_decision_ignore: unsafe extern "C" fn(Gpointer),
        webkit_navigation_policy_decision_get_navigation_action: unsafe extern "C" fn(Gpointer) -> Gpointer,
        webkit_navigation_action_get_request: unsafe extern "C" fn(Gpointer) -> Gpointer,
        webkit_uri_request_get_uri: unsafe extern "C" fn(Gpointer) -> *const c_char,
        webkit_response_policy_decision_get_response: unsafe extern "C" fn(Gpointer) -> Gpointer,
        webkit_response_policy_decision_is_mime_type_supported: unsafe extern "C" fn(Gpointer) -> Gboolean,
        webkit_uri_response_get_uri: unsafe extern "C" fn(Gpointer) -> *const c_char,
        webkit_download_get_web_view: unsafe extern "C" fn(Gpointer) -> Gpointer,
        webkit_download_get_request: unsafe extern "C" fn(Gpointer) -> Gpointer,
        webkit_download_cancel: unsafe extern "C" fn(Gpointer),
        webkit_permission_request_deny: unsafe extern "C" fn(Gpointer),
        webkit_file_chooser_request_cancel: unsafe extern "C" fn(Gpointer),
        webkit_script_dialog_get_dialog_type: unsafe extern "C" fn(Gpointer) -> c_int,
        webkit_script_dialog_confirm_set_confirmed: unsafe extern "C" fn(Gpointer, Gboolean),
        webkit_authentication_request_cancel: unsafe extern "C" fn(Gpointer),

        // JavaScriptCore
        jsc_value_to_json: unsafe extern "C" fn(Gpointer, c_uint) -> *mut c_char,
        jsc_value_is_undefined: unsafe extern "C" fn(Gpointer) -> Gboolean,

        // cairo
        cairo_surface_write_to_png_stream: unsafe extern "C" fn(Gpointer, CairoWriteFunc, Gpointer) -> c_int,
        cairo_surface_destroy: unsafe extern "C" fn(Gpointer),
        cairo_region_create_rectangle: unsafe extern "C" fn(*const CairoRectangleInt) -> Gpointer,
        cairo_region_subtract_rectangle: unsafe extern "C" fn(Gpointer, *const CairoRectangleInt) -> c_int,
        cairo_region_create: unsafe extern "C" fn() -> Gpointer,
        cairo_region_destroy: unsafe extern "C" fn(Gpointer),
    }
    optional {
        // GDK's X11 backend; absent from a GTK built for Wayland only.
        gdk_x11_display_get_type: unsafe extern "C" fn() -> Gtype,
        gdk_x11_display_get_xdisplay: unsafe extern "C" fn(Gpointer) -> Gpointer,
        gdk_x11_window_get_xid: unsafe extern "C" fn(Gpointer) -> XWindow,
        gdk_x11_display_error_trap_push: unsafe extern "C" fn(Gpointer),
        gdk_x11_display_error_trap_pop_ignored: unsafe extern "C" fn(Gpointer),

        // Xlib
        XReparentWindow: unsafe extern "C" fn(Gpointer, XWindow, XWindow, c_int, c_int) -> c_int,
        XSetInputFocus: unsafe extern "C" fn(Gpointer, XWindow, c_int, c_ulong) -> c_int,
        XFlush: unsafe extern "C" fn(Gpointer) -> c_int,

        // WebKitGTK 2.32
        webkit_network_proxy_settings_new: unsafe extern "C" fn(*const c_char, *const *const c_char) -> Gpointer,
        webkit_network_proxy_settings_free: unsafe extern "C" fn(Gpointer),
        webkit_website_data_manager_set_network_proxy_settings: unsafe extern "C" fn(Gpointer, c_int, Gpointer),

        // WebKitGTK 2.40 replaced `run_javascript` with `evaluate_javascript`;
        // either pair is enough.
        webkit_web_view_evaluate_javascript: unsafe extern "C" fn(Gpointer, *const c_char, isize, *const c_char, *const c_char, Gpointer, GasyncReadyCallback, Gpointer),
        webkit_web_view_evaluate_javascript_finish: unsafe extern "C" fn(Gpointer, Gpointer, *mut *mut Gerror) -> Gpointer,
        webkit_web_view_run_javascript: unsafe extern "C" fn(Gpointer, *const c_char, Gpointer, GasyncReadyCallback, Gpointer),
        webkit_web_view_run_javascript_finish: unsafe extern "C" fn(Gpointer, Gpointer, *mut *mut Gerror) -> Gpointer,
        webkit_javascript_result_get_js_value: unsafe extern "C" fn(Gpointer) -> Gpointer,
        webkit_javascript_result_unref: unsafe extern "C" fn(Gpointer),
    }
}
