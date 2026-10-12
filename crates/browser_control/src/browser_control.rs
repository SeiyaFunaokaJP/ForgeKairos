//! The contract between the built-in browser (`browser_panel`) and the AI tools that drive it
//! (ADR-F89). The IDE MCP server (`ide_server`) and the built-in agent (`agent`) both expose the
//! same browser tools; their definitions, schemas and argument parsing live here so that the two
//! cannot drift apart, and the browser registers itself through [`GlobalBrowserControl`] so that
//! neither consumer has to depend on `workspace` or the webview backends.

use std::{fmt, sync::Arc};

use anyhow::{Result, anyhow, bail};
use gpui::{App, Entity, Global, Task};
use project::Project;
use serde_json::{Map, Value, json};

pub const DEFAULT_MAX_CHARS: usize = 20_000;
pub const MAX_CHARS_LIMIT: usize = 100_000;
pub const DEFAULT_FIND_LIMIT: usize = 20;
pub const MAX_FIND_LIMIT: usize = 100;
pub const DEFAULT_LOG_LIMIT: usize = 50;
pub const MAX_LOG_LIMIT: usize = 500;
pub const DEFAULT_TIMEOUT_MS: u64 = 15_000;
pub const MAX_TIMEOUT_MS: u64 = 120_000;
pub const MAX_KEY_REPEAT: u32 = 100;
pub const DEFAULT_SCROLL_AMOUNT: u32 = 3;
pub const MAX_SCROLL_AMOUNT: u32 = 20;
pub const MAX_CLICK_COUNT: u8 = 3;
pub const MAX_EXPRESSION_CHARS: usize = 100_000;
/// Screenshots are downscaled so that their longer edge is at most this many pixels.
pub const MAX_SCREENSHOT_EDGE_PX: u32 = 1568;

pub const TOOL_NAMES: [&str; 19] = [
    "browser_tabs",
    "browser_open_tab",
    "browser_close_tab",
    "browser_select_tab",
    "browser_navigate",
    "browser_page_text",
    "browser_snapshot",
    "browser_find",
    "browser_click",
    "browser_hover",
    "browser_type",
    "browser_press_key",
    "browser_scroll",
    "browser_set_value",
    "browser_screenshot",
    "browser_console",
    "browser_network",
    "browser_eval",
    "browser_wait_for",
];

/// An AI tab, as the tools name it: short ("1", "2", ...) and unique within a workspace.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TabId(pub String);

impl fmt::Display for TabId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabInfo {
    pub id: TabId,
    pub title: String,
    pub url: String,
    /// Whether this is the workspace's current AI tab (the one used when a tool omits `tab`).
    pub active: bool,
    pub loading: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ElementTarget {
    /// A ref handed out by `browser_snapshot` or `browser_find`.
    Ref(String),
    /// CSS pixels relative to the top-left corner of the viewport.
    Point { x: f64, y: f64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NavigateTarget {
    Url(String),
    Back,
    Forward,
    Reload,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyModifier {
    Shift,
    Control,
    Alt,
    Meta,
}

/// One browser tool call, validated and with every default and limit already applied.
/// `tab: None` means the workspace's current AI tab.
#[derive(Clone, Debug, PartialEq)]
pub enum BrowserCommand {
    ListTabs,
    OpenTab {
        url: Option<String>,
    },
    CloseTab {
        tab: TabId,
    },
    SelectTab {
        tab: TabId,
    },
    Navigate {
        tab: Option<TabId>,
        target: NavigateTarget,
        timeout_ms: u64,
    },
    PageText {
        tab: Option<TabId>,
        max_chars: usize,
    },
    Snapshot {
        tab: Option<TabId>,
        interactive_only: bool,
        max_chars: usize,
    },
    Find {
        tab: Option<TabId>,
        query: String,
        limit: usize,
    },
    Click {
        tab: Option<TabId>,
        target: ElementTarget,
        button: MouseButton,
        click_count: u8,
        modifiers: Vec<KeyModifier>,
    },
    Hover {
        tab: Option<TabId>,
        target: ElementTarget,
    },
    Type {
        tab: Option<TabId>,
        element: Option<String>,
        text: String,
        submit: bool,
    },
    /// `keys` is a space-separated sequence of presses such as `"ctrl+a Backspace"`.
    PressKey {
        tab: Option<TabId>,
        keys: String,
        repeat: u32,
    },
    Scroll {
        tab: Option<TabId>,
        target: Option<ElementTarget>,
        direction: ScrollDirection,
        amount: u32,
    },
    /// `value` is a string, number, boolean or an array of strings (multi-selects).
    SetValue {
        tab: Option<TabId>,
        element: String,
        value: Value,
    },
    Screenshot {
        tab: Option<TabId>,
    },
    Console {
        tab: Option<TabId>,
        only_errors: bool,
        pattern: Option<String>,
        limit: usize,
        clear: bool,
    },
    Network {
        tab: Option<TabId>,
        url_pattern: Option<String>,
        limit: usize,
        clear: bool,
        request_id: Option<String>,
    },
    Eval {
        tab: Option<TabId>,
        expression: String,
    },
    /// At least one of `text` and `selector` is set; all that are set must hold at once.
    WaitFor {
        tab: Option<TabId>,
        text: Option<String>,
        selector: Option<String>,
        timeout_ms: u64,
    },
}

impl BrowserCommand {
    /// The tab the command is aimed at; `None` for the current AI tab and for the commands that
    /// are not about one tab.
    pub fn tab(&self) -> Option<&TabId> {
        match self {
            Self::ListTabs | Self::OpenTab { .. } => None,
            Self::CloseTab { tab } | Self::SelectTab { tab } => Some(tab),
            Self::Navigate { tab, .. }
            | Self::PageText { tab, .. }
            | Self::Snapshot { tab, .. }
            | Self::Find { tab, .. }
            | Self::Click { tab, .. }
            | Self::Hover { tab, .. }
            | Self::Type { tab, .. }
            | Self::PressKey { tab, .. }
            | Self::Scroll { tab, .. }
            | Self::SetValue { tab, .. }
            | Self::Screenshot { tab }
            | Self::Console { tab, .. }
            | Self::Network { tab, .. }
            | Self::Eval { tab, .. }
            | Self::WaitFor { tab, .. } => tab.as_ref(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BrowserOutput {
    pub text: String,
    pub image_png: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolDefinition {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

/// Implemented by `browser_panel`.
pub trait BrowserControl: 'static + Send + Sync {
    /// Whether the user turned AI control of the browser on (`browser.ai_control`). The tools are
    /// not offered at all while it is off.
    fn is_enabled(&self, cx: &App) -> bool;

    /// Finds the workspace showing `project` and runs the command in its browser panel.
    fn run(
        &self,
        project: &Entity<Project>,
        command: BrowserCommand,
        cx: &mut App,
    ) -> Task<Result<BrowserOutput>>;
}

/// What every consumer answers a browser command with while `browser.ai_control` is off.
pub const AI_CONTROL_OFF_MESSAGE: &str = "AI control of the built-in browser is turned off. \
     Only the user can turn it on, in the settings (browser.ai_control).";

pub struct GlobalBrowserControl(pub Arc<dyn BrowserControl>);

impl Global for GlobalBrowserControl {}

pub fn browser_control(cx: &App) -> Option<Arc<dyn BrowserControl>> {
    cx.try_global::<GlobalBrowserControl>()
        .map(|control| control.0.clone())
}

pub fn tool_names() -> impl Iterator<Item = &'static str> {
    TOOL_NAMES.iter().copied()
}

/// The `browser_tabs` listing.
pub fn format_tabs(tabs: &[TabInfo]) -> String {
    if tabs.is_empty() {
        return "No AI tabs are open. browser_open_tab or browser_navigate opens one.".to_string();
    }
    let mut text = format!(
        "{} AI tab{} (* = current AI tab):\n",
        tabs.len(),
        if tabs.len() == 1 { "" } else { "s" }
    );
    for tab in tabs {
        let marker = if tab.active { '*' } else { ' ' };
        let title = if tab.title.trim().is_empty() {
            "(untitled)"
        } else {
            tab.title.trim()
        };
        let loading = if tab.loading { " (loading)" } else { "" };
        text.push_str(&format!(
            "{marker} {}: {title} - {}{loading}\n",
            tab.id, tab.url
        ));
    }
    text
}

// The tool descriptions are concatenated at compile time so that `ToolDefinition` can keep
// `&'static str` descriptions; `concat!` only takes literals, hence macros instead of consts.
macro_rules! network_note {
    () => {
        "AI tabs are separate from the user's own browser tabs: they use their own browser \
         profile (cookies and storage), so the user's sign-ins are not available unless the user \
         signed in inside an AI tab. All traffic goes through the user's network allowlist; a \
         host that is not allowed fails with an error naming it, and the user can add it to the \
         allowlist. `localhost` is the machine running the editor, also when the project is \
         remote (SSH or WSL). Only http, https, data: and about:blank URLs can be opened; \
         downloads are blocked. WebRTC calls connect only when they can be relayed through the \
         allowlist, and on some systems WebRTC isn't available in AI tabs."
    };
}

macro_rules! tab_property {
    () => {
        "Id of the AI tab, from browser_tabs. Omit for the current AI tab."
    };
}

macro_rules! ref_property {
    () => {
        "Element ref from browser_snapshot or browser_find, for example \"e12\". Refs stop \
         working when the page navigates."
    };
}

macro_rules! point_note {
    () => {
        "Give either `ref` or both `x` and `y`: CSS pixels from the top-left corner of the \
         viewport (browser_screenshot reports the viewport size in CSS pixels; its image may be \
         scaled down)."
    };
}

fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn tab_schema() -> Value {
    json!({ "type": "string", "description": tab_property!() })
}

fn ref_schema() -> Value {
    json!({ "type": "string", "description": ref_property!() })
}

fn coordinate_schema(axis: &str) -> Value {
    json!({
        "type": "number",
        "minimum": 0,
        "description": format!("{axis} coordinate in CSS pixels from the top-left corner of the viewport."),
    })
}

fn limit_schema(description: &str, default: usize, maximum: usize) -> Value {
    json!({
        "type": "integer",
        "minimum": 1,
        "maximum": maximum,
        "default": default,
        "description": description,
    })
}

fn timeout_schema() -> Value {
    json!({
        "type": "integer",
        "minimum": 0,
        "maximum": MAX_TIMEOUT_MS,
        "default": DEFAULT_TIMEOUT_MS,
        "description": "How long to wait, in milliseconds.",
    })
}

fn max_chars_schema() -> Value {
    limit_schema(
        "Longest text to return; the rest is cut off.",
        DEFAULT_MAX_CHARS,
        MAX_CHARS_LIMIT,
    )
}

pub fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "browser_tabs",
            description: concat!(
                "List the AI tabs of the built-in browser in this workspace: their ids, titles \
                 and URLs, with the current AI tab marked. ",
                network_note!()
            ),
            input_schema: object_schema(json!({}), &[]),
        },
        ToolDefinition {
            name: "browser_open_tab",
            description: concat!(
                "Open a new AI tab in the built-in browser and make it the current AI tab. With \
                 `url` it starts loading that page but does not wait for it; use \
                 browser_navigate to load and wait. ",
                network_note!()
            ),
            input_schema: object_schema(
                json!({
                    "url": { "type": "string", "description": "Page to open. Omit for an empty tab." }
                }),
                &[],
            ),
        },
        ToolDefinition {
            name: "browser_close_tab",
            description: "Close an AI tab of the built-in browser.",
            input_schema: object_schema(
                json!({
                    "tab": { "type": "string", "description": "Id of the AI tab, from browser_tabs." }
                }),
                &["tab"],
            ),
        },
        ToolDefinition {
            name: "browser_select_tab",
            description: "Make an AI tab the current AI tab (the one the other browser tools use \
                          when `tab` is omitted) and the front tab of the AI browser panel. It \
                          does not bring a hidden panel forward: every browser tool works on AI \
                          tabs whether or not the user can see them.",
            input_schema: object_schema(
                json!({
                    "tab": { "type": "string", "description": "Id of the AI tab, from browser_tabs." }
                }),
                &["tab"],
            ),
        },
        ToolDefinition {
            name: "browser_navigate",
            description: concat!(
                "Load a URL in an AI tab, or go back, forward or reload, and wait until the page \
                 has loaded or `timeout_ms` passes. Opens an AI tab first when there is none. Give \
                 exactly one of `url` and `action`. ",
                network_note!()
            ),
            input_schema: object_schema(
                json!({
                    "tab": tab_schema(),
                    "url": { "type": "string", "description": "Page to load, for example http://localhost:3000/." },
                    "action": {
                        "type": "string",
                        "enum": ["back", "forward", "reload"],
                        "description": "Instead of a URL: go back or forward in the tab's history, or reload the page.",
                    },
                    "timeout_ms": timeout_schema(),
                }),
                &[],
            ),
        },
        ToolDefinition {
            name: "browser_page_text",
            description: "Read the visible text of the page in an AI tab, cut off at `max_chars`. \
                          Use it to check what the page says; use browser_snapshot or \
                          browser_find to get elements to act on.",
            input_schema: object_schema(
                json!({
                    "tab": tab_schema(),
                    "max_chars": max_chars_schema(),
                }),
                &[],
            ),
        },
        ToolDefinition {
            name: "browser_snapshot",
            description: "Get an outline of the elements of the page in an AI tab (role, name, \
                          value and state), each with a ref to pass to browser_click, \
                          browser_hover, browser_type, browser_scroll and browser_set_value. \
                          Take a new snapshot after the page changes; refs stop working when the \
                          page navigates.",
            input_schema: object_schema(
                json!({
                    "tab": tab_schema(),
                    "interactive_only": {
                        "type": "boolean",
                        "default": false,
                        "description": "List only links, buttons, form fields and other elements that respond to input.",
                    },
                    "max_chars": max_chars_schema(),
                }),
                &[],
            ),
        },
        ToolDefinition {
            name: "browser_find",
            description: "Find elements of the page in an AI tab whose text, accessible name, \
                          label or placeholder contains `query` (ignoring case), and return their \
                          refs with a short description of each. Cheaper than browser_snapshot \
                          when you know what you are looking for.",
            input_schema: object_schema(
                json!({
                    "tab": tab_schema(),
                    "query": { "type": "string", "description": "Text to look for." },
                    "limit": limit_schema("Most elements to return.", DEFAULT_FIND_LIMIT, MAX_FIND_LIMIT),
                }),
                &["query"],
            ),
        },
        ToolDefinition {
            name: "browser_click",
            description: concat!(
                "Click an element or a point of the page in an AI tab. ",
                point_note!()
            ),
            input_schema: object_schema(
                json!({
                    "tab": tab_schema(),
                    "ref": ref_schema(),
                    "x": coordinate_schema("Horizontal"),
                    "y": coordinate_schema("Vertical"),
                    "button": {
                        "type": "string",
                        "enum": ["left", "middle", "right"],
                        "default": "left",
                        "description": "Mouse button to click with.",
                    },
                    "click_count": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_CLICK_COUNT,
                        "default": 1,
                        "description": "2 for a double click, 3 for a triple click.",
                    },
                    "modifiers": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["shift", "control", "alt", "meta"] },
                        "description": "Keys held down during the click.",
                    },
                }),
                &[],
            ),
        },
        ToolDefinition {
            name: "browser_hover",
            description: concat!(
                "Move the mouse over an element or a point of the page in an AI tab, for example \
                 to open a hover menu or a tooltip. ",
                point_note!()
            ),
            input_schema: object_schema(
                json!({
                    "tab": tab_schema(),
                    "ref": ref_schema(),
                    "x": coordinate_schema("Horizontal"),
                    "y": coordinate_schema("Vertical"),
                }),
                &[],
            ),
        },
        ToolDefinition {
            name: "browser_type",
            description: "Type text into the page in an AI tab, as key presses that fire input \
                          events. With `ref` the element is focused first; without it the text \
                          goes to the focused element. The text is inserted at the caret and the \
                          field's existing value is kept; use browser_set_value to replace a \
                          value. With `submit`, Enter is pressed afterwards.",
            input_schema: object_schema(
                json!({
                    "tab": tab_schema(),
                    "ref": ref_schema(),
                    "text": { "type": "string", "description": "Text to type." },
                    "submit": { "type": "boolean", "default": false, "description": "Press Enter after typing." },
                }),
                &["text"],
            ),
        },
        ToolDefinition {
            name: "browser_press_key",
            description: "Press keys in the page in an AI tab; they go to the focused element. \
                          `keys` is a space-separated sequence of presses, each a key name with \
                          optional modifiers joined by `+`, for example \"Enter\", \"Tab\", \
                          \"ctrl+a Backspace\" or \"shift+ArrowDown\". Key names are those of \
                          KeyboardEvent.key (Enter, Escape, ArrowUp, PageDown, F5, a, ...).",
            input_schema: object_schema(
                json!({
                    "tab": tab_schema(),
                    "keys": { "type": "string", "description": "Key presses, separated by spaces." },
                    "repeat": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_KEY_REPEAT,
                        "default": 1,
                        "description": "How many times to press the whole sequence.",
                    },
                }),
                &["keys"],
            ),
        },
        ToolDefinition {
            name: "browser_scroll",
            description: "Scroll the page in an AI tab, or the scrollable element at a ref or \
                          point, by `amount` steps of about one mouse-wheel notch (100 CSS \
                          pixels) each. Without `ref`, `x` and `y` the whole page scrolls.",
            input_schema: object_schema(
                json!({
                    "tab": tab_schema(),
                    "ref": ref_schema(),
                    "x": coordinate_schema("Horizontal"),
                    "y": coordinate_schema("Vertical"),
                    "direction": {
                        "type": "string",
                        "enum": ["up", "down", "left", "right"],
                        "description": "Which way to scroll.",
                    },
                    "amount": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_SCROLL_AMOUNT,
                        "default": DEFAULT_SCROLL_AMOUNT,
                        "description": "How many steps to scroll.",
                    },
                }),
                &["direction"],
            ),
        },
        ToolDefinition {
            name: "browser_set_value",
            description: "Set the value of a form control in an AI tab directly and fire its \
                          input and change events: a text field or text area (a string), a \
                          select (an option's value or visible text, or an array of them for a \
                          multiple select), a checkbox (true or false) or a radio button (true \
                          selects it).",
            input_schema: object_schema(
                json!({
                    "tab": tab_schema(),
                    "ref": ref_schema(),
                    "value": {
                        "anyOf": [
                            { "type": "string" },
                            { "type": "number" },
                            { "type": "boolean" },
                            { "type": "array", "items": { "type": "string" } },
                        ],
                        "description": "The new value.",
                    },
                }),
                &["ref", "value"],
            ),
        },
        ToolDefinition {
            name: "browser_screenshot",
            description: "Take a PNG screenshot of the visible part of the page in an AI tab, \
                          scaled down so that its longer edge is at most 1568 pixels. The text \
                          part of the result gives the viewport size in CSS pixels, which is what \
                          the `x` and `y` of browser_click, browser_hover and browser_scroll use.",
            input_schema: object_schema(json!({ "tab": tab_schema() }), &[]),
        },
        ToolDefinition {
            name: "browser_console",
            description: "Read the console messages and uncaught errors of the page in an AI tab, \
                          oldest first, since the page loaded or the last `clear`. They are lost \
                          when the page navigates.",
            input_schema: object_schema(
                json!({
                    "tab": tab_schema(),
                    "only_errors": { "type": "boolean", "default": false, "description": "Only errors and uncaught exceptions." },
                    "pattern": { "type": "string", "description": "Only messages containing this text (ignoring case)." },
                    "limit": limit_schema("Most messages to return (the newest ones).", DEFAULT_LOG_LIMIT, MAX_LOG_LIMIT),
                    "clear": { "type": "boolean", "default": false, "description": "Empty the buffer after reading it." },
                }),
                &[],
            ),
        },
        ToolDefinition {
            name: "browser_network",
            description: "List the fetch and XMLHttpRequest requests the page in an AI tab made \
                          (method, URL, status, duration and an id), oldest first, since the page \
                          loaded or the last `clear`; or, with `request_id`, show one of them in \
                          detail with its headers and the start of its bodies. Requests the \
                          document makes by itself (navigations, scripts, images) are not \
                          included, and the list is lost when the page navigates.",
            input_schema: object_schema(
                json!({
                    "tab": tab_schema(),
                    "url_pattern": { "type": "string", "description": "Only requests whose URL contains this text (ignoring case)." },
                    "limit": limit_schema("Most requests to return (the newest ones).", DEFAULT_LOG_LIMIT, MAX_LOG_LIMIT),
                    "clear": { "type": "boolean", "default": false, "description": "Empty the list after reading it." },
                    "request_id": { "type": "string", "description": "Id of one request from an earlier listing, to show it in detail." },
                }),
                &[],
            ),
        },
        ToolDefinition {
            name: "browser_eval",
            description: "Evaluate a JavaScript expression in the page in an AI tab and return \
                          its value as JSON; a returned Promise is awaited. It runs with the \
                          page's own privileges. Use it to inspect state the other tools do not \
                          show (application state, storage, computed styles).",
            input_schema: object_schema(
                json!({
                    "tab": tab_schema(),
                    "expression": {
                        "type": "string",
                        "maxLength": MAX_EXPRESSION_CHARS,
                        "description": "A JavaScript expression, or an immediately invoked function for several statements.",
                    },
                }),
                &["expression"],
            ),
        },
        ToolDefinition {
            name: "browser_wait_for",
            description: "Wait until `text` appears in the visible text of the page in an AI tab \
                          and/or an element matching the CSS `selector` exists. Give at least one \
                          of them; when both are given, both must hold. Fails when `timeout_ms` \
                          passes first.",
            input_schema: object_schema(
                json!({
                    "tab": tab_schema(),
                    "text": { "type": "string", "description": "Text to wait for." },
                    "selector": { "type": "string", "description": "CSS selector of an element to wait for." },
                    "timeout_ms": timeout_schema(),
                }),
                &[],
            ),
        },
    ]
}

/// Validates the arguments of a browser tool call and fills in the defaults. Numbers above
/// their limit are lowered to it; anything else that is wrong is an error whose message is meant
/// for the model that made the call.
pub fn parse_command(tool_name: &str, input: Value) -> Result<BrowserCommand> {
    let Some(&tool) = TOOL_NAMES.iter().find(|name| **name == tool_name) else {
        bail!(
            "unknown browser tool `{tool_name}`; the browser tools are: {}",
            TOOL_NAMES.join(", ")
        );
    };
    let arguments = Arguments::new(tool, input)?;
    let command = match tool {
        "browser_tabs" => BrowserCommand::ListTabs,
        "browser_open_tab" => BrowserCommand::OpenTab {
            url: arguments.url("url")?,
        },
        "browser_close_tab" => BrowserCommand::CloseTab {
            tab: arguments.required_tab()?,
        },
        "browser_select_tab" => BrowserCommand::SelectTab {
            tab: arguments.required_tab()?,
        },
        "browser_navigate" => {
            let url = arguments.url("url")?;
            let action = arguments.keyword(
                "action",
                &[
                    ("back", NavigateTarget::Back),
                    ("forward", NavigateTarget::Forward),
                    ("reload", NavigateTarget::Reload),
                ],
            )?;
            let target = match (url, action) {
                (Some(url), None) => NavigateTarget::Url(url),
                (None, Some(action)) => action,
                (Some(_), Some(_)) => {
                    bail!("{tool}: give either `url` or `action`, not both")
                }
                (None, None) => bail!(
                    "{tool}: give `url` (the page to load) or `action` (back, forward or reload)"
                ),
            };
            BrowserCommand::Navigate {
                tab: arguments.tab()?,
                target,
                timeout_ms: arguments.timeout()?,
            }
        }
        "browser_page_text" => BrowserCommand::PageText {
            tab: arguments.tab()?,
            max_chars: arguments.max_chars()?,
        },
        "browser_snapshot" => BrowserCommand::Snapshot {
            tab: arguments.tab()?,
            interactive_only: arguments.boolean("interactive_only")?,
            max_chars: arguments.max_chars()?,
        },
        "browser_find" => BrowserCommand::Find {
            tab: arguments.tab()?,
            query: arguments.required_text("query")?,
            limit: arguments.count("limit", DEFAULT_FIND_LIMIT as u64, MAX_FIND_LIMIT as u64)?
                as usize,
        },
        "browser_click" => {
            let Some(target) = arguments.element_target()? else {
                bail!("{tool}: {}", MISSING_TARGET);
            };
            BrowserCommand::Click {
                tab: arguments.tab()?,
                target,
                button: arguments
                    .keyword(
                        "button",
                        &[
                            ("left", MouseButton::Left),
                            ("middle", MouseButton::Middle),
                            ("right", MouseButton::Right),
                        ],
                    )?
                    .unwrap_or(MouseButton::Left),
                click_count: arguments.count("click_count", 1, MAX_CLICK_COUNT.into())? as u8,
                modifiers: arguments.modifiers()?,
            }
        }
        "browser_hover" => {
            let Some(target) = arguments.element_target()? else {
                bail!("{tool}: {}", MISSING_TARGET);
            };
            BrowserCommand::Hover {
                tab: arguments.tab()?,
                target,
            }
        }
        "browser_type" => BrowserCommand::Type {
            tab: arguments.tab()?,
            element: arguments.element_ref()?,
            text: arguments
                .string("text")?
                .ok_or_else(|| anyhow!("{tool}: `text` is required"))?,
            submit: arguments.boolean("submit")?,
        },
        "browser_press_key" => BrowserCommand::PressKey {
            tab: arguments.tab()?,
            keys: arguments.required_text("keys")?,
            repeat: arguments.count("repeat", 1, MAX_KEY_REPEAT.into())? as u32,
        },
        "browser_scroll" => BrowserCommand::Scroll {
            tab: arguments.tab()?,
            target: arguments.element_target()?,
            direction: arguments
                .keyword(
                    "direction",
                    &[
                        ("up", ScrollDirection::Up),
                        ("down", ScrollDirection::Down),
                        ("left", ScrollDirection::Left),
                        ("right", ScrollDirection::Right),
                    ],
                )?
                .ok_or_else(|| {
                    anyhow!("{tool}: `direction` is required (up, down, left or right)")
                })?,
            amount: arguments.count(
                "amount",
                DEFAULT_SCROLL_AMOUNT.into(),
                MAX_SCROLL_AMOUNT.into(),
            )? as u32,
        },
        "browser_set_value" => BrowserCommand::SetValue {
            tab: arguments.tab()?,
            element: arguments.element_ref()?.ok_or_else(|| {
                anyhow!("{tool}: `ref` is required (from browser_snapshot or browser_find)")
            })?,
            value: arguments.form_value()?,
        },
        "browser_screenshot" => BrowserCommand::Screenshot {
            tab: arguments.tab()?,
        },
        "browser_console" => BrowserCommand::Console {
            tab: arguments.tab()?,
            only_errors: arguments.boolean("only_errors")?,
            pattern: arguments.optional_text("pattern")?,
            limit: arguments.count("limit", DEFAULT_LOG_LIMIT as u64, MAX_LOG_LIMIT as u64)?
                as usize,
            clear: arguments.boolean("clear")?,
        },
        "browser_network" => BrowserCommand::Network {
            tab: arguments.tab()?,
            url_pattern: arguments.optional_text("url_pattern")?,
            limit: arguments.count("limit", DEFAULT_LOG_LIMIT as u64, MAX_LOG_LIMIT as u64)?
                as usize,
            clear: arguments.boolean("clear")?,
            request_id: arguments.optional_text("request_id")?,
        },
        "browser_eval" => {
            let expression = arguments.required_text("expression")?;
            let length = expression.chars().count();
            if length > MAX_EXPRESSION_CHARS {
                bail!(
                    "{tool}: `expression` is {length} characters long; the limit is \
                     {MAX_EXPRESSION_CHARS}"
                );
            }
            BrowserCommand::Eval {
                tab: arguments.tab()?,
                expression,
            }
        }
        "browser_wait_for" => {
            let text = arguments.optional_text("text")?;
            let selector = arguments.optional_text("selector")?;
            if text.is_none() && selector.is_none() {
                bail!("{tool}: give `text`, `selector` or both");
            }
            BrowserCommand::WaitFor {
                tab: arguments.tab()?,
                text,
                selector,
                timeout_ms: arguments.timeout()?,
            }
        }
        _ => bail!("{tool}: this browser tool has no parser"),
    };
    Ok(command)
}

const MISSING_TARGET: &str =
    "give `ref` (from browser_snapshot or browser_find) or both `x` and `y`";

struct Arguments {
    tool: &'static str,
    values: Map<String, Value>,
}

impl Arguments {
    fn new(tool: &'static str, input: Value) -> Result<Self> {
        let mut values = match input {
            Value::Object(values) => values,
            Value::Null => Map::new(),
            other => bail!(
                "{tool}: the arguments must be a JSON object, not {}",
                json_kind(&other)
            ),
        };
        // Models often send `"key": null` for an argument they mean to leave out.
        values.retain(|_, value| !value.is_null());

        let known = known_properties(tool);
        let unknown = values
            .keys()
            .filter(|key| !known.contains(key))
            .map(|key| format!("`{key}`"))
            .collect::<Vec<_>>();
        if !unknown.is_empty() {
            let mut accepted = known;
            accepted.sort_unstable();
            bail!(
                "{tool}: unknown argument{} {}; the arguments are: {}",
                if unknown.len() == 1 { "" } else { "s" },
                unknown.join(", "),
                if accepted.is_empty() {
                    "none".to_string()
                } else {
                    accepted.join(", ")
                }
            );
        }
        Ok(Self { tool, values })
    }

    fn string(&self, key: &str) -> Result<Option<String>> {
        match self.values.get(key) {
            None => Ok(None),
            Some(Value::String(text)) => Ok(Some(text.clone())),
            Some(other) => bail!(
                "{}: `{key}` must be a string, not {}",
                self.tool,
                json_kind(other)
            ),
        }
    }

    /// A string that has to say something when given; empty means absent.
    fn optional_text(&self, key: &str) -> Result<Option<String>> {
        Ok(self.string(key)?.filter(|text| !text.trim().is_empty()))
    }

    fn required_text(&self, key: &str) -> Result<String> {
        match self.string(key)? {
            Some(text) if !text.trim().is_empty() => Ok(text),
            Some(_) => bail!("{}: `{key}` must not be empty", self.tool),
            None => bail!("{}: `{key}` is required", self.tool),
        }
    }

    fn url(&self, key: &str) -> Result<Option<String>> {
        match self.string(key)? {
            Some(url) if url.trim().is_empty() => bail!("{}: `{key}` is empty", self.tool),
            Some(url) => Ok(Some(url.trim().to_string())),
            None => Ok(None),
        }
    }

    fn tab(&self) -> Result<Option<TabId>> {
        let tab = match self.values.get("tab") {
            None => return Ok(None),
            Some(Value::String(tab)) => tab.trim().to_string(),
            // Tab ids look like numbers, so models send them as numbers too.
            Some(Value::Number(number)) => number.to_string(),
            Some(other) => bail!(
                "{}: `tab` must be a tab id from browser_tabs, not {}",
                self.tool,
                json_kind(other)
            ),
        };
        if tab.is_empty() {
            bail!(
                "{}: `tab` is empty; omit it for the current AI tab",
                self.tool
            );
        }
        Ok(Some(TabId(tab)))
    }

    fn required_tab(&self) -> Result<TabId> {
        self.tab()?
            .ok_or_else(|| anyhow!("{}: `tab` is required (an id from browser_tabs)", self.tool))
    }

    fn element_ref(&self) -> Result<Option<String>> {
        match self.string("ref")? {
            Some(element) if element.trim().is_empty() => {
                bail!("{}: `ref` is empty", self.tool)
            }
            Some(element) => Ok(Some(element.trim().to_string())),
            None => Ok(None),
        }
    }

    fn element_target(&self) -> Result<Option<ElementTarget>> {
        let element = self.element_ref()?;
        let x = self.coordinate("x")?;
        let y = self.coordinate("y")?;
        match (element, x, y) {
            (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
                bail!("{}: give either `ref` or `x` and `y`, not both", self.tool)
            }
            (Some(element), None, None) => Ok(Some(ElementTarget::Ref(element))),
            (None, Some(x), Some(y)) => Ok(Some(ElementTarget::Point { x, y })),
            (None, Some(_), None) | (None, None, Some(_)) => {
                bail!("{}: `x` and `y` must be given together", self.tool)
            }
            (None, None, None) => Ok(None),
        }
    }

    fn coordinate(&self, key: &str) -> Result<Option<f64>> {
        let Some(value) = self.values.get(key) else {
            return Ok(None);
        };
        let coordinate = match value {
            Value::Number(number) => number.as_f64(),
            Value::String(text) => text.trim().parse::<f64>().ok(),
            _ => None,
        };
        match coordinate {
            Some(coordinate) if coordinate.is_finite() && coordinate >= 0. => Ok(Some(coordinate)),
            _ => bail!(
                "{}: `{key}` must be a number of CSS pixels, 0 or more (got {value})",
                self.tool
            ),
        }
    }

    fn boolean(&self, key: &str) -> Result<bool> {
        match self.values.get(key) {
            None => Ok(false),
            Some(Value::Bool(value)) => Ok(*value),
            Some(Value::String(text)) if text.eq_ignore_ascii_case("true") => Ok(true),
            Some(Value::String(text)) if text.eq_ignore_ascii_case("false") => Ok(false),
            Some(other) => bail!("{}: `{key}` must be true or false (got {other})", self.tool),
        }
    }

    fn integer(&self, key: &str) -> Result<Option<u64>> {
        let Some(value) = self.values.get(key) else {
            return Ok(None);
        };
        let integer = match value {
            Value::Number(number) => number.as_u64().or_else(|| {
                number
                    .as_f64()
                    .filter(|float| {
                        float.fract() == 0. && *float >= 0. && *float <= u64::MAX as f64
                    })
                    .map(|float| float as u64)
            }),
            Value::String(text) => text.trim().parse::<u64>().ok(),
            _ => None,
        };
        match integer {
            Some(integer) => Ok(Some(integer)),
            None => bail!(
                "{}: `{key}` must be a whole number, 0 or more (got {value})",
                self.tool
            ),
        }
    }

    /// A count of at least 1, lowered to `maximum` when larger. Every maximum fits the type
    /// the caller casts the result to, so those casts cannot truncate.
    fn count(&self, key: &str, default: u64, maximum: u64) -> Result<u64> {
        let Some(count) = self.integer(key)? else {
            return Ok(default);
        };
        if count == 0 {
            bail!("{}: `{key}` must be at least 1", self.tool);
        }
        Ok(count.min(maximum))
    }

    fn max_chars(&self) -> Result<usize> {
        Ok(self.count(
            "max_chars",
            DEFAULT_MAX_CHARS as u64,
            MAX_CHARS_LIMIT as u64,
        )? as usize)
    }

    fn timeout(&self) -> Result<u64> {
        Ok(self
            .integer("timeout_ms")?
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .min(MAX_TIMEOUT_MS))
    }

    fn keyword<T: Clone>(&self, key: &str, choices: &[(&str, T)]) -> Result<Option<T>> {
        let Some(word) = self.string(key)? else {
            return Ok(None);
        };
        let word = word.trim();
        choices
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(word))
            .map(|(_, choice)| Some(choice.clone()))
            .ok_or_else(|| {
                anyhow!(
                    "{}: `{key}` must be one of {} (got \"{word}\")",
                    self.tool,
                    choices
                        .iter()
                        .map(|(name, _)| *name)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
    }

    fn modifiers(&self) -> Result<Vec<KeyModifier>> {
        let Some(value) = self.values.get("modifiers") else {
            return Ok(Vec::new());
        };
        let Value::Array(names) = value else {
            bail!(
                "{}: `modifiers` must be an array such as [\"shift\"]",
                self.tool
            );
        };
        let mut modifiers = Vec::new();
        for name in names {
            let modifier = match name.as_str().map(|name| name.trim().to_ascii_lowercase()) {
                Some(name) => match name.as_str() {
                    "shift" => KeyModifier::Shift,
                    "control" | "ctrl" => KeyModifier::Control,
                    "alt" => KeyModifier::Alt,
                    "meta" | "cmd" => KeyModifier::Meta,
                    _ => bail!(
                        "{}: unknown modifier \"{name}\"; use shift, control, alt or meta",
                        self.tool
                    ),
                },
                None => bail!(
                    "{}: `modifiers` must contain strings, not {}",
                    self.tool,
                    json_kind(name)
                ),
            };
            if !modifiers.contains(&modifier) {
                modifiers.push(modifier);
            }
        }
        Ok(modifiers)
    }

    fn form_value(&self) -> Result<Value> {
        let value = self
            .values
            .get("value")
            .ok_or_else(|| anyhow!("{}: `value` is required", self.tool))?;
        match value {
            Value::String(_) | Value::Number(_) | Value::Bool(_) => Ok(value.clone()),
            Value::Array(items) if items.iter().all(Value::is_string) => Ok(value.clone()),
            Value::Array(_) => bail!(
                "{}: an array `value` (for a multiple select) must contain only strings",
                self.tool
            ),
            other => bail!(
                "{}: `value` must be a string, number, boolean or array of strings, not {}",
                self.tool,
                json_kind(other)
            ),
        }
    }
}

/// The argument names the tool's schema declares, which are the only ones it accepts, so that
/// the parser and the schema cannot disagree about which arguments exist.
fn known_properties(tool: &str) -> Vec<String> {
    tool_definitions()
        .into_iter()
        .find(|definition| definition.name == tool)
        .and_then(|definition| {
            definition.input_schema["properties"]
                .as_object()
                .map(|properties| properties.keys().cloned().collect())
        })
        .unwrap_or_default()
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn parse(tool: &str, input: Value) -> BrowserCommand {
        match parse_command(tool, input.clone()) {
            Ok(command) => command,
            Err(error) => panic!("{tool} {input} should parse: {error:#}"),
        }
    }

    fn parse_error(tool: &str, input: Value) -> String {
        match parse_command(tool, input.clone()) {
            Ok(command) => panic!("{tool} {input} should fail, got {command:?}"),
            Err(error) => format!("{error:#}"),
        }
    }

    fn minimal_input(tool: &str) -> Value {
        match tool {
            "browser_close_tab" | "browser_select_tab" => json!({ "tab": "1" }),
            "browser_navigate" => json!({ "url": "http://localhost:3000/" }),
            "browser_find" => json!({ "query": "Sign in" }),
            "browser_click" | "browser_hover" => json!({ "ref": "e1" }),
            "browser_type" => json!({ "text": "hello" }),
            "browser_press_key" => json!({ "keys": "Enter" }),
            "browser_scroll" => json!({ "direction": "down" }),
            "browser_set_value" => json!({ "ref": "e1", "value": "x" }),
            "browser_eval" => json!({ "expression": "document.title" }),
            "browser_wait_for" => json!({ "text": "Done" }),
            _ => json!({}),
        }
    }

    #[test]
    fn every_tool_parses_its_minimal_input() {
        for tool in tool_names() {
            parse(tool, minimal_input(tool));
        }
    }

    #[test]
    fn every_required_argument_is_enforced() {
        for definition in tool_definitions() {
            let required = definition.input_schema["required"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            for key in required {
                let key = key.as_str().unwrap_or_default();
                let mut input = minimal_input(definition.name);
                if let Some(object) = input.as_object_mut() {
                    object.remove(key);
                }
                let error = parse_error(definition.name, input);
                assert!(
                    error.contains(&format!("`{key}`")),
                    "{}: the error for a missing `{key}` should name it: {error}",
                    definition.name
                );
            }
        }
    }

    #[test]
    fn definitions_are_objects_with_unique_names_matching_tool_names() {
        let definitions = tool_definitions();
        let names = definitions
            .iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();
        assert_eq!(names, tool_names().collect::<Vec<_>>());
        let unique = names.iter().collect::<HashSet<_>>();
        assert_eq!(unique.len(), names.len(), "tool names must be unique");

        for definition in &definitions {
            let schema = &definition.input_schema;
            assert_eq!(schema["type"], json!("object"), "{}", definition.name);
            let properties = schema["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("{} has no properties object", definition.name));
            for key in schema["required"].as_array().into_iter().flatten() {
                let key = key.as_str().unwrap_or_default();
                assert!(
                    properties.contains_key(key),
                    "{} requires undeclared `{key}`",
                    definition.name
                );
            }
            assert!(definition.name.starts_with("browser_"));
            assert!(!definition.description.trim().is_empty());
        }

        let screenshot = definitions
            .iter()
            .find(|definition| definition.name == "browser_screenshot")
            .map(|definition| definition.description)
            .unwrap_or_default();
        assert!(screenshot.contains(&MAX_SCREENSHOT_EDGE_PX.to_string()));
    }

    #[test]
    fn defaults_are_applied() {
        assert_eq!(
            parse(
                "browser_navigate",
                json!({ "url": " https://example.com/ " })
            ),
            BrowserCommand::Navigate {
                tab: None,
                target: NavigateTarget::Url("https://example.com/".to_string()),
                timeout_ms: DEFAULT_TIMEOUT_MS,
            }
        );
        assert_eq!(
            parse("browser_snapshot", json!({})),
            BrowserCommand::Snapshot {
                tab: None,
                interactive_only: false,
                max_chars: DEFAULT_MAX_CHARS,
            }
        );
        assert_eq!(
            parse("browser_page_text", Value::Null),
            BrowserCommand::PageText {
                tab: None,
                max_chars: DEFAULT_MAX_CHARS,
            }
        );
        assert_eq!(
            parse("browser_find", json!({ "query": "Save" })),
            BrowserCommand::Find {
                tab: None,
                query: "Save".to_string(),
                limit: DEFAULT_FIND_LIMIT,
            }
        );
        assert_eq!(
            parse("browser_click", json!({ "ref": "e3" })),
            BrowserCommand::Click {
                tab: None,
                target: ElementTarget::Ref("e3".to_string()),
                button: MouseButton::Left,
                click_count: 1,
                modifiers: Vec::new(),
            }
        );
        assert_eq!(
            parse("browser_press_key", json!({ "keys": "ctrl+a Backspace" })),
            BrowserCommand::PressKey {
                tab: None,
                keys: "ctrl+a Backspace".to_string(),
                repeat: 1,
            }
        );
        assert_eq!(
            parse("browser_scroll", json!({ "direction": "up" })),
            BrowserCommand::Scroll {
                tab: None,
                target: None,
                direction: ScrollDirection::Up,
                amount: DEFAULT_SCROLL_AMOUNT,
            }
        );
        assert_eq!(
            parse("browser_console", json!({ "pattern": "" })),
            BrowserCommand::Console {
                tab: None,
                only_errors: false,
                pattern: None,
                limit: DEFAULT_LOG_LIMIT,
                clear: false,
            }
        );
        assert_eq!(
            parse("browser_network", json!({})),
            BrowserCommand::Network {
                tab: None,
                url_pattern: None,
                limit: DEFAULT_LOG_LIMIT,
                clear: false,
                request_id: None,
            }
        );
        assert_eq!(
            parse("browser_wait_for", json!({ "selector": "#done" })),
            BrowserCommand::WaitFor {
                tab: None,
                text: None,
                selector: Some("#done".to_string()),
                timeout_ms: DEFAULT_TIMEOUT_MS,
            }
        );
        assert_eq!(
            parse("browser_open_tab", json!({})),
            BrowserCommand::OpenTab { url: None }
        );
    }

    #[test]
    fn limits_are_capped() {
        assert_eq!(
            parse("browser_page_text", json!({ "max_chars": 10_000_000 })),
            BrowserCommand::PageText {
                tab: None,
                max_chars: MAX_CHARS_LIMIT,
            }
        );
        assert_eq!(
            parse("browser_find", json!({ "query": "a", "limit": 1000 })),
            BrowserCommand::Find {
                tab: None,
                query: "a".to_string(),
                limit: MAX_FIND_LIMIT,
            }
        );
        assert_eq!(
            parse(
                "browser_console",
                json!({ "limit": 9999, "only_errors": true })
            ),
            BrowserCommand::Console {
                tab: None,
                only_errors: true,
                pattern: None,
                limit: MAX_LOG_LIMIT,
                clear: false,
            }
        );
        assert_eq!(
            parse(
                "browser_navigate",
                json!({ "action": "reload", "timeout_ms": 999_999_999 })
            ),
            BrowserCommand::Navigate {
                tab: None,
                target: NavigateTarget::Reload,
                timeout_ms: MAX_TIMEOUT_MS,
            }
        );
        assert_eq!(
            parse(
                "browser_press_key",
                json!({ "keys": "Tab", "repeat": 5000 })
            ),
            BrowserCommand::PressKey {
                tab: None,
                keys: "Tab".to_string(),
                repeat: MAX_KEY_REPEAT,
            }
        );
        assert_eq!(
            parse(
                "browser_scroll",
                json!({ "direction": "down", "amount": u64::MAX })
            ),
            BrowserCommand::Scroll {
                tab: None,
                target: None,
                direction: ScrollDirection::Down,
                amount: MAX_SCROLL_AMOUNT,
            }
        );
        assert_eq!(
            parse(
                "browser_click",
                json!({ "x": 10, "y": 20.5, "click_count": 7, "button": "Right", "modifiers": ["ctrl", "shift", "control"] })
            ),
            BrowserCommand::Click {
                tab: None,
                target: ElementTarget::Point { x: 10., y: 20.5 },
                button: MouseButton::Right,
                click_count: MAX_CLICK_COUNT,
                modifiers: vec![KeyModifier::Control, KeyModifier::Shift],
            }
        );
    }

    #[test]
    fn lenient_spellings_are_accepted() {
        assert_eq!(
            parse(
                "browser_wait_for",
                json!({ "tab": 2, "text": "Done", "selector": null, "timeout_ms": "500" })
            ),
            BrowserCommand::WaitFor {
                tab: Some(TabId("2".to_string())),
                text: Some("Done".to_string()),
                selector: None,
                timeout_ms: 500,
            }
        );
        assert_eq!(
            parse(
                "browser_type",
                json!({ "ref": "e4", "text": "", "submit": "true" })
            ),
            BrowserCommand::Type {
                tab: None,
                element: Some("e4".to_string()),
                text: String::new(),
                submit: true,
            }
        );
        assert_eq!(
            parse("browser_find", json!({ "query": "x", "limit": 5.0 })),
            BrowserCommand::Find {
                tab: None,
                query: "x".to_string(),
                limit: 5,
            }
        );
    }

    #[test]
    fn bad_input_is_rejected_with_a_clear_error() {
        let cases = [
            ("browser_nope", json!({}), "unknown browser tool"),
            ("browser_tabs", json!([]), "must be a JSON object"),
            (
                "browser_tabs",
                json!({ "tab": "1" }),
                "unknown argument `tab`",
            ),
            (
                "browser_click",
                json!({ "selector": "a" }),
                "unknown argument",
            ),
            ("browser_click", json!({}), "give `ref`"),
            ("browser_click", json!({ "x": 1 }), "together"),
            (
                "browser_click",
                json!({ "ref": "e1", "x": 1, "y": 2 }),
                "not both",
            ),
            ("browser_click", json!({ "x": -1, "y": 2 }), "`x`"),
            (
                "browser_click",
                json!({ "ref": "e1", "button": "side" }),
                "`button`",
            ),
            (
                "browser_click",
                json!({ "ref": "e1", "modifiers": ["hyper"] }),
                "hyper",
            ),
            (
                "browser_click",
                json!({ "ref": "e1", "modifiers": "shift" }),
                "array",
            ),
            (
                "browser_click",
                json!({ "ref": "e1", "click_count": 0 }),
                "at least 1",
            ),
            ("browser_hover", json!({ "ref": " " }), "`ref` is empty"),
            ("browser_navigate", json!({}), "give `url`"),
            ("browser_navigate", json!({ "url": "  " }), "`url` is empty"),
            (
                "browser_navigate",
                json!({ "url": "http://a/", "action": "back" }),
                "not both",
            ),
            ("browser_navigate", json!({ "action": "home" }), "`action`"),
            ("browser_navigate", json!({ "url": 5 }), "must be a string"),
            (
                "browser_navigate",
                json!({ "url": "http://a/", "timeout_ms": -5 }),
                "whole number",
            ),
            ("browser_open_tab", json!({ "url": "" }), "`url` is empty"),
            ("browser_close_tab", json!({ "tab": "" }), "`tab` is empty"),
            ("browser_select_tab", json!({ "tab": true }), "tab id"),
            ("browser_find", json!({ "query": " " }), "must not be empty"),
            ("browser_page_text", json!({ "max_chars": 0 }), "at least 1"),
            (
                "browser_scroll",
                json!({ "direction": "sideways" }),
                "`direction`",
            ),
            (
                "browser_set_value",
                json!({ "ref": "e1", "value": {} }),
                "`value`",
            ),
            (
                "browser_set_value",
                json!({ "ref": "e1", "value": [1] }),
                "only strings",
            ),
            (
                "browser_snapshot",
                json!({ "interactive_only": "yes" }),
                "true or false",
            ),
            ("browser_wait_for", json!({}), "`text`, `selector`"),
            (
                "browser_eval",
                json!({ "expression": "x".repeat(MAX_EXPRESSION_CHARS + 1) }),
                "limit",
            ),
        ];
        for (tool, input, expected) in cases {
            let error = parse_error(tool, input.clone());
            assert!(
                error.contains(expected),
                "{tool} {input}: expected an error containing {expected:?}, got {error:?}"
            );
        }
    }

    #[test]
    fn the_tab_of_a_command_is_reported() {
        let command = parse("browser_screenshot", json!({ "tab": "3" }));
        assert_eq!(command.tab(), Some(&TabId("3".to_string())));
        assert_eq!(parse("browser_tabs", json!({})).tab(), None);
    }

    #[test]
    fn tabs_are_formatted_with_the_current_one_marked() {
        assert!(format_tabs(&[]).starts_with("No AI tabs are open"));
        let text = format_tabs(&[
            TabInfo {
                id: TabId("1".to_string()),
                title: "Dashboard".to_string(),
                url: "http://localhost:3000/".to_string(),
                active: true,
                loading: false,
            },
            TabInfo {
                id: TabId("2".to_string()),
                title: String::new(),
                url: "about:blank".to_string(),
                active: false,
                loading: true,
            },
        ]);
        assert_eq!(
            text,
            "2 AI tabs (* = current AI tab):\n\
             * 1: Dashboard - http://localhost:3000/\n  \
             2: (untitled) - about:blank (loading)\n"
        );
    }
}
