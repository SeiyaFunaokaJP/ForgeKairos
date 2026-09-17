mod persistence;
pub mod session_tools;
pub mod terminal_element;
pub mod terminal_panel;
mod terminal_path_like_target;
pub mod terminal_scrollbar;

use editor::{
    Editor, EditorEvent, EditorSettings, actions::SelectAll, blink_manager::BlinkManager,
    ui_scrollbar_settings_from_raw,
};
use gpui::{
    Action, AnyElement, App, ClipboardEntry, DismissEvent, Entity, EventEmitter, ExternalPaths,
    FocusHandle, Focusable, Font, KeyContext, KeyDownEvent, Keystroke, MouseButton, MouseDownEvent,
    Pixels, Point as GpuiPoint, Render, ScrollWheelEvent, Styled, Subscription, Task, TaskExt,
    WeakEntity, WindowId, actions, anchored, deferred, div,
};
use menu;
use persistence::TerminalDb;
use project::{Project, ProjectEntryId, search::SearchQuery};
use remote::ConnectionState;
use schemars::JsonSchema;
use serde::Deserialize;
use settings::{
    SeedQuerySetting, Settings, SettingsStore, TerminalBell, TerminalBlink, WorkingDirectory,
};
use std::{
    any::Any,
    cmp,
    ops::Range as StdRange,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::Duration,
};
use task::TaskId;
use terminal::{
    Clear, Copy, Event, HoveredWord, MaybeNavigationTarget, Modes, Paste, PasteText, Point, Range,
    ScrollLineDown, ScrollLineUp, ScrollPageDown, ScrollPageUp, ScrollToBottom, ScrollToTop,
    Search, ShowCharacterPalette, TaskState, TaskStatus, Terminal, TerminalBounds, ToggleViMode,
    session_log,
    terminal_macro::{MacroProgress, TerminalMacro},
    terminal_settings::{CursorShape, TerminalSettings},
};
use terminal_element::TerminalElement;
use terminal_panel::TerminalPanel;
use terminal_path_like_target::{hover_path_like_target, open_path_like_target};
use terminal_scrollbar::TerminalScrollHandle;
use ui::{
    Checkbox, ContextMenu, Divider, ScrollAxes, Scrollbars, ToggleState, Tooltip, WithScrollbar,
    prelude::*,
    scrollbars::{self, ScrollbarVisibility},
};
use util::ResultExt;
use workspace::{
    CloseActiveItem, DraggedSelection, DraggedTab, NewCenterTerminal, NewTerminal, Pane,
    ToolbarItemLocation, Workspace, WorkspaceId, delete_unloaded_items,
    item::{
        HighlightedText, Item, ItemEvent, SerializableItem, TabContentParams, TabTooltipContent,
    },
    register_serializable_item,
    searchable::{
        Direction, SearchEvent, SearchOptions, SearchToken, SearchableItem, SearchableItemHandle,
    },
};
use zed_actions::{agent::AddSelectionToThread, assistant::InlineAssist};

struct ImeState {
    marked_text: String,
}

fn viewport_line_for_point(point: Point, display_offset: usize) -> Option<usize> {
    let display_offset = i32::try_from(display_offset).unwrap_or(i32::MAX);
    let line = point.line.saturating_add(display_offset);
    if line < 0 {
        None
    } else {
        usize::try_from(line).ok()
    }
}

const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(500);

/// Labels for the recording controls, shared by the menus and the tab bar
/// toggle so both name the same command the same way. The one that opens a save
/// dialog is the one that ends in `…`.
pub(crate) const START_SESSION_LOG_LABEL: &str = "ログの記録を開始 (既定の保存先)";
pub(crate) const CHOOSE_SESSION_LOG_PATH_LABEL: &str = "ログの記録を開始 (保存先を選ぶ)…";
pub(crate) const STOP_SESSION_LOG_LABEL: &str = "ログの記録を停止";

/// Event to transmit the scroll from the element to the view
#[derive(Clone, Debug, PartialEq)]
pub struct ScrollTerminal(pub i32);

/// Sends the specified text directly to the terminal.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = terminal)]
pub struct SendText(String);

/// Sends a keystroke sequence to the terminal.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = terminal)]
pub struct SendKeystroke(String);

actions!(
    terminal,
    [
        /// Reruns the last executed task in the terminal.
        RerunTask,
        /// ターミナルの出力の記録を、設定した保存先で開始または停止します。
        ToggleSessionLog,
        /// ターミナルの出力をファイルに記録し始めます (保存先を選びます)。
        StartSessionLog,
        /// ターミナルの出力の記録を止めます。
        StopSessionLog,
        /// 記録中のログファイルをファイルマネージャーで表示します。
        OpenSessionLog,
        /// 記録中のログファイルをエディターで開き、末尾に追従します。
        TailSessionLog,
        /// 同じターミナルパネル内のすべてのペインへ同時に入力するかどうかを切り替えます。
        ToggleBroadcastInput,
        /// 実行中のマクロを中止します。
        CancelMacro,
        /// 終了したシェルを同じ作業ディレクトリで起動し直します。
        RestartTerminal,
    ]
);

/// Renames the terminal tab.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Action)]
#[action(namespace = terminal)]
pub struct RenameTerminal;

pub fn init(cx: &mut App) {
    terminal_panel::init(cx);
    session_tools::init(cx);

    register_serializable_item::<TerminalView>(cx);

    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(TerminalView::deploy);
    })
    .detach();
}

pub struct BlockProperties {
    pub height: u8,
    pub render: Box<dyn Send + Fn(&mut BlockContext) -> AnyElement>,
}

pub struct BlockContext<'a, 'b> {
    pub window: &'a mut Window,
    pub context: &'b mut App,
    pub dimensions: TerminalBounds,
}

///A terminal view, maintains the PTY's file handles and communicates with the terminal
pub struct TerminalView {
    terminal: Entity<Terminal>,
    workspace: WeakEntity<Workspace>,
    project: WeakEntity<Project>,
    focus_handle: FocusHandle,
    //Currently using iTerm bell, show bell emoji in tab until input is received
    has_bell: bool,
    context_menu: Option<(Entity<ContextMenu>, GpuiPoint<Pixels>, Subscription)>,
    cursor_shape: CursorShape,
    blink_manager: Entity<BlinkManager>,
    mode: TerminalMode,
    // Explicit override for whether workspace-specific context menu actions are shown.
    // When `None`, visibility is derived from `mode` (hidden for embedded terminals).
    show_workspace_actions: Option<bool>,
    blinking_terminal_enabled: bool,
    needs_serialize: bool,
    custom_title: Option<String>,
    hover: Option<HoverTarget>,
    hover_tooltip_update: Task<()>,
    workspace_id: Option<WorkspaceId>,
    show_breadcrumbs: bool,
    block_below_cursor: Option<Rc<BlockProperties>>,
    scroll_top: Pixels,
    scroll_handle: TerminalScrollHandle,
    ime_state: Option<ImeState>,
    self_handle: WeakEntity<Self>,
    rename_editor: Option<Entity<Editor>>,
    rename_editor_subscription: Option<Subscription>,
    macro_run: Option<MacroRun>,
    macro_status: Option<MacroStatus>,
    session_log_tail: Option<SessionLogTail>,
    /// Whether the project's remote connection was up the last time it was
    /// observed, so that one reconnection restarts a dead shell exactly once
    /// (observers run on every notify, not only on state changes).
    remote_connection_was_connected: bool,
    /// Kept so a restart in flight is cancelled with the view, and so a second
    /// request cannot spawn a second shell.
    restart_task: Option<Task<()>>,
    /// Focus listeners live on the `Window` they were registered against, so a
    /// terminal whose tab is torn off into another window has to register them
    /// again. Kept apart from `_subscriptions` so only these are rebuilt.
    _window_focus_subscriptions: Vec<Subscription>,
    /// Which window [`TerminalView::_window_focus_subscriptions`] were
    /// registered against.
    focus_listener_window: WindowId,
    _subscriptions: Vec<Subscription>,
    _terminal_subscriptions: Vec<Subscription>,
}

struct MacroRun {
    name: SharedString,
    progress: MacroProgress,
    /// Dropping this cancels the run, so it must stay owned by the view.
    _task: Task<()>,
}

enum MacroStatus {
    Finished(SharedString),
    Failed(SharedString),
}

/// An editor opened on the session log by `terminal::TailSessionLog`, kept
/// pinned to the end of the file while the log keeps growing.
struct SessionLogTail {
    /// Whether the view is still pinned to the end. Every user scroll re-reads
    /// it from the editor's position, so scrolling up stops the tail and
    /// scrolling back to the bottom resumes it, the way `tail -f` behaves in a
    /// pager.
    following: bool,
    _subscriptions: Vec<Subscription>,
}

/// The scroll offset that puts the last line of `editor` at the bottom of the
/// view. Before the first layout there is no visible line count to subtract, in
/// which case the caller's clamp is what keeps the position sane.
fn end_scroll_position(editor: &mut Editor, cx: &mut Context<Editor>) -> GpuiPoint<f64> {
    let last_row = f64::from(editor.max_point(cx).row().0);
    let visible_lines = editor.visible_line_count().unwrap_or(0.);
    let horizontal = editor.scroll_position(cx).x;
    GpuiPoint::new(horizontal, (last_row + 1. - visible_lines).max(0.))
}

/// Whether `editor` is showing the end of its buffer. A line of slack keeps a
/// tail that is one partially scrolled line short of the bottom from counting
/// as "the user scrolled away".
fn is_scrolled_to_end(editor: &mut Editor, cx: &mut Context<Editor>) -> bool {
    let end = end_scroll_position(editor, cx);
    editor.scroll_position(cx).y + 1. >= end.y
}

/// The 「実行中」 line of the session strip. A macro that runs once keeps the
/// short form; the repetition is only named when there is more than one.
///
/// A nested 繰り返し step is appended rather than folded into the step count,
/// so the outer numbers keep meaning "how far the macro as a whole has got".
fn macro_progress_label(name: &str, progress: MacroProgress) -> String {
    let step = format!("{}/{}", progress.step_index + 1, progress.step_count);
    let mut detail = match progress.iteration_count {
        Some(1) => step,
        Some(count) => format!("{}/{count} 回目、ステップ {step}", progress.iteration),
        None => format!("{} 回目、ステップ {step}", progress.iteration),
    };
    if let Some(nested) = progress.nested {
        let inner_iteration = match nested.iteration_count {
            Some(count) => format!("繰り返し {}/{count} のステップ", nested.iteration),
            None => format!("繰り返し {} 回目のステップ", nested.iteration),
        };
        detail = format!(
            "{detail} → {inner_iteration} {}/{}「{}」",
            nested.step_index + 1,
            nested.step_count,
            nested.kind_label
        );
    }
    // Appended as a clause rather than another arrow: which way a 分岐 went is
    // an annotation on the position above, and a branch may sit either side of
    // a 繰り返し in the nesting.
    if let Some(branch) = progress.branch {
        detail = format!("{detail}、分岐: {}", branch.label());
    }
    format!("マクロ「{name}」実行中 ({detail})")
}

#[derive(Default, Clone)]
pub enum TerminalMode {
    #[default]
    Standalone,
    Embedded {
        max_lines_when_unfocused: Option<usize>,
    },
}

#[derive(Clone)]
pub enum ContentMode {
    Scrollable,
    Inline {
        displayed_lines: usize,
        total_lines: usize,
    },
}

impl ContentMode {
    pub fn is_limited(&self) -> bool {
        match self {
            ContentMode::Scrollable => false,
            ContentMode::Inline {
                displayed_lines,
                total_lines,
            } => displayed_lines < total_lines,
        }
    }

    pub fn is_scrollable(&self) -> bool {
        matches!(self, ContentMode::Scrollable)
    }
}

#[derive(Debug)]
#[cfg_attr(test, derive(Clone, Eq, PartialEq))]
struct HoverTarget {
    tooltip: String,
    hovered_word: HoveredWord,
}

impl EventEmitter<Event> for TerminalView {}
impl EventEmitter<ItemEvent> for TerminalView {}
impl EventEmitter<SearchEvent> for TerminalView {}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TerminalView {
    ///Create a new Terminal in the current working directory or the user's home directory
    pub fn deploy(
        workspace: &mut Workspace,
        action: &NewCenterTerminal,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let local = action.local;
        let working_directory = default_working_directory(workspace, cx);
        TerminalPanel::add_center_terminal(workspace, window, cx, move |project, cx| {
            if local {
                project.create_local_terminal(cx)
            } else {
                project.create_terminal_shell(working_directory, cx)
            }
        })
        .detach_and_log_err(cx);
    }

    pub fn new(
        terminal: Entity<Terminal>,
        workspace: WeakEntity<Workspace>,
        workspace_id: Option<WorkspaceId>,
        project: WeakEntity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let workspace_handle = workspace.clone();
        let terminal_subscriptions =
            subscribe_for_terminal_events(&terminal, workspace, window, cx);

        let focus_handle = cx.focus_handle();
        let window_focus_subscriptions =
            Self::build_window_focus_subscriptions(&focus_handle, window, cx);
        let focus_listener_window = window.window_handle().window_id();
        let cursor_shape = TerminalSettings::get_global(cx).cursor_shape;

        let scroll_handle = TerminalScrollHandle::new(terminal.read(cx));

        let blink_manager = cx.new(|cx| {
            BlinkManager::new(
                CURSOR_BLINK_INTERVAL,
                |cx| {
                    !matches!(
                        TerminalSettings::get_global(cx).blinking,
                        TerminalBlink::Off
                    )
                },
                cx,
            )
        });

        let mut subscriptions = vec![
            cx.observe(&blink_manager, |_, _, cx| cx.notify()),
            cx.observe_global::<SettingsStore>(Self::settings_changed),
        ];

        let remote_client = project
            .upgrade()
            .and_then(|project| project.read(cx).remote_client());
        let remote_connection_was_connected = remote_client.as_ref().is_some_and(|remote_client| {
            remote_client.read(cx).connection_state() == ConnectionState::Connected
        });
        if let Some(remote_client) = remote_client {
            subscriptions.push(cx.observe_in(
                &remote_client,
                window,
                |terminal_view, remote_client, window, cx| {
                    let connection_state = remote_client.read(cx).connection_state();
                    terminal_view.remote_connection_state_changed(connection_state, window, cx);
                },
            ));
        }

        let mut this = Self {
            terminal,
            workspace: workspace_handle,
            project,
            has_bell: false,
            focus_handle,
            context_menu: None,
            cursor_shape,
            blink_manager,
            blinking_terminal_enabled: false,
            hover: None,
            hover_tooltip_update: Task::ready(()),
            mode: TerminalMode::Standalone,
            show_workspace_actions: None,
            workspace_id,
            show_breadcrumbs: TerminalSettings::get_global(cx).toolbar.breadcrumbs,
            block_below_cursor: None,
            scroll_top: Pixels::ZERO,
            scroll_handle,
            needs_serialize: false,
            custom_title: None,
            ime_state: None,
            self_handle: cx.entity().downgrade(),
            rename_editor: None,
            rename_editor_subscription: None,
            macro_run: None,
            macro_status: None,
            session_log_tail: None,
            remote_connection_was_connected,
            restart_task: None,
            _window_focus_subscriptions: window_focus_subscriptions,
            focus_listener_window,
            _subscriptions: subscriptions,
            _terminal_subscriptions: terminal_subscriptions,
        };

        if TerminalSettings::get_global(cx).session_log.auto
            && this.terminal.read(cx).is_pty()
            && let Err(error) = this.start_session_log_at_default_path(None, cx)
        {
            log::warn!("could not start the automatic terminal session log: {error:#}");
        }

        this
    }

    /// Enable 'embedded' mode where the terminal displays the full content with an optional limit of lines.
    pub fn set_embedded_mode(
        &mut self,
        max_lines_when_unfocused: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        self.mode = TerminalMode::Embedded {
            max_lines_when_unfocused,
        };
        // `new` cannot tell an embedded terminal from a standalone one, so an
        // automatic session log started there is undone once the host declares
        // itself; the agent's terminals are not sessions the user is running.
        self.terminal.update(cx, |terminal, cx| {
            terminal.stop_session_log(cx);
        });
        cx.notify();
    }

    /// Explicitly override whether workspace-specific context menu actions (e.g. creating or
    /// closing terminal tabs, inline assist) are shown.
    ///
    /// This lets hosts that aren't workspace panes (such as the agent panel) hide these
    /// actions without `terminal_view` needing to know about those hosts. When never called,
    /// visibility is derived from the terminal's `mode`.
    pub fn set_show_workspace_actions(&mut self, show: bool, cx: &mut Context<Self>) {
        self.show_workspace_actions = Some(show);
        cx.notify();
    }

    fn shows_workspace_actions(&self) -> bool {
        self.show_workspace_actions
            .unwrap_or_else(|| !matches!(self.mode, TerminalMode::Embedded { .. }))
    }

    const MAX_EMBEDDED_LINES: usize = 1_000;

    /// Returns the current `ContentMode` depending on the set `TerminalMode` and the current number of lines
    ///
    /// Note: Even in embedded mode, the terminal will fallback to scrollable when its content exceeds `MAX_EMBEDDED_LINES`
    pub fn content_mode(&self, window: &Window, cx: &App) -> ContentMode {
        match &self.mode {
            TerminalMode::Standalone => ContentMode::Scrollable,
            TerminalMode::Embedded {
                max_lines_when_unfocused,
            } => {
                let terminal = self.terminal.read(cx);
                let total_lines = terminal.total_lines();

                if total_lines > Self::MAX_EMBEDDED_LINES {
                    ContentMode::Scrollable
                } else {
                    let mut displayed_lines = terminal.used_lines().min(total_lines);

                    if !self.focus_handle.is_focused(window)
                        && let Some(max_lines) = max_lines_when_unfocused
                    {
                        displayed_lines = displayed_lines.min(*max_lines)
                    }

                    ContentMode::Inline {
                        displayed_lines,
                        total_lines,
                    }
                }
            }
        }
    }

    /// Sets the marked (pre-edit) text from the IME.
    pub(crate) fn set_marked_text(&mut self, text: String, cx: &mut Context<Self>) {
        if text.is_empty() {
            return self.clear_marked_text(cx);
        }
        self.ime_state = Some(ImeState { marked_text: text });
        cx.notify();
    }

    /// Gets the current marked range (UTF-16).
    pub(crate) fn marked_text_range(&self) -> Option<StdRange<usize>> {
        self.ime_state
            .as_ref()
            .map(|state| 0..state.marked_text.encode_utf16().count())
    }

    /// Clears the marked (pre-edit) text state.
    pub(crate) fn clear_marked_text(&mut self, cx: &mut Context<Self>) {
        if self.ime_state.is_some() {
            self.ime_state = None;
            cx.notify();
        }
    }

    /// Commits (sends) the given text to the PTY. Called by InputHandler::replace_text_in_range.
    pub(crate) fn commit_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if !text.is_empty() {
            self.terminal.update(cx, |term, _| {
                term.input(text.to_string().into_bytes());
            });
            self.broadcast_input(cx, |terminal, cx| {
                terminal.update(cx, |terminal, _| {
                    terminal.input(text.to_string().into_bytes());
                });
            });
        }
    }

    /// Whether this view takes part in 全ペイン同時入力. An agent's embedded
    /// terminal is not in the panel to begin with, and a task terminal reruns
    /// its own command rather than taking dictation.
    pub(crate) fn accepts_broadcast_input(&self, cx: &App) -> bool {
        matches!(self.mode, TerminalMode::Standalone) && self.terminal.read(cx).task().is_none()
    }

    /// The other terminals in this view's panel that input should also reach.
    /// Empty unless 同時入力 is on and this view is itself one of them.
    pub(crate) fn broadcast_targets(&self, cx: &App) -> Vec<Entity<Terminal>> {
        if !self.accepts_broadcast_input(cx) {
            return Vec::new();
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return Vec::new();
        };
        let Some(terminal_panel) = workspace.read(cx).panel::<TerminalPanel>(cx) else {
            return Vec::new();
        };
        let terminal_panel = terminal_panel.read(cx);
        if !terminal_panel.broadcast_input() {
            return Vec::new();
        }

        // This runs while `self`'s own entity is leased (from `render`, or from
        // a key handler), so the walk must recognize this view by its id and
        // never read it back out of the panel.
        let own_id = self.self_handle.entity_id();
        let mut targets = Vec::new();
        let mut belongs_to_panel = false;
        for pane in terminal_panel.panes() {
            for item in pane.read(cx).items() {
                let Some(terminal_view) = item.downcast::<TerminalView>() else {
                    continue;
                };
                if terminal_view.entity_id() == own_id {
                    belongs_to_panel = true;
                    continue;
                }
                let terminal_view = terminal_view.read(cx);
                if terminal_view.accepts_broadcast_input(cx) {
                    targets.push(terminal_view.terminal.clone());
                }
            }
        }

        // A terminal outside the panel (an agent's, or a center-pane tab) must
        // not drive the panel's shells.
        if belongs_to_panel {
            targets
        } else {
            Vec::new()
        }
    }

    /// Whether this view is currently mirroring its input into the panel's
    /// other shells, which is what the 「同時入力中」 chip reports.
    fn is_broadcasting_input(&self, cx: &App) -> bool {
        !self.broadcast_targets(cx).is_empty()
    }

    fn broadcast_input(
        &mut self,
        cx: &mut Context<Self>,
        send: impl Fn(&Entity<Terminal>, &mut Context<Self>),
    ) {
        for terminal in self.broadcast_targets(cx) {
            send(&terminal, cx);
        }
    }

    pub(crate) fn terminal_bounds(&self, cx: &App) -> TerminalBounds {
        self.terminal.read(cx).last_content().terminal_bounds
    }

    pub fn entity(&self) -> &Entity<Terminal> {
        &self.terminal
    }

    pub fn has_bell(&self) -> bool {
        self.has_bell
    }

    pub fn custom_title(&self) -> Option<&str> {
        self.custom_title.as_deref()
    }

    pub fn set_custom_title(&mut self, label: Option<String>, cx: &mut Context<Self>) {
        let label = label.filter(|l| !l.trim().is_empty());
        if self.custom_title != label {
            self.custom_title = label;
            self.needs_serialize = true;
            cx.emit(ItemEvent::UpdateTab);
            cx.notify();
        }
    }

    pub fn is_renaming(&self) -> bool {
        self.rename_editor.is_some()
    }

    pub fn rename_editor_is_focused(&self, window: &Window, cx: &App) -> bool {
        self.rename_editor
            .as_ref()
            .is_some_and(|editor| editor.focus_handle(cx).is_focused(window))
    }

    fn finish_renaming(&mut self, save: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.rename_editor.take() else {
            return;
        };
        self.rename_editor_subscription = None;
        if save {
            let new_label = editor.read(cx).text(cx).trim().to_string();
            let label = if new_label.is_empty() {
                None
            } else {
                // Only set custom_title if the text differs from the terminal's dynamic title.
                // This prevents subtle layout changes when clicking away without making changes.
                let terminal_title = self.terminal.read(cx).title(true);
                if new_label == terminal_title {
                    None
                } else {
                    Some(new_label)
                }
            };
            self.set_custom_title(label, cx);
        }
        cx.notify();
        self.focus_handle.focus(window, cx);
    }

    pub fn rename_terminal(
        &mut self,
        _: &RenameTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.terminal.read(cx).task().is_some() {
            return;
        }

        let current_label = self
            .custom_title
            .clone()
            .unwrap_or_else(|| self.terminal.read(cx).title(true));

        let rename_editor = cx.new(|cx| Editor::single_line(window, cx));
        let rename_editor_subscription = cx.subscribe_in(&rename_editor, window, {
            let rename_editor = rename_editor.clone();
            move |_this, _, event, window, cx| {
                if let editor::EditorEvent::Blurred = event {
                    // Defer to let focus settle (avoids canceling during double-click).
                    let rename_editor = rename_editor.clone();
                    cx.defer_in(window, move |this, window, cx| {
                        let still_current = this
                            .rename_editor
                            .as_ref()
                            .is_some_and(|current| current == &rename_editor);
                        if still_current && !rename_editor.focus_handle(cx).is_focused(window) {
                            this.finish_renaming(false, window, cx);
                        }
                    });
                }
            }
        });

        self.rename_editor = Some(rename_editor.clone());
        self.rename_editor_subscription = Some(rename_editor_subscription);

        rename_editor.update(cx, |editor, cx| {
            editor.set_text(current_label, window, cx);
            editor.select_all(&SelectAll, window, cx);
            editor.focus_handle(cx).focus(window, cx);
        });
        cx.notify();
    }

    pub fn clear_bell(&mut self, cx: &mut Context<TerminalView>) {
        self.has_bell = false;
        cx.emit(Event::Wakeup);
    }

    pub fn session_log_path(&self, cx: &App) -> Option<PathBuf> {
        self.terminal
            .read(cx)
            .session_log_path()
            .map(Path::to_path_buf)
    }

    /// Builds the path a new log would take from `terminal.session_log`.
    fn default_session_log_path(&self, host: Option<&str>, cx: &App) -> PathBuf {
        let settings = &TerminalSettings::get_global(cx).session_log;
        let title = self.terminal.read(cx).title(false);
        session_log::default_log_path(settings, &title, host)
    }

    /// Starts logging without asking, used by `terminal.session_log.auto` and
    /// by quick connect.
    pub fn start_session_log_at_default_path(
        &mut self,
        host: Option<&str>,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let path = self.default_session_log_path(host, cx);
        let options = TerminalSettings::get_global(cx).session_log.options();
        self.terminal.update(cx, |terminal, cx| {
            terminal.start_session_log(path, options, cx)
        })
    }

    /// The one-click recording toggle behind `terminal::ToggleSessionLog` and
    /// the tab bar button: no save dialog, the path comes from
    /// `terminal.session_log`.
    pub fn toggle_session_log(&mut self, cx: &mut Context<Self>) {
        if self.session_log_path(cx).is_some() {
            self.terminal.update(cx, |terminal, cx| {
                terminal.stop_session_log(cx);
            });
        } else if let Err(error) = self.start_session_log_at_default_path(None, cx) {
            self.report_error(format!("ログを開始できませんでした: {error}"), cx);
        }
        cx.notify();
    }

    fn toggle_session_log_action(
        &mut self,
        _: &ToggleSessionLog,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_session_log(cx);
    }

    fn start_session_log(
        &mut self,
        _: &StartSessionLog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let suggested = self.default_session_log_path(None, cx);
        let directory = suggested
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let file_name = suggested
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        let options = TerminalSettings::get_global(cx).session_log.options();
        let prompt = window.prompt_for_new_path(&directory, file_name.as_deref(), cx);

        cx.spawn_in(window, async move |this, cx| {
            let path = match prompt.await {
                Ok(Ok(Some(path))) => path,
                Ok(Ok(None)) => return,
                Ok(Err(error)) => {
                    this.update(cx, |this, cx| {
                        this.report_error(format!("保存先を選べませんでした: {error}"), cx);
                    })
                    .ok();
                    return;
                }
                Err(_) => return,
            };
            this.update(cx, |this, cx| {
                let started = this.terminal.update(cx, |terminal, cx| {
                    terminal.start_session_log(path, options, cx)
                });
                if let Err(error) = started {
                    this.report_error(format!("ログを開始できませんでした: {error}"), cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn stop_session_log(&mut self, _: &StopSessionLog, _: &mut Window, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, cx| {
            terminal.stop_session_log(cx);
        });
        cx.notify();
    }

    fn open_session_log(&mut self, _: &OpenSessionLog, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.session_log_path(cx) {
            cx.reveal_path(&path);
        }
    }

    /// Opens the running log in an editor tab and keeps it pinned to the end of
    /// the file, so a log that is still being written reads like `tail -f`.
    fn tail_session_log(
        &mut self,
        _: &TailSessionLog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self.session_log_path(cx) else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let open = workspace.update(cx, |workspace, cx| {
            workspace.open_abs_path(
                path,
                workspace::OpenOptions {
                    focus: Some(true),
                    ..Default::default()
                },
                window,
                cx,
            )
        });

        cx.spawn_in(window, async move |this, cx| {
            let item = match open.await {
                Ok(item) => item,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.report_error(format!("ログを開けませんでした: {error}"), cx);
                    })
                    .ok();
                    return;
                }
            };
            let Some(editor) = item.downcast::<Editor>() else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.follow_session_log(editor, window, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Starts following `editor` and jumps it to the end of the log.
    fn follow_session_log(
        &mut self,
        editor: Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let subscriptions = vec![
            cx.subscribe_in(
                &editor,
                window,
                |this, editor, event: &EditorEvent, window, cx| {
                    match event {
                        // A reload from disk reaches the editor as an edit of its
                        // buffer, which is what makes the log grow under the tail.
                        EditorEvent::BufferEdited => {
                            if this
                                .session_log_tail
                                .as_ref()
                                .is_some_and(|tail| tail.following)
                            {
                                editor.update(cx, |editor, cx| {
                                    let position = end_scroll_position(editor, cx);
                                    editor.set_scroll_position(position, window, cx);
                                });
                            }
                        }
                        // Every scroll re-derives whether the tail is still on: our
                        // own jump lands at the end and keeps it, a scroll up stops
                        // it, and a scroll back to the bottom turns it on again.
                        EditorEvent::ScrollPositionChanged { local: true, .. } => {
                            let at_end = editor.update(cx, is_scrolled_to_end);
                            if let Some(tail) = this.session_log_tail.as_mut() {
                                tail.following = at_end;
                            }
                        }
                        _ => {}
                    }
                },
            ),
            cx.observe_release(&editor, |this, _, _| {
                this.session_log_tail = None;
            }),
        ];

        self.session_log_tail = Some(SessionLogTail {
            following: true,
            _subscriptions: subscriptions,
        });
        editor.update(cx, |editor, cx| {
            editor.move_to_end(&editor::actions::MoveToEnd, window, cx);
        });
    }

    /// Whether the running log is stamping line-start times, or `None` when
    /// nothing is being recorded.
    fn session_log_timestamps(&self, cx: &App) -> Option<bool> {
        self.terminal
            .read(cx)
            .session_log_timestamps()
            .map(session_log::TimestampFlag::get)
    }

    /// Flips timestamps on the log that is already running. The writer reads
    /// the flag at the start of every line, so this takes effect immediately
    /// without splitting the file.
    fn toggle_session_log_timestamps(&mut self, cx: &mut Context<Self>) {
        let terminal = self.terminal.read(cx);
        let Some(timestamps) = terminal.session_log_timestamps() else {
            return;
        };
        timestamps.set(!timestamps.get());
        cx.notify();
    }

    /// Starts `terminal_macro` against this terminal, replacing any run that is
    /// already in flight.
    pub fn run_macro(&mut self, terminal_macro: TerminalMacro, cx: &mut Context<Self>) {
        let name = SharedString::from(terminal_macro.name.clone());
        let step_count = terminal_macro.steps.len();
        let iteration_count = terminal_macro.iteration_count();
        let view = cx.weak_entity();
        let run = terminal::terminal_macro::run_macro(
            self.terminal.clone(),
            terminal_macro,
            {
                move |progress, cx| {
                    // A closed terminal view simply has no progress to show.
                    view.update(cx, |this, cx| {
                        if let Some(run) = this.macro_run.as_mut() {
                            run.progress = progress;
                        }
                        cx.notify();
                    })
                    .ok();
                }
            },
            cx,
        );

        let task = cx.spawn({
            let name = name.clone();
            async move |this, cx| {
                let result = run.await;
                this.update(cx, |this, cx| {
                    this.macro_run = None;
                    this.macro_status = Some(match result {
                        Ok(()) => {
                            MacroStatus::Finished(format!("マクロ「{name}」が完了しました").into())
                        }
                        Err(error) => {
                            MacroStatus::Failed(format!("マクロ「{name}」: {error}").into())
                        }
                    });
                    cx.notify();
                })
                .ok();
            }
        });

        self.macro_status = None;
        self.macro_run = Some(MacroRun {
            name,
            progress: MacroProgress {
                step_index: 0,
                step_count,
                iteration: 1,
                iteration_count,
                nested: None,
                branch: None,
            },
            _task: task,
        });
        cx.notify();
    }

    fn cancel_macro(&mut self, _: &CancelMacro, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(run) = self.macro_run.take() {
            self.macro_status = Some(MacroStatus::Failed(
                format!("マクロ「{}」を中止しました", run.name).into(),
            ));
            cx.notify();
        }
    }

    /// Restarts a shell that is no longer running, keeping this view — and so
    /// the tab, its title and its position — in place. The scrollback of the
    /// dead shell is lost.
    fn restart_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.restart_task.is_some() {
            return;
        }
        let terminal = self.terminal.read(cx);
        // A task terminal reruns through `terminal::RerunTask`, and a live shell
        // has nothing to restart.
        if terminal.task().is_some() || !(terminal.remote_connection_lost() || terminal.exited()) {
            return;
        }

        // A remote shell has no client-side working directory (its cwd lives on
        // the host, and the local `ssh` process is not in it). It is tracked
        // separately when the shell reports it with OSC 7; otherwise fall back
        // to the directory a new terminal would open in, which is the one this
        // terminal was created with.
        let working_directory = terminal
            .working_directory()
            .or_else(|| terminal.remote_working_directory())
            .or_else(|| {
                let workspace = self.workspace.upgrade()?;
                let workspace = workspace.read(cx);
                default_working_directory(workspace, cx)
            });
        let is_remote_terminal = terminal.is_remote_terminal();
        let Some(project) = self.project.upgrade() else {
            return;
        };
        // Recording continues into the same file, so the restart shows up as a
        // gap in the session log rather than splitting it in two.
        let session_log_path = self.session_log_path(cx);
        let new_terminal = project.update(cx, |project, cx| {
            // A remote project can also hold local shells; those must not come
            // back as remote ones.
            if project.is_via_remote_server() && !is_remote_terminal {
                project.create_local_terminal(cx)
            } else {
                project.create_terminal_shell(working_directory, cx)
            }
        });

        self.restart_task = Some(cx.spawn_in(window, async move |this, cx| {
            let new_terminal = new_terminal.await;
            this.update_in(cx, |this, window, cx| {
                this.restart_task = None;
                match new_terminal {
                    Ok(new_terminal) => {
                        this.set_terminal(new_terminal, window, cx);
                        this.has_bell = false;
                        this.needs_serialize = true;
                        if let Some(path) = session_log_path {
                            let options = TerminalSettings::get_global(cx).session_log.options();
                            let started = this.terminal.update(cx, |terminal, cx| {
                                terminal.start_session_log(path, options, cx)
                            });
                            if let Err(error) = started {
                                this.report_error(
                                    format!("ログを再開できませんでした: {error}"),
                                    cx,
                                );
                            }
                        }
                        cx.emit(ItemEvent::UpdateTab);
                        cx.notify();
                    }
                    Err(error) => this
                        .report_error(format!("ターミナルを再起動できませんでした: {error}"), cx),
                }
            })
            .ok();
        }));
    }

    fn restart_terminal_action(
        &mut self,
        _: &RestartTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.restart_terminal(window, cx);
    }

    fn remote_connection_state_changed(
        &mut self,
        connection_state: ConnectionState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let connected = connection_state == ConnectionState::Connected;
        let reconnected = connected && !self.remote_connection_was_connected;
        self.remote_connection_was_connected = connected;
        if reconnected && self.terminal.read(cx).remote_connection_lost() {
            self.restart_terminal(window, cx);
        }
    }

    /// The strip above the terminal shown while a remote shell is waiting for
    /// its connection to come back.
    fn render_connection_lost_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.terminal.read(cx).remote_connection_lost() {
            return None;
        }

        let colors = cx.theme().colors();
        Some(
            h_flex()
                .w_full()
                .flex_none()
                .gap_2()
                .px_2()
                .py_0p5()
                .bg(colors.title_bar_background)
                .border_b_1()
                .border_color(colors.border_variant)
                .child(
                    Icon::new(IconName::Disconnected)
                        .size(IconSize::XSmall)
                        .color(Color::Error),
                )
                .child(Label::new("リモート接続が切れました").size(LabelSize::Small))
                .child(
                    Button::new("terminal-restart", "再起動")
                        .label_size(LabelSize::Small)
                        .tooltip(move |_window, cx| {
                            Tooltip::for_action(
                                "同じ作業ディレクトリでシェルを起動し直します",
                                &RestartTerminal,
                                cx,
                            )
                        })
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.restart_terminal(window, cx);
                        })),
                )
                .into_any_element(),
        )
    }

    fn report_error(&mut self, message: String, cx: &mut Context<Self>) {
        if self
            .workspace
            .update(cx, |workspace, cx| {
                workspace.show_error(message.clone(), cx)
            })
            .is_err()
        {
            log::error!("{message}");
        }
    }

    /// The strip above the terminal that shows the recording indicator and the
    /// progress of a running macro. Absent when neither applies.
    fn render_session_status_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let log_path = self.session_log_path(cx);
        let broadcasting = self.is_broadcasting_input(cx);
        if log_path.is_none()
            && !broadcasting
            && self.macro_run.is_none()
            && self.macro_status.is_none()
        {
            return None;
        }

        let colors = cx.theme().colors();
        let mut bar = h_flex()
            .w_full()
            .flex_none()
            .gap_2()
            .px_2()
            .py_0p5()
            .bg(colors.title_bar_background)
            .border_b_1()
            .border_color(colors.border_variant);

        if broadcasting {
            bar = bar.child(
                h_flex()
                    .gap_1()
                    .child(
                        Icon::new(IconName::Keyboard)
                            .size(IconSize::XSmall)
                            .color(Color::Accent),
                    )
                    .child(
                        Label::new("同時入力中")
                            .size(LabelSize::Small)
                            .color(Color::Accent),
                    ),
            );
            if log_path.is_some() {
                bar = bar.child(Divider::vertical());
            }
        }

        if let Some(path) = log_path {
            let display_path = path.display().to_string();
            let file_name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| display_path.clone());
            bar = bar
                .child(
                    Icon::new(IconName::Circle)
                        .size(IconSize::XSmall)
                        .color(Color::Error),
                )
                .child(
                    Button::new("session-log-path", format!("記録中: {file_name}"))
                        .label_size(LabelSize::Small)
                        .tooltip(Tooltip::text(format!(
                            "{display_path}\nクリックするとエディターで開き、末尾に追従します。"
                        )))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.tail_session_log(&TailSessionLog, window, cx);
                        })),
                )
                .child(
                    Checkbox::new(
                        "session-log-timestamps",
                        self.session_log_timestamps(cx).unwrap_or(false).into(),
                    )
                    .label("タイムスタンプ")
                    .label_size(LabelSize::Small)
                    .tooltip(Tooltip::text(
                        "記録中のログの行頭に時刻を付けます。次の行から反映されます。",
                    ))
                    .on_click(cx.listener(
                        move |this, _: &ToggleState, _, cx| {
                            this.toggle_session_log_timestamps(cx);
                        },
                    )),
                )
                .child(
                    Button::new("session-log-stop", "記録を停止")
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.stop_session_log(&StopSessionLog, window, cx);
                        })),
                );
        }

        if let Some(run) = self.macro_run.as_ref() {
            bar = bar
                .child(Divider::vertical())
                .child(
                    Label::new(macro_progress_label(&run.name, run.progress))
                        .size(LabelSize::Small),
                )
                .child(
                    Button::new("macro-cancel", "中止")
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.cancel_macro(&CancelMacro, window, cx);
                        })),
                );
        } else if let Some(status) = self.macro_status.as_ref() {
            let (message, color) = match status {
                MacroStatus::Finished(message) => (message.clone(), Color::Success),
                MacroStatus::Failed(message) => (message.clone(), Color::Error),
            };
            bar = bar
                .child(Label::new(message).size(LabelSize::Small).color(color))
                .child(
                    Button::new("macro-status-dismiss", "閉じる")
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.macro_status = None;
                            cx.notify();
                        })),
                );
        }

        Some(bar.into_any_element())
    }

    pub fn deploy_context_menu(
        &mut self,
        position: GpuiPoint<Pixels>,
        has_selection: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let assistant_enabled = self
            .workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).panel::<TerminalPanel>(cx))
            .is_some_and(|terminal_panel| terminal_panel.read(cx).assistant_enabled());
        let recording = self.session_log_path(cx).is_some();
        let context_menu = ContextMenu::build(window, cx, |menu, _, _| {
            menu.context(self.focus_handle.clone())
                .when(self.shows_workspace_actions(), |menu| {
                    menu.action("新規ターミナル", Box::new(NewTerminal::default()))
                        .action(
                            "中央ペインに新規ターミナル",
                            Box::new(NewCenterTerminal::default()),
                        )
                        .separator()
                })
                .action("コピー", Box::new(Copy))
                .when(
                    !matches!(self.mode, TerminalMode::Embedded { .. }),
                    |menu| {
                        menu.action("貼り付け", Box::new(Paste))
                            .action("テキストとして貼り付け", Box::new(PasteText))
                    },
                )
                .action("すべて選択", Box::new(SelectAll))
                .when(
                    !matches!(self.mode, TerminalMode::Embedded { .. }),
                    |menu| menu.action("画面を消去", Box::new(Clear)),
                )
                .when(self.shows_workspace_actions(), |menu| {
                    menu.separator()
                        .when(!recording, |menu| {
                            menu.action(START_SESSION_LOG_LABEL, Box::new(ToggleSessionLog))
                                .action(CHOOSE_SESSION_LOG_PATH_LABEL, Box::new(StartSessionLog))
                        })
                        .when(recording, |menu| {
                            menu.action(STOP_SESSION_LOG_LABEL, Box::new(StopSessionLog))
                                .action("ログをエディターで開く", Box::new(TailSessionLog))
                                .action("ログファイルの場所を開く", Box::new(OpenSessionLog))
                        })
                        .action("マクロを実行…", Box::new(session_tools::RunMacro))
                        // ADR-F28. The handler lives in the `file_transfer`
                        // crate; dispatching the action keeps this crate free
                        // of a dependency on it.
                        .action(
                            "ファイル転送…",
                            Box::new(zed_actions::terminal::OpenFileTransfer::default()),
                        )
                })
                .when(
                    assistant_enabled && !matches!(self.mode, TerminalMode::Embedded { .. }),
                    |menu| {
                        menu.separator()
                            .action("インラインアシスト", Box::new(InlineAssist::default()))
                            .when(has_selection && self.shows_workspace_actions(), |menu| {
                                menu.action(
                                    "エージェントのスレッドに追加",
                                    Box::new(AddSelectionToThread),
                                )
                            })
                    },
                )
                .when(self.shows_workspace_actions(), |menu| {
                    menu.separator().action(
                        "ターミナルタブを閉じる",
                        Box::new(CloseActiveItem {
                            save_intent: None,
                            close_pinned: true,
                        }),
                    )
                })
        });

        window.focus(&context_menu.focus_handle(cx), cx);
        let subscription = cx.subscribe_in(
            &context_menu,
            window,
            |this, _, _: &DismissEvent, window, cx| {
                if this.context_menu.as_ref().is_some_and(|context_menu| {
                    context_menu.0.focus_handle(cx).contains_focused(window, cx)
                }) {
                    cx.focus_self(window);
                }
                this.context_menu.take();
                cx.notify();
            },
        );

        self.context_menu = Some((context_menu, position, subscription));
    }

    fn settings_changed(&mut self, cx: &mut Context<Self>) {
        let settings = TerminalSettings::get_global(cx);
        let breadcrumb_visibility_changed = self.show_breadcrumbs != settings.toolbar.breadcrumbs;
        self.show_breadcrumbs = settings.toolbar.breadcrumbs;

        let should_blink = match settings.blinking {
            TerminalBlink::Off => false,
            TerminalBlink::On => true,
            TerminalBlink::TerminalControlled => self.blinking_terminal_enabled,
        };
        let new_cursor_shape = settings.cursor_shape;
        let old_cursor_shape = self.cursor_shape;
        if old_cursor_shape != new_cursor_shape {
            self.cursor_shape = new_cursor_shape;
            self.terminal.update(cx, |term, _| {
                term.set_cursor_shape(self.cursor_shape);
            });
        }

        self.blink_manager.update(
            cx,
            if should_blink {
                BlinkManager::enable
            } else {
                BlinkManager::disable
            },
        );

        if breadcrumb_visibility_changed {
            cx.emit(ItemEvent::UpdateBreadcrumbs);
        }
        cx.notify();
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .terminal
            .read(cx)
            .last_content
            .mode
            .contains(Modes::ALT_SCREEN)
        {
            self.terminal.update(cx, |term, cx| {
                term.try_keystroke(
                    &Keystroke::parse("ctrl-cmd-space").unwrap(),
                    TerminalSettings::get_global(cx).option_as_meta,
                )
            });
        } else {
            window.show_character_palette();
        }
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.terminal.update(cx, |term, _| term.select_all());
        cx.notify();
    }

    fn rerun_task(&mut self, _: &RerunTask, window: &mut Window, cx: &mut Context<Self>) {
        let task = self
            .terminal
            .read(cx)
            .task()
            .map(|task| terminal_rerun_override(&task.spawned_task.id))
            .unwrap_or_default();
        window.dispatch_action(Box::new(task), cx);
    }

    fn clear(&mut self, _: &Clear, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll_top = px(0.);
        self.terminal.update(cx, |term, _| term.clear());
        cx.notify();
    }

    fn max_scroll_top(&self, cx: &App) -> Pixels {
        let terminal = self.terminal.read(cx);

        let Some(block) = self.block_below_cursor.as_ref() else {
            return Pixels::ZERO;
        };

        let line_height = terminal.last_content().terminal_bounds.line_height;
        let viewport_lines = terminal.viewport_lines();
        let cursor_line = viewport_line_for_point(
            terminal.last_content.cursor.point,
            terminal.last_content.display_offset,
        )
        .unwrap_or_default();
        let max_scroll_top_in_lines =
            (block.height as usize).saturating_sub(viewport_lines.saturating_sub(cursor_line + 1));

        max_scroll_top_in_lines as f32 * line_height
    }

    fn scroll_wheel(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let terminal_content = self.terminal.read(cx).last_content();

        if self.block_below_cursor.is_some() && terminal_content.display_offset == 0 {
            let line_height = terminal_content.terminal_bounds.line_height;
            let y_delta = event.delta.pixel_delta(line_height).y;
            if y_delta < Pixels::ZERO || self.scroll_top > Pixels::ZERO {
                self.scroll_top = cmp::max(
                    Pixels::ZERO,
                    cmp::min(self.scroll_top - y_delta, self.max_scroll_top(cx)),
                );
                cx.notify();
                return;
            }
        }
        self.terminal.update(cx, |term, cx| {
            term.scroll_wheel(
                event,
                TerminalSettings::get_global(cx).scroll_multiplier.max(0.01),
            )
        });
    }

    fn is_alt_screen(&self, cx: &App) -> bool {
        self.terminal
            .read(cx)
            .last_content
            .mode
            .contains(Modes::ALT_SCREEN)
    }

    fn scroll_line_up(&mut self, _: &ScrollLineUp, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_alt_screen(cx) {
            cx.propagate();
            return;
        }

        let terminal_content = self.terminal.read(cx).last_content();
        if self.block_below_cursor.is_some()
            && terminal_content.display_offset == 0
            && self.scroll_top > Pixels::ZERO
        {
            let line_height = terminal_content.terminal_bounds.line_height;
            self.scroll_top = cmp::max(self.scroll_top - line_height, Pixels::ZERO);
            return;
        }

        self.terminal.update(cx, |term, _| term.scroll_line_up());
        cx.notify();
    }

    fn scroll_line_down(&mut self, _: &ScrollLineDown, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_alt_screen(cx) {
            cx.propagate();
            return;
        }

        let terminal_content = self.terminal.read(cx).last_content();
        if self.block_below_cursor.is_some() && terminal_content.display_offset == 0 {
            let max_scroll_top = self.max_scroll_top(cx);
            if self.scroll_top < max_scroll_top {
                let line_height = terminal_content.terminal_bounds.line_height;
                self.scroll_top = cmp::min(self.scroll_top + line_height, max_scroll_top);
            }
            return;
        }

        self.terminal.update(cx, |term, _| term.scroll_line_down());
        cx.notify();
    }

    fn scroll_page_up(&mut self, _: &ScrollPageUp, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_alt_screen(cx) {
            cx.propagate();
            return;
        }

        if self.scroll_top == Pixels::ZERO {
            self.terminal.update(cx, |term, _| term.scroll_page_up());
        } else {
            let line_height = self
                .terminal
                .read(cx)
                .last_content
                .terminal_bounds
                .line_height();
            let visible_block_lines = (self.scroll_top / line_height) as usize;
            let viewport_lines = self.terminal.read(cx).viewport_lines();
            let visible_content_lines = viewport_lines - visible_block_lines;

            if visible_block_lines >= viewport_lines {
                self.scroll_top = ((visible_block_lines - viewport_lines) as f32) * line_height;
            } else {
                self.scroll_top = px(0.);
                self.terminal
                    .update(cx, |term, _| term.scroll_up_by(visible_content_lines));
            }
        }
        cx.notify();
    }

    fn scroll_page_down(&mut self, _: &ScrollPageDown, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_alt_screen(cx) {
            cx.propagate();
            return;
        }

        self.terminal.update(cx, |term, _| term.scroll_page_down());
        let terminal = self.terminal.read(cx);
        if terminal.last_content().display_offset < terminal.viewport_lines() {
            self.scroll_top = self.max_scroll_top(cx);
        }
        cx.notify();
    }

    fn scroll_to_top(&mut self, _: &ScrollToTop, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_alt_screen(cx) {
            cx.propagate();
            return;
        }

        self.terminal.update(cx, |term, _| term.scroll_to_top());
        cx.notify();
    }

    fn scroll_to_bottom(&mut self, _: &ScrollToBottom, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_alt_screen(cx) {
            cx.propagate();
            return;
        }

        self.terminal.update(cx, |term, _| term.scroll_to_bottom());
        if self.block_below_cursor.is_some() {
            self.scroll_top = self.max_scroll_top(cx);
        }
        cx.notify();
    }

    fn toggle_vi_mode(&mut self, _: &ToggleViMode, _: &mut Window, cx: &mut Context<Self>) {
        self.terminal.update(cx, |term, _| term.toggle_vi_mode());
        cx.notify();
    }

    pub fn should_show_cursor(&self, focused: bool, cx: &mut Context<Self>) -> bool {
        // Hide cursor when in embedded mode and not focused (read-only output like Agent panel)
        if let TerminalMode::Embedded { .. } = &self.mode {
            if !focused {
                return false;
            }
        }

        // For Standalone mode: always show cursor when not focused or in special modes
        if !focused
            || self
                .terminal
                .read(cx)
                .last_content
                .mode
                .contains(Modes::ALT_SCREEN)
        {
            return true;
        }

        // When focused, check blinking settings and blink manager state
        match TerminalSettings::get_global(cx).blinking {
            TerminalBlink::Off => true,
            TerminalBlink::TerminalControlled => {
                !self.blinking_terminal_enabled || self.blink_manager.read(cx).visible()
            }
            TerminalBlink::On => self.blink_manager.read(cx).visible(),
        }
    }

    pub fn pause_cursor_blinking(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.blink_manager.update(cx, BlinkManager::pause_blinking);
    }

    pub fn terminal(&self) -> &Entity<Terminal> {
        &self.terminal
    }

    pub fn set_block_below_cursor(
        &mut self,
        block: BlockProperties,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.block_below_cursor = Some(Rc::new(block));
        self.scroll_to_bottom(&ScrollToBottom, window, cx);
        cx.notify();
    }

    pub fn clear_block_below_cursor(&mut self, cx: &mut Context<Self>) {
        self.block_below_cursor = None;
        self.scroll_top = Pixels::ZERO;
        cx.notify();
    }

    ///Attempt to paste the clipboard into the terminal
    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        self.terminal.update(cx, |term, _| term.copy(None));
        cx.notify();
    }

    /// Specific handler for the [`editor::actions::Copy`] action in order for
    /// the `Edit > Copy` menu item to not be disabled, as the app expects a
    /// handler for this action in order to enable/disable the menu item.
    fn editor_copy(
        &mut self,
        _: &editor::actions::Copy,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.copy(&Copy, window, cx);
    }

    ///Attempt to paste the clipboard into the terminal
    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        let Some(clipboard) = cx.read_from_clipboard() else {
            return;
        };

        match clipboard.entries().first() {
            Some(ClipboardEntry::Image(image)) if !image.bytes.is_empty() => {
                self.forward_ctrl_v(cx);
            }
            Some(ClipboardEntry::ExternalPaths(paths)) => {
                self.add_paths_to_terminal(paths.paths(), window, cx);
            }
            _ => {
                if let Some(text) = clipboard.text() {
                    self.terminal
                        .update(cx, |terminal, _cx| terminal.paste(&text));
                    self.broadcast_input(cx, |terminal, cx| {
                        terminal.update(cx, |terminal, _| terminal.paste(&text));
                    });
                }
            }
        }
    }

    /// Specific handler for the [`editor::actions::Paste`] action in order for
    /// the `Edit > Paste` menu item to not be disabled, as the app expects a
    /// handler for this action in order to enable/disable the menu item.
    fn editor_paste(
        &mut self,
        _: &editor::actions::Paste,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.paste(&Paste, window, cx);
    }

    ///Attempt to paste the clipboard text into the terminal
    fn paste_text(&mut self, _: &PasteText, _: &mut Window, cx: &mut Context<Self>) {
        let Some(clipboard) = cx.read_from_clipboard() else {
            return;
        };

        if let Some(text) = clipboard.text() {
            self.terminal
                .update(cx, |terminal, _cx| terminal.paste(&text));
            self.broadcast_input(cx, |terminal, cx| {
                terminal.update(cx, |terminal, _| terminal.paste(&text));
            });
        }
    }

    /// Emits a raw Ctrl+V so TUI agents can read the OS clipboard directly
    /// and attach images using their native workflows.
    fn forward_ctrl_v(&self, cx: &mut Context<Self>) {
        self.terminal.update(cx, |term, _| {
            term.input(vec![0x16]);
        });
    }

    pub fn add_paths_to_terminal(&self, paths: &[PathBuf], window: &mut Window, cx: &mut App) {
        let mut text = paths
            .iter()
            .filter_map(|path| Some(format!(" {}", shlex::try_quote(path.to_str()?).ok()?)))
            .collect::<String>();
        text.push(' ');
        window.focus(&self.focus_handle(cx), cx);
        self.terminal.update(cx, |terminal, _| {
            terminal.paste(&text);
        });
    }

    fn send_text(&mut self, text: &SendText, _: &mut Window, cx: &mut Context<Self>) {
        self.clear_bell(cx);
        self.blink_manager.update(cx, BlinkManager::pause_blinking);
        self.terminal.update(cx, |term, _| {
            term.input(text.0.to_string().into_bytes());
        });
    }

    fn send_keystroke(&mut self, text: &SendKeystroke, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(keystroke) = Keystroke::parse(&text.0).log_err() {
            self.clear_bell(cx);
            self.blink_manager.update(cx, BlinkManager::pause_blinking);
            self.process_keystroke(&keystroke, cx);
        }
    }

    fn dispatch_context(&self, cx: &App) -> KeyContext {
        let mut dispatch_context = KeyContext::new_with_defaults();
        dispatch_context.add("Terminal");

        if self.terminal.read(cx).vi_mode_enabled() {
            dispatch_context.add("vi_mode");
        }

        let mode = self.terminal.read(cx).last_content.mode;
        dispatch_context.set(
            "screen",
            if mode.contains(Modes::ALT_SCREEN) {
                "alt"
            } else {
                "normal"
            },
        );

        if mode.contains(Modes::APP_CURSOR) {
            dispatch_context.add("DECCKM");
        }
        if mode.contains(Modes::APP_KEYPAD) {
            dispatch_context.add("DECPAM");
        } else {
            dispatch_context.add("DECPNM");
        }
        if mode.contains(Modes::SHOW_CURSOR) {
            dispatch_context.add("DECTCEM");
        }
        if mode.contains(Modes::LINE_WRAP) {
            dispatch_context.add("DECAWM");
        }
        if mode.contains(Modes::ORIGIN) {
            dispatch_context.add("DECOM");
        }
        if mode.contains(Modes::INSERT) {
            dispatch_context.add("IRM");
        }
        //LNM is apparently the name for this. https://vt100.net/docs/vt510-rm/LNM.html
        if mode.contains(Modes::LINE_FEED_NEW_LINE) {
            dispatch_context.add("LNM");
        }
        if mode.contains(Modes::FOCUS_IN_OUT) {
            dispatch_context.add("report_focus");
        }
        if mode.contains(Modes::ALTERNATE_SCROLL) {
            dispatch_context.add("alternate_scroll");
        }
        if mode.contains(Modes::BRACKETED_PASTE) {
            dispatch_context.add("bracketed_paste");
        }
        if mode.intersects(Modes::MOUSE_MODE) {
            dispatch_context.add("any_mouse_reporting");
        }
        {
            let mouse_reporting = if mode.contains(Modes::MOUSE_REPORT_CLICK) {
                "click"
            } else if mode.contains(Modes::MOUSE_DRAG) {
                "drag"
            } else if mode.contains(Modes::MOUSE_MOTION) {
                "motion"
            } else {
                "off"
            };
            dispatch_context.set("mouse_reporting", mouse_reporting);
        }
        {
            let format = if mode.contains(Modes::SGR_MOUSE) {
                "sgr"
            } else if mode.contains(Modes::UTF8_MOUSE) {
                "utf8"
            } else {
                "normal"
            };
            dispatch_context.set("mouse_format", format);
        };

        if self.terminal.read(cx).last_content.selection.is_some() {
            dispatch_context.add("selection");
        }

        dispatch_context
    }

    fn set_terminal(
        &mut self,
        terminal: Entity<Terminal>,
        window: &mut Window,
        cx: &mut Context<TerminalView>,
    ) {
        self._terminal_subscriptions =
            subscribe_for_terminal_events(&terminal, self.workspace.clone(), window, cx);
        self.terminal = terminal;
    }

    fn rerun_button(task: &TaskState) -> Option<IconButton> {
        if !task.spawned_task.show_rerun {
            return None;
        }

        let task_id = task.spawned_task.id.clone();
        Some(
            IconButton::new("rerun-icon", IconName::Rerun)
                .icon_size(IconSize::Small)
                .size(ButtonSize::Compact)
                .icon_color(Color::Default)
                .shape(ui::IconButtonShape::Square)
                .tooltip(move |_window, cx| Tooltip::for_action("タスクを再実行", &RerunTask, cx))
                .on_click(move |_, window, cx| {
                    window.dispatch_action(Box::new(terminal_rerun_override(&task_id)), cx);
                }),
        )
    }
}

fn terminal_rerun_override(task: &TaskId) -> zed_actions::Rerun {
    zed_actions::Rerun {
        task_id: Some(task.0.clone()),
        allow_concurrent_runs: Some(true),
        use_new_terminal: Some(false),
        reevaluate_context: false,
    }
}

fn subscribe_for_terminal_events(
    terminal: &Entity<Terminal>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut Context<TerminalView>,
) -> Vec<Subscription> {
    let terminal_subscription = cx.observe(terminal, |_, _, cx| cx.notify());
    let mut previous_cwd = None;
    let mut previous_remote_cwd = None;
    let terminal_events_subscription = cx.subscribe_in(
        terminal,
        window,
        move |terminal_view, terminal, event, window, cx| {
            let current_cwd = terminal.read(cx).working_directory();
            if current_cwd != previous_cwd {
                previous_cwd = current_cwd;
                terminal_view.needs_serialize = true;
            }
            // A remote shell's directory arrives as PTY output (OSC 7) rather
            // than as an event of its own, so it is polled here beside the
            // client-side one.
            let current_remote_cwd = terminal.read(cx).remote_working_directory();
            if current_remote_cwd != previous_remote_cwd {
                previous_remote_cwd = current_remote_cwd;
                terminal_view.needs_serialize = true;
            }

            match event {
                Event::Wakeup => {
                    cx.notify();
                    window.invalidate_character_coordinates();
                    cx.emit(Event::Wakeup);
                    cx.emit(ItemEvent::UpdateTab);
                    cx.emit(SearchEvent::MatchesInvalidated);
                }

                Event::Bell => {
                    terminal_view.has_bell = true;
                    if let TerminalBell::System = TerminalSettings::get_global(cx).bell {
                        window.play_system_bell();
                    }
                    cx.emit(Event::Wakeup);
                }

                Event::BlinkChanged(blinking) => {
                    terminal_view.blinking_terminal_enabled = *blinking;

                    // If in terminal-controlled mode and focused, update blink manager
                    if matches!(
                        TerminalSettings::get_global(cx).blinking,
                        TerminalBlink::TerminalControlled
                    ) && terminal_view.focus_handle.is_focused(window)
                    {
                        terminal_view.blink_manager.update(cx, |manager, cx| {
                            if *blinking {
                                manager.enable(cx);
                            } else {
                                manager.disable(cx);
                            }
                        });
                    }
                }

                Event::TitleChanged => {
                    cx.emit(ItemEvent::UpdateTab);
                }

                Event::NewNavigationTarget(maybe_navigation_target) => {
                    match maybe_navigation_target
                        .as_ref()
                        .zip(terminal.read(cx).last_content.last_hovered_word.as_ref())
                    {
                        Some((MaybeNavigationTarget::Url(url), hovered_word)) => {
                            if Some(hovered_word)
                                != terminal_view
                                    .hover
                                    .as_ref()
                                    .map(|hover| &hover.hovered_word)
                            {
                                terminal_view.hover = Some(HoverTarget {
                                    tooltip: url.clone(),
                                    hovered_word: hovered_word.clone(),
                                });
                                terminal_view.hover_tooltip_update = Task::ready(());
                                cx.notify();
                            }
                        }
                        Some((MaybeNavigationTarget::PathLike(path_like_target), hovered_word)) => {
                            if Some(hovered_word)
                                != terminal_view
                                    .hover
                                    .as_ref()
                                    .map(|hover| &hover.hovered_word)
                            {
                                terminal_view.hover = None;
                                terminal_view.hover_tooltip_update = hover_path_like_target(
                                    &workspace,
                                    hovered_word.clone(),
                                    path_like_target,
                                    cx,
                                );
                                cx.notify();
                            }
                        }
                        None => {
                            terminal_view.hover = None;
                            terminal_view.hover_tooltip_update = Task::ready(());
                            cx.notify();
                        }
                    }
                }

                Event::Open(maybe_navigation_target) => match maybe_navigation_target {
                    MaybeNavigationTarget::Url(url) => cx.open_url(url),
                    MaybeNavigationTarget::PathLike(path_like_target) => open_path_like_target(
                        &workspace,
                        terminal_view,
                        path_like_target,
                        window,
                        cx,
                    ),
                },
                Event::BreadcrumbsChanged => cx.emit(ItemEvent::UpdateBreadcrumbs),
                // The tab's recording dot and the tab bar's recording toggle are
                // drawn by the pane, which only redraws for `UpdateTab`.
                Event::SessionLogChanged => {
                    cx.notify();
                    cx.emit(ItemEvent::UpdateTab);
                }
                Event::RemoteConnectionLost => {
                    cx.notify();
                    cx.emit(ItemEvent::UpdateTab);
                }
                Event::CloseTerminal => cx.emit(ItemEvent::CloseItem),
                Event::SelectionsChanged => {
                    window.invalidate_character_coordinates();
                    cx.emit(SearchEvent::ActiveMatchChanged)
                }
            }
        },
    );
    vec![terminal_subscription, terminal_events_subscription]
}

fn regex_search_for_query(query: &SearchQuery) -> Option<Search> {
    let str = query.as_str();
    if query.is_regex() {
        if str == "." {
            return None;
        }
        Search::new(str)
    } else {
        Search::new(&regex::escape(str))
    }
}

#[derive(Default)]
struct TerminalScrollbarSettingsWrapper;

impl ScrollbarVisibility for TerminalScrollbarSettingsWrapper {
    fn visibility(&self, cx: &App) -> scrollbars::ShowScrollbar {
        TerminalSettings::get_global(cx)
            .scrollbar
            .show
            .map(ui_scrollbar_settings_from_raw)
            .unwrap_or_else(|| EditorSettings::get_global(cx).scrollbar.show)
    }
}

impl TerminalView {
    /// Attempts to process a keystroke in the terminal. Returns true if handled.
    ///
    /// In vi mode, explicitly triggers a re-render because vi navigation (like j/k)
    /// updates the cursor locally without sending data to the shell, so there's no
    /// shell output to automatically trigger a re-render.
    fn process_keystroke(&mut self, keystroke: &Keystroke, cx: &mut Context<Self>) -> bool {
        let (handled, vi_mode_enabled) = self.terminal.update(cx, |term, cx| {
            (
                term.try_keystroke(keystroke, TerminalSettings::get_global(cx).option_as_meta),
                term.vi_mode_enabled(),
            )
        });

        if handled && vi_mode_enabled {
            cx.notify();
        }

        // A vi-mode key moves this terminal's own cursor and never reaches the
        // pty, so mirroring it would type into the other shells instead.
        if handled && !vi_mode_enabled {
            let option_as_meta = TerminalSettings::get_global(cx).option_as_meta;
            let keystroke = keystroke.clone();
            self.broadcast_input(cx, move |terminal, cx| {
                terminal.update(cx, |terminal, _| {
                    terminal.try_keystroke(&keystroke, option_as_meta);
                });
            });
        }

        handled
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_bell(cx);
        self.pause_cursor_blinking(window, cx);

        if self.process_keystroke(&event.keystroke, cx) {
            cx.stop_propagation();
        }
    }

    fn build_window_focus_subscriptions(
        focus_handle: &FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Subscription> {
        vec![
            cx.on_focus_in(focus_handle, window, |terminal_view, window, cx| {
                terminal_view.focus_in(window, cx);
            }),
            cx.on_focus_out(focus_handle, window, |terminal_view, _event, window, cx| {
                terminal_view.focus_out(window, cx);
            }),
        ]
    }

    /// Points the focus listeners at `window`, which they do not follow on
    /// their own: they are stored on the `Window` that created them. Without
    /// this a terminal moved to another window never tells the PTY it gained
    /// or lost focus, so its cursor and focus reporting freeze.
    fn rebind_window_focus_listeners(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let window_id = window.window_handle().window_id();
        if self.focus_listener_window == window_id {
            return;
        }

        let focus_handle = self.focus_handle.clone();
        self._window_focus_subscriptions =
            Self::build_window_focus_subscriptions(&focus_handle, window, cx);
        self.focus_listener_window = window_id;
    }

    fn focus_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, _| {
            terminal.set_cursor_shape(self.cursor_shape);
            terminal.focus_in();
        });

        let should_blink = match TerminalSettings::get_global(cx).blinking {
            TerminalBlink::Off => false,
            TerminalBlink::On => true,
            TerminalBlink::TerminalControlled => self.blinking_terminal_enabled,
        };

        if should_blink {
            self.blink_manager.update(cx, BlinkManager::enable);
        }

        window.invalidate_character_coordinates();
        cx.notify();
    }

    fn focus_out(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.blink_manager.update(cx, BlinkManager::disable);
        self.terminal.update(cx, |terminal, _| {
            terminal.focus_out();
            terminal.set_cursor_shape(CursorShape::Hollow);
        });
        cx.notify();
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // TODO: this should be moved out of render
        self.scroll_handle.update(self.terminal.read(cx));

        if let Some(new_display_offset) = self.scroll_handle.future_display_offset.take() {
            self.terminal.update(cx, |term, _| {
                let delta = new_display_offset as i32 - term.last_content.display_offset as i32;
                match delta.cmp(&0) {
                    cmp::Ordering::Greater => term.scroll_up_by(delta as usize),
                    cmp::Ordering::Less => term.scroll_down_by(-delta as usize),
                    cmp::Ordering::Equal => {}
                }
            });
        }

        let terminal_handle = self.terminal.clone();
        let terminal_view_handle = cx.entity();

        let focused = self.focus_handle.is_focused(window);

        div()
            .id("terminal-view")
            .size_full()
            .relative()
            .track_focus(&self.focus_handle(cx))
            .key_context(self.dispatch_context(cx))
            .on_action(cx.listener(TerminalView::send_text))
            .on_action(cx.listener(TerminalView::send_keystroke))
            .on_action(cx.listener(TerminalView::copy))
            .on_action(cx.listener(TerminalView::editor_copy))
            .on_action(cx.listener(TerminalView::paste))
            .on_action(cx.listener(TerminalView::editor_paste))
            .on_action(cx.listener(TerminalView::paste_text))
            .on_action(cx.listener(TerminalView::clear))
            .on_action(cx.listener(TerminalView::scroll_line_up))
            .on_action(cx.listener(TerminalView::scroll_line_down))
            .on_action(cx.listener(TerminalView::scroll_page_up))
            .on_action(cx.listener(TerminalView::scroll_page_down))
            .on_action(cx.listener(TerminalView::scroll_to_top))
            .on_action(cx.listener(TerminalView::scroll_to_bottom))
            .on_action(cx.listener(TerminalView::toggle_vi_mode))
            .on_action(cx.listener(TerminalView::show_character_palette))
            .on_action(cx.listener(TerminalView::select_all))
            .on_action(cx.listener(TerminalView::rerun_task))
            .on_action(cx.listener(TerminalView::rename_terminal))
            .on_action(cx.listener(TerminalView::toggle_session_log_action))
            .on_action(cx.listener(TerminalView::start_session_log))
            .on_action(cx.listener(TerminalView::stop_session_log))
            .on_action(cx.listener(TerminalView::open_session_log))
            .on_action(cx.listener(TerminalView::tail_session_log))
            .on_action(cx.listener(TerminalView::cancel_macro))
            .on_action(cx.listener(TerminalView::restart_terminal_action))
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    if !this.terminal.read(cx).mouse_mode(event.modifiers.shift) {
                        let had_selection = this.terminal.read(cx).last_content.selection.is_some();
                        if !had_selection {
                            this.terminal.update(cx, |terminal, _| {
                                terminal.select_word_at_event_position(event);
                            });
                        }
                        let has_selection = !had_selection
                            || this
                                .terminal
                                .read(cx)
                                .last_content
                                .selection_text
                                .as_ref()
                                .is_some_and(|text| !text.is_empty());
                        this.deploy_context_menu(event.position, has_selection, window, cx);
                        cx.notify();
                    }
                }),
            )
            .child(
                v_flex()
                    .size_full()
                    .children(self.render_connection_lost_banner(cx))
                    .children(self.render_session_status_bar(cx))
                    .child(
                        // TODO: Oddly this wrapper div is needed for TerminalElement to not steal events from the context menu
                        div()
                            .id("terminal-view-container")
                            .w_full()
                            .flex_1()
                            .min_h_0()
                            .bg(cx.theme().colors().editor_background)
                            .child(TerminalElement::new(
                                terminal_handle,
                                terminal_view_handle,
                                self.workspace.clone(),
                                self.focus_handle.clone(),
                                focused,
                                self.should_show_cursor(focused, cx),
                                self.block_below_cursor.clone(),
                                self.mode.clone(),
                            ))
                            .when(self.content_mode(window, cx).is_scrollable(), |div| {
                                let colors = cx.theme().colors();
                                div.custom_scrollbars(
                                    Scrollbars::for_settings::<TerminalScrollbarSettingsWrapper>()
                                        .show_along(ScrollAxes::Vertical)
                                        .with_stable_track_along(
                                            ScrollAxes::Vertical,
                                            colors.editor_background,
                                        )
                                        .tracked_scroll_handle(&self.scroll_handle),
                                    window,
                                    cx,
                                )
                            }),
                    ),
            )
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .anchor(gpui::Anchor::TopLeft)
                        .child(menu.clone()),
                )
                .with_priority(1)
            }))
    }
}

impl Item for TerminalView {
    type Event = ItemEvent;

    fn tab_tooltip_content(&self, cx: &App) -> Option<TabTooltipContent> {
        Some(TabTooltipContent::Custom(Box::new(Tooltip::element({
            let terminal = self.terminal().read(cx);
            let title = terminal.title(false);
            let pid = terminal.pid_getter()?.fallback_pid();

            move |_, _| {
                v_flex()
                    .gap_1()
                    .child(Label::new(title.clone()))
                    .child(h_flex().flex_grow_1().child(Divider::horizontal()))
                    .child(
                        Label::new(format!("プロセス ID (PID): {}", pid))
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    )
                    .into_any_element()
            }
        }))))
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        let terminal = self.terminal().read(cx);
        let title = self
            .custom_title
            .as_ref()
            .filter(|title| !title.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| terminal.title(true));

        let (icon, icon_color, rerun_button) = match terminal.task() {
            Some(terminal_task) => match &terminal_task.status {
                TaskStatus::Running => (
                    IconName::PlayFilled,
                    Color::Disabled,
                    TerminalView::rerun_button(terminal_task),
                ),
                TaskStatus::Unknown => (
                    IconName::Warning,
                    Color::Warning,
                    TerminalView::rerun_button(terminal_task),
                ),
                TaskStatus::Completed { success } => {
                    let rerun_button = TerminalView::rerun_button(terminal_task);

                    if *success {
                        (IconName::Check, Color::Success, rerun_button)
                    } else {
                        (IconName::XCircle, Color::Error, rerun_button)
                    }
                }
            },
            None => (IconName::Terminal, Color::Muted, None),
        };

        let self_handle = self.self_handle.clone();
        h_flex()
            .gap_1()
            .group("term-tab-icon")
            .when(!params.selected, |this| {
                this.track_focus(&self.focus_handle)
            })
            .on_action(move |action: &RenameTerminal, window, cx| {
                self_handle
                    .update(cx, |this, cx| this.rename_terminal(action, window, cx))
                    .ok();
            })
            .child(
                h_flex()
                    .group("term-tab-icon")
                    .child(
                        div()
                            .when(rerun_button.is_some(), |this| {
                                this.hover(|style| style.invisible().w_0())
                            })
                            .child(Icon::new(icon).color(icon_color)),
                    )
                    .when_some(rerun_button, |this, rerun_button| {
                        this.child(
                            div()
                                .absolute()
                                .visible_on_hover("term-tab-icon")
                                .child(rerun_button),
                        )
                    }),
            )
            .child(
                div()
                    .relative()
                    .child(
                        Label::new(title)
                            .color(params.text_color())
                            .when(self.is_renaming(), |this| this.alpha(0.)),
                    )
                    .when_some(self.rename_editor.clone(), |this, editor| {
                        let self_handle = self.self_handle.clone();
                        let self_handle_cancel = self.self_handle.clone();
                        this.child(
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .size_full()
                                .child(editor)
                                .on_action(move |_: &menu::Confirm, window, cx| {
                                    self_handle
                                        .update(cx, |this, cx| {
                                            this.finish_renaming(true, window, cx)
                                        })
                                        .ok();
                                })
                                .on_action(move |_: &menu::Cancel, window, cx| {
                                    self_handle_cancel
                                        .update(cx, |this, cx| {
                                            this.finish_renaming(false, window, cx)
                                        })
                                        .ok();
                                }),
                        )
                    }),
            )
            .when(terminal.session_log_path().is_some(), |this| {
                this.child(
                    Icon::new(IconName::Circle)
                        .size(IconSize::XSmall)
                        .color(Color::Error),
                )
            })
            .into_any()
    }

    fn tab_content_text(&self, detail: usize, cx: &App) -> SharedString {
        if let Some(custom_title) = self.custom_title.as_ref().filter(|l| !l.trim().is_empty()) {
            return custom_title.clone().into();
        }
        let terminal = self.terminal().read(cx);
        terminal.title(detail == 0).into()
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn handle_drop(
        &self,
        active_pane: &Pane,
        dropped: &dyn Any,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let Some(project) = self.project.upgrade() else {
            return false;
        };

        if let Some(paths) = dropped.downcast_ref::<ExternalPaths>() {
            let is_local = project.read(cx).is_local();
            if is_local {
                self.add_paths_to_terminal(paths.paths(), window, cx);
                return true;
            }

            return false;
        } else if let Some(tab) = dropped.downcast_ref::<DraggedTab>() {
            let Some(self_handle) = self.self_handle.upgrade() else {
                return false;
            };

            let Some(workspace) = self.workspace.upgrade() else {
                return false;
            };

            let Some(this_pane) = workspace.read(cx).pane_for(&self_handle) else {
                return false;
            };

            let item = if tab.pane == this_pane {
                active_pane.item_for_index(tab.ix)
            } else {
                tab.pane.read(cx).item_for_index(tab.ix)
            };

            let Some(item) = item else {
                return false;
            };

            // ターミナルのタブをペインの端に落としたときの分割は
            // `Pane::set_tab_drop_split_handler` (ターミナルパネルが登録) が引き受ける
            // ので、ここでは横取りしない。エディターのタブを落としたときだけは
            // 「移動」ではなく「パスを打ち込む」が期待どおりの動きなので残す。
            if item.downcast::<TerminalView>().is_none()
                && let Some(project_path) = item.project_path(cx)
                && let Some(path) = project.read(cx).absolute_path(&project_path, cx)
            {
                self.add_paths_to_terminal(&[path], window, cx);
                return true;
            }

            return false;
        } else if let Some(selection) = dropped.downcast_ref::<DraggedSelection>() {
            let project = project.read(cx);
            let paths = selection
                .items()
                .map(|selected_entry| selected_entry.entry_id)
                .filter_map(|entry_id| project.path_for_entry(entry_id, cx))
                .filter_map(|project_path| project.absolute_path(&project_path, cx))
                .collect::<Vec<_>>();

            if !paths.is_empty() {
                self.add_paths_to_terminal(&paths, window, cx);
            }

            return true;
        } else if let Some(&entry_id) = dropped.downcast_ref::<ProjectEntryId>() {
            let project = project.read(cx);
            if let Some(path) = project
                .path_for_entry(entry_id, cx)
                .and_then(|project_path| project.absolute_path(&project_path, cx))
            {
                self.add_paths_to_terminal(&[path], window, cx);
            }

            return true;
        }

        false
    }

    fn tab_extra_context_menu_actions(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<(SharedString, Box<dyn gpui::Action>)> {
        let terminal = self.terminal.read(cx);
        // NOTE: `workspace::pane` compares the "名前の変更" label to decide
        // where to place this entry; leave that string alone.
        let mut actions: Vec<(SharedString, Box<dyn gpui::Action>)> = Vec::new();
        if terminal.task().is_none() {
            actions.push(("名前の変更".into(), Box::new(RenameTerminal)));
        }
        if terminal.session_log_path().is_some() {
            actions.push((STOP_SESSION_LOG_LABEL.into(), Box::new(StopSessionLog)));
            actions.push(("ログをエディターで開く".into(), Box::new(TailSessionLog)));
            actions.push(("ログファイルの場所を開く".into(), Box::new(OpenSessionLog)));
        } else {
            actions.push((START_SESSION_LOG_LABEL.into(), Box::new(ToggleSessionLog)));
            actions.push((
                CHOOSE_SESSION_LOG_PATH_LABEL.into(),
                Box::new(StartSessionLog),
            ));
        }
        actions.push(("マクロを実行…".into(), Box::new(session_tools::RunMacro)));
        actions
    }

    fn buffer_kind(&self, _: &App) -> workspace::item::ItemBufferKind {
        workspace::item::ItemBufferKind::Singleton
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        workspace_id: Option<WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>> {
        let Ok(terminal) = self.project.update(cx, |project, cx| {
            let cwd = project
                .active_project_directory(cx)
                .map(|it| it.to_path_buf());
            project.clone_terminal(self.terminal(), cx, cwd)
        }) else {
            return Task::ready(None);
        };
        cx.spawn_in(window, async move |this, cx| {
            let terminal = terminal.await.log_err()?;
            this.update_in(cx, |this, window, cx| {
                cx.new(|cx| {
                    TerminalView::new(
                        terminal,
                        this.workspace.clone(),
                        workspace_id,
                        this.project.clone(),
                        window,
                        cx,
                    )
                })
            })
            .ok()
        })
    }

    fn is_dirty(&self, cx: &App) -> bool {
        match self.terminal.read(cx).task() {
            Some(task) => task.status == TaskStatus::Running,
            None => self.has_bell(),
        }
    }

    fn has_conflict(&self, _cx: &App) -> bool {
        false
    }

    fn can_save_as(&self, _cx: &App) -> bool {
        false
    }

    fn as_searchable(
        &self,
        handle: &Entity<Self>,
        _: &App,
    ) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(handle.clone()))
    }

    fn breadcrumb_location(&self, cx: &App) -> ToolbarItemLocation {
        if self.show_breadcrumbs && !self.terminal().read(cx).breadcrumb_text.trim().is_empty() {
            ToolbarItemLocation::PrimaryLeft
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    fn breadcrumbs(&self, cx: &App) -> Option<(Vec<HighlightedText>, Option<Font>)> {
        Some((
            vec![HighlightedText {
                text: self.terminal().read(cx).breadcrumb_text.clone().into(),
                highlights: vec![],
            }],
            None,
        ))
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Everything this view reaches the workspace for - the terminal panel
        // it broadcasts to, where a session log opens, the working directory a
        // restart falls back to, the links a click follows - has to follow the
        // tab, or a terminal moved to a new window keeps driving the old one.
        self.workspace = workspace.weak_handle();
        self.rebind_window_focus_listeners(window, cx);
        if self.terminal().read(cx).task().is_none() {
            if let Some((new_id, old_id)) = workspace.database_id().zip(self.workspace_id) {
                log::debug!(
                    "Updating workspace id for the terminal, old: {old_id:?}, new: {new_id:?}",
                );
                let db = TerminalDb::global(cx);
                let entity_id = cx.entity_id().as_u64();
                cx.background_spawn(async move {
                    db.update_workspace_id(new_id, old_id, entity_id).await
                })
                .detach();
            }
            self.workspace_id = workspace.database_id();
        }
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl SerializableItem for TerminalView {
    fn serialized_item_kind() -> &'static str {
        "Terminal"
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<workspace::ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<()>> {
        let db = TerminalDb::global(cx);
        delete_unloaded_items(alive_items, workspace_id, "terminals", &db, cx)
    }

    fn serialize(
        &mut self,
        _workspace: &mut Workspace,
        item_id: workspace::ItemId,
        _closing: bool,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Task<anyhow::Result<()>>> {
        let terminal = self.terminal().read(cx);
        if terminal.task().is_some() {
            return None;
        }

        if !self.needs_serialize {
            return None;
        }

        let workspace_id = self.workspace_id?;
        let cwd = terminal.working_directory();
        // A remote shell has no client-side directory to save; what it has is
        // the directory it reported on the host, which the restored terminal
        // rebuilds its `ssh` command around.
        let remote_cwd = terminal
            .remote_working_directory()
            .map(|cwd| cwd.to_string_lossy().into_owned());
        let custom_title = self.custom_title.clone();
        self.needs_serialize = false;

        let db = TerminalDb::global(cx);
        Some(cx.background_spawn(async move {
            if let Some(cwd) = cwd {
                db.save_working_directory(item_id, workspace_id, cwd)
                    .await?;
            }
            if let Some(remote_cwd) = remote_cwd {
                db.save_remote_working_directory(item_id, workspace_id, remote_cwd)
                    .await?;
            }
            db.save_custom_title(item_id, workspace_id, custom_title)
                .await?;
            Ok(())
        }))
    }

    fn should_serialize(&self, _: &Self::Event) -> bool {
        self.needs_serialize
    }

    fn deserialize(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: workspace::ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        window.spawn(cx, async move |cx| {
            let (cwd, custom_title) = cx
                .update(|_window, cx| {
                    let db = TerminalDb::global(cx);
                    // The two directories are stored apart because they are
                    // resolved on different machines; which one applies is
                    // decided by where this workspace's shells now run.
                    let from_db = if project.read(cx).is_via_remote_server() {
                        db.get_remote_working_directory(item_id, workspace_id)
                            .log_err()
                            .flatten()
                            .map(PathBuf::from)
                    } else {
                        db.get_working_directory(item_id, workspace_id)
                            .log_err()
                            .flatten()
                    };
                    let cwd = if from_db
                        .as_ref()
                        .is_some_and(|from_db| !from_db.as_os_str().is_empty())
                    {
                        from_db
                    } else {
                        workspace
                            .upgrade()
                            .and_then(|workspace| default_working_directory(workspace.read(cx), cx))
                    };
                    let custom_title = db
                        .get_custom_title(item_id, workspace_id)
                        .log_err()
                        .flatten()
                        .filter(|title| !title.trim().is_empty());
                    (cwd, custom_title)
                })
                .ok()
                .unwrap_or((None, None));

            let terminal = project
                .update(cx, |project, cx| project.create_terminal_shell(cwd, cx))
                .await?;
            cx.update(|window, cx| {
                cx.new(|cx| {
                    let mut view = TerminalView::new(
                        terminal,
                        workspace,
                        Some(workspace_id),
                        project.downgrade(),
                        window,
                        cx,
                    );
                    if custom_title.is_some() {
                        view.custom_title = custom_title;
                    }
                    view
                })
            })
        })
    }
}

impl SearchableItem for TerminalView {
    type Match = Range;

    fn supported_options(&self) -> SearchOptions {
        SearchOptions {
            case: false,
            word: false,
            regex: true,
            replacement: false,
            selection: false,
            select_all: false,
            find_in_results: false,
        }
    }

    /// Clear stored matches
    fn clear_matches(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.terminal().update(cx, |term, _| term.matches.clear())
    }

    /// Store matches returned from find_matches somewhere for rendering
    fn update_matches(
        &mut self,
        matches: &[Self::Match],
        _active_match_index: Option<usize>,
        _token: SearchToken,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.terminal()
            .update(cx, |term, _| term.matches = matches.to_vec())
    }

    /// Returns the selection content to pre-load into this search
    fn query_suggestion(
        &mut self,
        _seed_query_override: Option<SeedQuerySetting>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> String {
        self.terminal()
            .read(cx)
            .last_content
            .selection_text
            .clone()
            .unwrap_or_default()
    }

    /// Focus match at given index into the Vec of matches
    fn activate_match(
        &mut self,
        index: usize,
        _: &[Self::Match],
        _token: SearchToken,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.terminal()
            .update(cx, |term, _| term.activate_match(index));
        cx.notify();
    }

    /// Add selections for all matches given.
    fn select_matches(
        &mut self,
        matches: &[Self::Match],
        _token: SearchToken,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.terminal()
            .update(cx, |term, _| term.select_matches(matches));
        cx.notify();
    }

    /// Get all of the matches for this query, should be done on the background
    fn find_matches(
        &mut self,
        query: Arc<SearchQuery>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Vec<Self::Match>> {
        if let Some(s) = regex_search_for_query(&query) {
            self.terminal()
                .update(cx, |term, cx| term.find_matches(s, cx))
        } else {
            Task::ready(vec![])
        }
    }

    /// Reports back to the search toolbar what the active match should be (the selection)
    fn active_match_index(
        &mut self,
        direction: Direction,
        matches: &[Self::Match],
        _token: SearchToken,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        // Selection head might have a value if there's a selection that isn't
        // associated with a match. Therefore, if there are no matches, we should
        // report None, no matter the state of the terminal

        if !matches.is_empty() {
            if let Some(selection_head) = self.terminal().read(cx).selection_head {
                // If selection head is contained in a match. Return that match
                match direction {
                    Direction::Prev => {
                        // If no selection before selection head, return the first match
                        Some(
                            matches
                                .iter()
                                .enumerate()
                                .rev()
                                .find(|(_, search_match)| {
                                    search_match.contains(selection_head)
                                        || search_match.start() < selection_head
                                })
                                .map(|(ix, _)| ix)
                                .unwrap_or(0),
                        )
                    }
                    Direction::Next => {
                        // If no selection after selection head, return the last match
                        Some(
                            matches
                                .iter()
                                .enumerate()
                                .find(|(_, search_match)| {
                                    search_match.contains(selection_head)
                                        || search_match.start() > selection_head
                                })
                                .map(|(ix, _)| ix)
                                .unwrap_or(matches.len().saturating_sub(1)),
                        )
                    }
                }
            } else {
                // Matches found but no active selection, return the first last one (closest to cursor)
                Some(matches.len().saturating_sub(1))
            }
        } else {
            None
        }
    }
    fn replace(
        &mut self,
        _: &Self::Match,
        _: &SearchQuery,
        _token: SearchToken,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) {
        // Replacement is not supported in terminal view, so this is a no-op.
    }
}

/// Gets the working directory for the given workspace, respecting the user's settings.
/// Falls back to home directory when no project directory is available.
///
/// For remote projects, local-only resolution (home dir fallback, shell expansion,
/// local `is_dir` checks) is skipped -- returning `None` lets the remote shell
/// open in the remote user's home directory by default.
pub fn default_working_directory(workspace: &Workspace, cx: &App) -> Option<PathBuf> {
    let is_remote = workspace.project().read(cx).is_remote();
    let directory = match &TerminalSettings::get_global(cx).working_directory {
        WorkingDirectory::CurrentFileDirectory => workspace
            .project()
            .read(cx)
            .active_entry_directory(cx)
            .or_else(|| current_project_directory(workspace, cx)),
        WorkingDirectory::CurrentProjectDirectory => current_project_directory(workspace, cx),
        WorkingDirectory::FirstProjectDirectory => first_project_directory(workspace, cx),
        WorkingDirectory::AlwaysHome => None,
        WorkingDirectory::Always { directory } if !is_remote => shellexpand::full(directory)
            .ok()
            .map(|dir| Path::new(&dir.to_string()).to_path_buf())
            .filter(|dir| dir.is_dir()),
        WorkingDirectory::Always { .. } => None,
    };

    if is_remote {
        directory
    } else {
        directory.or_else(dirs::home_dir)
    }
}

fn current_project_directory(workspace: &Workspace, cx: &App) -> Option<PathBuf> {
    workspace
        .project()
        .read(cx)
        .active_project_directory(cx)
        .as_deref()
        .map(Path::to_path_buf)
        .or_else(|| first_project_directory(workspace, cx))
}

///Gets the first project's home directory, or the home directory
fn first_project_directory(workspace: &Workspace, cx: &App) -> Option<PathBuf> {
    let worktree = workspace.worktrees(cx).next()?.read(cx);
    let worktree_path = worktree.abs_path();
    if worktree.root_entry()?.is_dir() {
        Some(worktree_path.to_path_buf())
    } else {
        // If worktree is a file, return its parent directory
        worktree_path.parent().map(|p| p.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use project::{Entry, Project, ProjectPath, Worktree};
    use remote::RemoteClient;
    use std::path::{Path, PathBuf};
    use util::paths::PathStyle;
    use util::rel_path::RelPath;
    use workspace::item::test::{TestItem, TestProjectItem};
    use workspace::{AppState, MultiWorkspace, SelectedEntry};

    #[test]
    fn the_progress_label_appends_a_nested_repeat() {
        let progress = MacroProgress {
            step_index: 1,
            step_count: 3,
            iteration: 2,
            iteration_count: Some(5),
            nested: None,
            branch: None,
        };
        assert_eq!(
            macro_progress_label("再起動", progress),
            "マクロ「再起動」実行中 (2/5 回目、ステップ 2/3)"
        );

        let nested = MacroProgress {
            nested: Some(terminal::terminal_macro::NestedProgress {
                iteration: 1,
                iteration_count: Some(4),
                step_index: 0,
                step_count: 2,
                kind_label: "送信",
            }),
            ..progress
        };
        assert_eq!(
            macro_progress_label("再起動", nested),
            "マクロ「再起動」実行中 (2/5 回目、ステップ 2/3 → 繰り返し 1/4 のステップ 1/2「送信」)"
        );

        let endless_nested = MacroProgress {
            iteration_count: Some(1),
            nested: Some(terminal::terminal_macro::NestedProgress {
                iteration: 7,
                iteration_count: None,
                step_index: 1,
                step_count: 2,
                kind_label: "画面を消去",
            }),
            ..progress
        };
        assert_eq!(
            macro_progress_label("巡回", endless_nested),
            "マクロ「巡回」実行中 (2/3 → 繰り返し 7 回目のステップ 2/2「画面を消去」)"
        );
    }

    #[test]
    fn the_progress_label_names_the_branch_a_macro_took() {
        let progress = MacroProgress {
            step_index: 1,
            step_count: 3,
            iteration: 1,
            iteration_count: Some(1),
            nested: None,
            branch: Some(terminal::terminal_macro::MacroBranch::Then),
        };
        assert_eq!(
            macro_progress_label("復旧", progress),
            "マクロ「復旧」実行中 (2/3、分岐: 一致 → そのとき)"
        );

        let missed = MacroProgress {
            branch: Some(terminal::terminal_macro::MacroBranch::Else),
            ..progress
        };
        assert_eq!(
            macro_progress_label("復旧", missed),
            "マクロ「復旧」実行中 (2/3、分岐: 不一致 → それ以外)"
        );

        // A 分岐 inside a 繰り返し keeps both, so the strip says where the run
        // is and which way it went.
        let inside_a_repeat = MacroProgress {
            nested: Some(terminal::terminal_macro::NestedProgress {
                iteration: 2,
                iteration_count: Some(4),
                step_index: 0,
                step_count: 2,
                kind_label: "分岐",
            }),
            branch: Some(terminal::terminal_macro::MacroBranch::Timeout),
            ..progress
        };
        assert_eq!(
            macro_progress_label("復旧", inside_a_repeat),
            "マクロ「復旧」実行中 (2/3 → 繰り返し 2/4 のステップ 1/2「分岐」、分岐: タイムアウト → 続行)"
        );
    }

    fn expected_drop_text(paths: &[PathBuf]) -> String {
        let mut text = String::new();
        for path in paths {
            text.push(' ');
            text.push_str(&shlex::try_quote(path.to_str().unwrap()).unwrap());
        }
        text.push(' ');
        text
    }

    fn assert_drop_writes_to_terminal(
        pane: &Entity<Pane>,
        terminal_view_index: usize,
        terminal: &Entity<Terminal>,
        dropped: &dyn Any,
        expected_text: &str,
        window: &mut Window,
        cx: &mut Context<MultiWorkspace>,
    ) {
        let _ = terminal.update(cx, |terminal, _| terminal.take_input_log());

        let handled = pane.update(cx, |pane, cx| {
            pane.item_for_index(terminal_view_index)
                .unwrap()
                .handle_drop(pane, dropped, window, cx)
        });
        assert!(handled, "handle_drop should return true for {:?}", dropped);

        let mut input_log = terminal.update(cx, |terminal, _| terminal.take_input_log());
        assert_eq!(input_log.len(), 1, "expected exactly one write to terminal");
        let written =
            String::from_utf8(input_log.remove(0)).expect("terminal write should be valid UTF-8");
        assert_eq!(written, expected_text);
    }

    // DEC private mode 1049: a program writes this to enter the alternate screen buffer.
    const ENTER_ALT_SCREEN: &[u8] = b"\x1b[?1049h";

    // CSI `1;2A` = cursor-up with the xterm Shift modifier (`1 + 1` for Shift).
    const SHIFT_UP_ESCAPE: &[u8] = b"\x1b[1;2A";

    #[gpui::test]
    async fn edit_menu_copy_and_paste_are_available_when_terminal_is_focused(
        cx: &mut TestAppContext,
    ) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        let (_pane, terminal, _terminal_view) =
            add_display_only_terminal(&project, window_handle, true, cx);

        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        cx.update(|window, cx| {
            let _ = window.draw(cx);
            assert!(window.is_action_available(&editor::actions::Copy, cx));
            assert!(window.is_action_available(&editor::actions::Paste, cx));

            cx.write_to_clipboard(gpui::ClipboardItem::new_string("foo".to_string()));
            terminal.update(cx, |terminal, _| terminal.take_input_log());
            window.dispatch_action(Box::new(editor::actions::Paste), cx);
        });
        cx.run_until_parked();

        cx.update(|_, cx| {
            let input_log = terminal.update(cx, |terminal, _| terminal.take_input_log());
            assert_eq!(input_log, vec![b"foo".to_vec()]);
        });
    }

    #[gpui::test]
    async fn shift_up_scrolls_history_in_normal_screen(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        cx.update(load_default_keymap);
        let (_pane, terminal, _terminal_view) =
            add_display_only_terminal(&project, window_handle, true, cx);

        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        let output = (0..200)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        cx.update(|window, cx| {
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(output.as_bytes(), cx);
                terminal.sync(window, cx);
            });
        });
        terminal.read_with(&cx, |terminal, _| {
            assert!(!terminal.last_content.mode.contains(Modes::ALT_SCREEN));
            assert_eq!(terminal.last_content.display_offset, 0);
        });

        cx.simulate_keystrokes("shift-up");
        cx.update(|window, cx| {
            terminal.update(cx, |terminal, cx| terminal.sync(window, cx));
        });

        assert_eq!(
            terminal.read_with(&cx, |terminal, _| terminal.last_content.display_offset),
            1,
            "shift-up should scroll terminal history in the normal screen",
        );
        assert!(
            terminal
                .update(&mut cx, |terminal, _| terminal.take_input_log())
                .is_empty(),
            "shift-up in the normal screen should not be forwarded to the shell",
        );
    }

    #[gpui::test]
    async fn shift_up_is_forwarded_to_program_in_alt_screen(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        cx.update(load_default_keymap);
        let (_pane, terminal, _terminal_view) =
            add_display_only_terminal(&project, window_handle, true, cx);

        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        cx.update(|window, cx| {
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(ENTER_ALT_SCREEN, cx);
                terminal.sync(window, cx);
            });
        });
        terminal.read_with(&cx, |terminal, _| {
            assert!(terminal.last_content.mode.contains(Modes::ALT_SCREEN));
        });

        cx.simulate_keystrokes("shift-up");
        assert_eq!(
            terminal.update(&mut cx, |terminal, _| terminal.take_input_log()),
            vec![SHIFT_UP_ESCAPE.to_vec()],
            "shift-up should be forwarded to the program in the alternate screen",
        );
    }

    #[cfg(target_os = "linux")]
    #[gpui::test]
    async fn ctrl_q_is_forwarded_to_terminal_not_quit(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;
        cx.update(load_default_keymap);
        let (_pane, terminal, _terminal_view) =
            add_display_only_terminal(&project, window_handle, true, cx);

        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        cx.simulate_keystrokes("ctrl-q");
        assert_eq!(
            terminal.update(&mut cx, |terminal, _| terminal.take_input_log()),
            vec![vec![0x11]],
            "ctrl-q in a focused terminal should send 0x11 to the PTY, not trigger zed::Quit",
        );
    }

    // Working directory calculation tests

    // No Worktrees in project -> home_dir()
    #[gpui::test]
    async fn no_worktree(cx: &mut TestAppContext) {
        let (project, workspace) = init_test(cx).await;
        cx.read(|cx| {
            let workspace = workspace.read(cx);
            let active_entry = project.read(cx).active_entry();

            //Make sure environment is as expected
            assert!(active_entry.is_none());
            assert!(workspace.worktrees(cx).next().is_none());

            let res = default_working_directory(workspace, cx);
            assert_eq!(res, dirs::home_dir());
            let res = first_project_directory(workspace, cx);
            assert_eq!(res, None);
        });
    }

    #[gpui::test]
    async fn remote_no_worktree_uses_remote_shell_default_cwd(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        let (_project, workspace) = init_remote_test(cx, server_cx).await;

        cx.read(|cx| {
            let workspace = workspace.read(cx);

            assert!(workspace.project().read(cx).is_remote());
            assert!(workspace.worktrees(cx).next().is_none());
            assert_eq!(default_working_directory(workspace, cx), None);
        });
    }

    // No active entry, but a worktree, worktree is a file -> parent directory
    #[gpui::test]
    async fn no_active_entry_worktree_is_file(cx: &mut TestAppContext) {
        let (project, workspace) = init_test(cx).await;

        create_file_wt(project.clone(), "/root.txt", cx).await;
        cx.read(|cx| {
            let workspace = workspace.read(cx);
            let active_entry = project.read(cx).active_entry();

            //Make sure environment is as expected
            assert!(active_entry.is_none());
            assert!(workspace.worktrees(cx).next().is_some());

            let res = default_working_directory(workspace, cx);
            assert_eq!(res, Some(Path::new("/").to_path_buf()));
            let res = first_project_directory(workspace, cx);
            assert_eq!(res, Some(Path::new("/").to_path_buf()));
        });
    }

    // No active entry, but a worktree, worktree is a folder -> worktree_folder
    #[gpui::test]
    async fn no_active_entry_worktree_is_dir(cx: &mut TestAppContext) {
        let (project, workspace) = init_test(cx).await;

        let (_wt, _entry) = create_folder_wt(project.clone(), "/root/", cx).await;
        cx.update(|cx| {
            let workspace = workspace.read(cx);
            let active_entry = project.read(cx).active_entry();

            assert!(active_entry.is_none());
            assert!(workspace.worktrees(cx).next().is_some());

            let res = default_working_directory(workspace, cx);
            assert_eq!(res, Some(Path::new("/root/").to_path_buf()));
            let res = first_project_directory(workspace, cx);
            assert_eq!(res, Some(Path::new("/root/").to_path_buf()));
        });
    }

    // Active entry with a work tree, worktree is a file -> worktree_folder()
    #[gpui::test]
    async fn active_entry_worktree_is_file(cx: &mut TestAppContext) {
        let (project, workspace) = init_test(cx).await;

        let (_wt, _entry) = create_folder_wt(project.clone(), "/root1/", cx).await;
        let (wt2, entry2) = create_file_wt(project.clone(), "/root2.txt", cx).await;
        insert_active_entry_for(wt2, entry2, project.clone(), cx);

        cx.update(|cx| {
            let workspace = workspace.read(cx);
            let active_entry = project.read(cx).active_entry();

            assert!(active_entry.is_some());

            let res = default_working_directory(workspace, cx);
            assert_eq!(res, Some(Path::new("/root1/").to_path_buf()));
            let res = first_project_directory(workspace, cx);
            assert_eq!(res, Some(Path::new("/root1/").to_path_buf()));
        });
    }

    // Active entry, with a worktree, worktree is a folder -> worktree_folder
    #[gpui::test]
    async fn active_entry_worktree_is_dir(cx: &mut TestAppContext) {
        let (project, workspace) = init_test(cx).await;

        let (_wt, _entry) = create_folder_wt(project.clone(), "/root1/", cx).await;
        let (wt2, entry2) = create_folder_wt(project.clone(), "/root2/", cx).await;
        insert_active_entry_for(wt2, entry2, project.clone(), cx);

        cx.update(|cx| {
            let workspace = workspace.read(cx);
            let active_entry = project.read(cx).active_entry();

            assert!(active_entry.is_some());

            let res = default_working_directory(workspace, cx);
            assert_eq!(res, Some(Path::new("/root2/").to_path_buf()));
            let res = first_project_directory(workspace, cx);
            assert_eq!(res, Some(Path::new("/root1/").to_path_buf()));
        });
    }

    // active_entry_directory: No active entry -> returns None (used by CurrentFileDirectory)
    #[gpui::test]
    async fn active_entry_directory_no_active_entry(cx: &mut TestAppContext) {
        let (project, _workspace) = init_test(cx).await;

        let (_wt, _entry) = create_folder_wt(project.clone(), "/root/", cx).await;

        cx.update(|cx| {
            assert!(project.read(cx).active_entry().is_none());

            let res = project.read(cx).active_entry_directory(cx);
            assert_eq!(res, None);
        });
    }

    // active_entry_directory: Active entry is file -> returns parent directory (used by CurrentFileDirectory)
    #[gpui::test]
    async fn active_entry_directory_active_file(cx: &mut TestAppContext) {
        let (project, _workspace) = init_test(cx).await;

        let (wt, _entry) = create_folder_wt(project.clone(), "/root/", cx).await;
        let entry = create_file_in_worktree(wt.clone(), "src/main.rs", cx).await;
        insert_active_entry_for(wt, entry, project.clone(), cx);

        cx.update(|cx| {
            let res = project.read(cx).active_entry_directory(cx);
            assert_eq!(res, Some(Path::new("/root/src").to_path_buf()));
        });
    }

    // active_entry_directory: Active entry is directory -> returns that directory (used by CurrentFileDirectory)
    #[gpui::test]
    async fn active_entry_directory_active_dir(cx: &mut TestAppContext) {
        let (project, _workspace) = init_test(cx).await;

        let (wt, entry) = create_folder_wt(project.clone(), "/root/", cx).await;
        insert_active_entry_for(wt, entry, project.clone(), cx);

        cx.update(|cx| {
            let res = project.read(cx).active_entry_directory(cx);
            assert_eq!(res, Some(Path::new("/root/").to_path_buf()));
        });
    }

    /// Creates a worktree with 1 file: /root.txt
    pub async fn init_test(cx: &mut TestAppContext) -> (Entity<Project>, Entity<Workspace>) {
        let (project, workspace, _) = init_test_with_window(cx).await;
        (project, workspace)
    }

    fn load_default_keymap(cx: &mut App) {
        cx.bind_keys(
            settings::KeymapFile::load_asset_allow_partial_failure(
                settings::DEFAULT_KEYMAP_PATH,
                cx,
            )
            .unwrap(),
        );
    }

    fn add_display_only_terminal(
        project: &Entity<Project>,
        window_handle: gpui::WindowHandle<MultiWorkspace>,
        focus: bool,
        cx: &mut TestAppContext,
    ) -> (Entity<Pane>, Entity<Terminal>, Entity<TerminalView>) {
        let project = project.clone();
        window_handle
            .update(cx, |multi_workspace, window, cx| {
                let workspace = multi_workspace.workspace().clone();
                let active_pane = workspace.read(cx).active_pane().clone();

                let terminal = cx.new(|cx| {
                    terminal::TerminalBuilder::new_display_only(
                        CursorShape::default(),
                        terminal::terminal_settings::AlternateScroll::On,
                        None,
                        0,
                        cx.background_executor(),
                        PathStyle::local(),
                    )
                    .subscribe(cx)
                });
                let terminal_view = cx.new(|cx| {
                    TerminalView::new(
                        terminal.clone(),
                        workspace.downgrade(),
                        None,
                        project.downgrade(),
                        window,
                        cx,
                    )
                });

                active_pane.update(cx, |pane, cx| {
                    pane.add_item(
                        Box::new(terminal_view.clone()),
                        true,
                        false,
                        None,
                        window,
                        cx,
                    );
                });

                if focus {
                    let focus_handle = terminal_view.read(cx).focus_handle.clone();
                    focus_handle.focus(window, cx);
                }

                (active_pane, terminal, terminal_view)
            })
            .unwrap()
    }

    /// Creates a worktree with 1 file /root.txt and returns the project, workspace, and window handle.
    async fn init_test_with_window(
        cx: &mut TestAppContext,
    ) -> (
        Entity<Project>,
        Entity<Workspace>,
        gpui::WindowHandle<MultiWorkspace>,
    ) {
        let params = cx.update(AppState::test);
        cx.update(|cx| {
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let project = Project::test(params.fs.clone(), [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window_handle
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();

        (project, workspace, window_handle)
    }

    async fn init_remote_test(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) -> (Entity<Project>, Entity<Workspace>) {
        cx.update(|cx| {
            release_channel::init(semver::Version::new(0, 0, 0), cx);
        });
        server_cx.update(|cx| {
            release_channel::init(semver::Version::new(0, 0, 0), cx);
        });

        let params = cx.update(AppState::test);
        let (opts, server_session, connect_guard) = RemoteClient::fake_server(cx, server_cx);
        let ping_handler = server_cx.new(|_| ());
        server_session.add_request_handler::<rpc::proto::Ping, _, _, _>(
            ping_handler.downgrade(),
            |_entity, _envelope, _cx| async { Ok(rpc::proto::Ack {}) },
        );
        drop(connect_guard);

        let remote_client = RemoteClient::connect_mock(opts, cx).await;
        let project = cx.update(|cx| {
            Project::remote(
                remote_client,
                params.client.clone(),
                params.node_runtime.clone(),
                params.user_store.clone(),
                params.languages.clone(),
                params.fs.clone(),
                false,
                cx,
            )
        });

        let window_handle = cx.add_window({
            let params = params.clone();
            let project_for_workspace = project.clone();
            move |window, cx| {
                window.activate_window();
                let workspace = cx.new(|cx| {
                    Workspace::new(
                        None,
                        project_for_workspace.clone(),
                        params.clone(),
                        window,
                        cx,
                    )
                });
                MultiWorkspace::new(workspace, window, cx)
            }
        });
        let workspace = window_handle
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();

        (project, workspace)
    }

    /// Creates a file in the given worktree and returns its entry.
    async fn create_file_in_worktree(
        worktree: Entity<Worktree>,
        relative_path: impl AsRef<Path>,
        cx: &mut TestAppContext,
    ) -> Entry {
        cx.update(|cx| {
            worktree.update(cx, |worktree, cx| {
                worktree.create_entry(
                    RelPath::new(relative_path.as_ref(), PathStyle::local())
                        .unwrap()
                        .as_ref()
                        .into(),
                    false,
                    None,
                    cx,
                )
            })
        })
        .await
        .unwrap()
        .into_included()
        .unwrap()
    }

    /// Creates a worktree with 1 folder: /root{suffix}/
    async fn create_folder_wt(
        project: Entity<Project>,
        path: impl AsRef<Path>,
        cx: &mut TestAppContext,
    ) -> (Entity<Worktree>, Entry) {
        create_wt(project, true, path, cx).await
    }

    /// Creates a worktree with 1 file: /root{suffix}.txt
    async fn create_file_wt(
        project: Entity<Project>,
        path: impl AsRef<Path>,
        cx: &mut TestAppContext,
    ) -> (Entity<Worktree>, Entry) {
        create_wt(project, false, path, cx).await
    }

    async fn create_wt(
        project: Entity<Project>,
        is_dir: bool,
        path: impl AsRef<Path>,
        cx: &mut TestAppContext,
    ) -> (Entity<Worktree>, Entry) {
        let (wt, _) = project
            .update(cx, |project, cx| {
                project.find_or_create_worktree(path, true, cx)
            })
            .await
            .unwrap();

        let entry = cx
            .update(|cx| {
                wt.update(cx, |wt, cx| {
                    wt.create_entry(RelPath::empty_arc(), is_dir, None, cx)
                })
            })
            .await
            .unwrap()
            .into_included()
            .unwrap();

        (wt, entry)
    }

    pub fn insert_active_entry_for(
        wt: Entity<Worktree>,
        entry: Entry,
        project: Entity<Project>,
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            let p = ProjectPath {
                worktree_id: wt.read(cx).id(),
                path: entry.path,
            };
            project.update(cx, |project, cx| project.set_active_path(Some(p), cx));
        });
    }

    // Terminal drag/drop test

    #[gpui::test]
    async fn test_handle_drop_writes_paths_for_all_drop_types(cx: &mut TestAppContext) {
        let (project, _workspace, window_handle) = init_test_with_window(cx).await;

        let (worktree, _) = create_folder_wt(project.clone(), "/root/", cx).await;
        let first_entry = create_file_in_worktree(worktree.clone(), "first.txt", cx).await;
        let second_entry = create_file_in_worktree(worktree.clone(), "second.txt", cx).await;

        let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
        let first_path = project
            .read_with(cx, |project, cx| {
                project.absolute_path(
                    &ProjectPath {
                        worktree_id,
                        path: first_entry.path.clone(),
                    },
                    cx,
                )
            })
            .unwrap();
        let second_path = project
            .read_with(cx, |project, cx| {
                project.absolute_path(
                    &ProjectPath {
                        worktree_id,
                        path: second_entry.path.clone(),
                    },
                    cx,
                )
            })
            .unwrap();

        let (active_pane, terminal, terminal_view) =
            add_display_only_terminal(&project, window_handle, false, cx);

        let tab_item = window_handle
            .update(cx, |_, window, cx| {
                let tab_project_item = cx.new(|_| TestProjectItem {
                    entry_id: Some(second_entry.id),
                    project_path: Some(ProjectPath {
                        worktree_id,
                        path: second_entry.path.clone(),
                    }),
                    is_dirty: false,
                });
                let tab_item =
                    cx.new(|cx| TestItem::new(cx).with_project_items(&[tab_project_item]));
                active_pane.update(cx, |pane, cx| {
                    pane.add_item(Box::new(tab_item.clone()), true, false, None, window, cx);
                });
                tab_item
            })
            .unwrap();

        cx.run_until_parked();

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                let workspace = multi_workspace.workspace().clone();
                let terminal_view_index =
                    active_pane.read(cx).index_for_item(&terminal_view).unwrap();
                let dragged_tab_index = active_pane.read(cx).index_for_item(&tab_item).unwrap();

                assert!(
                    workspace.read(cx).pane_for(&terminal_view).is_some(),
                    "terminal view not registered with workspace after run_until_parked"
                );

                // Dragging an external file should write its path to the terminal
                let external_paths = ExternalPaths(vec![first_path.clone()].into());
                assert_drop_writes_to_terminal(
                    &active_pane,
                    terminal_view_index,
                    &terminal,
                    &external_paths,
                    &expected_drop_text(std::slice::from_ref(&first_path)),
                    window,
                    cx,
                );

                // Dragging a tab should write the path of the tab's item to the terminal
                let dragged_tab = DraggedTab {
                    pane: active_pane.clone(),
                    item: Box::new(tab_item.clone()),
                    ix: dragged_tab_index,
                    detail: 0,
                    is_active: false,
                };
                assert_drop_writes_to_terminal(
                    &active_pane,
                    terminal_view_index,
                    &terminal,
                    &dragged_tab,
                    &expected_drop_text(std::slice::from_ref(&second_path)),
                    window,
                    cx,
                );

                // Dragging multiple selections should write both paths to the terminal
                let dragged_selection = DraggedSelection {
                    active_selection: SelectedEntry {
                        worktree_id,
                        entry_id: first_entry.id,
                    },
                    marked_selections: Arc::from([
                        SelectedEntry {
                            worktree_id,
                            entry_id: first_entry.id,
                        },
                        SelectedEntry {
                            worktree_id,
                            entry_id: second_entry.id,
                        },
                    ]),
                };
                assert_drop_writes_to_terminal(
                    &active_pane,
                    terminal_view_index,
                    &terminal,
                    &dragged_selection,
                    &expected_drop_text(&[first_path.clone(), second_path.clone()]),
                    window,
                    cx,
                );

                // Dropping a project entry should write the entry's path to the terminal
                let dropped_entry_id = first_entry.id;
                assert_drop_writes_to_terminal(
                    &active_pane,
                    terminal_view_index,
                    &terminal,
                    &dropped_entry_id,
                    &expected_drop_text(&[first_path]),
                    window,
                    cx,
                );
            })
            .unwrap();
    }

    // Terminal rename tests

    #[gpui::test]
    async fn test_custom_title_initially_none(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let (project, workspace) = init_test(cx).await;

        let terminal = project
            .update(cx, |project, cx| project.create_terminal_shell(None, cx))
            .await
            .unwrap();

        let terminal_view = cx
            .add_window(|window, cx| {
                TerminalView::new(
                    terminal,
                    workspace.downgrade(),
                    None,
                    project.downgrade(),
                    window,
                    cx,
                )
            })
            .root(cx)
            .unwrap();

        terminal_view.update(cx, |view, _cx| {
            assert!(view.custom_title().is_none());
        });
    }

    #[gpui::test]
    async fn test_set_custom_title(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let (project, workspace) = init_test(cx).await;

        let terminal = project
            .update(cx, |project, cx| project.create_terminal_shell(None, cx))
            .await
            .unwrap();

        let terminal_view = cx
            .add_window(|window, cx| {
                TerminalView::new(
                    terminal,
                    workspace.downgrade(),
                    None,
                    project.downgrade(),
                    window,
                    cx,
                )
            })
            .root(cx)
            .unwrap();

        terminal_view.update(cx, |view, cx| {
            view.set_custom_title(Some("frontend".to_string()), cx);
            assert_eq!(view.custom_title(), Some("frontend"));
        });
    }

    #[gpui::test]
    async fn test_set_custom_title_empty_becomes_none(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let (project, workspace) = init_test(cx).await;

        let terminal = project
            .update(cx, |project, cx| project.create_terminal_shell(None, cx))
            .await
            .unwrap();

        let terminal_view = cx
            .add_window(|window, cx| {
                TerminalView::new(
                    terminal,
                    workspace.downgrade(),
                    None,
                    project.downgrade(),
                    window,
                    cx,
                )
            })
            .root(cx)
            .unwrap();

        terminal_view.update(cx, |view, cx| {
            view.set_custom_title(Some("test".to_string()), cx);
            assert_eq!(view.custom_title(), Some("test"));

            view.set_custom_title(Some("".to_string()), cx);
            assert!(view.custom_title().is_none());

            view.set_custom_title(Some("  ".to_string()), cx);
            assert!(view.custom_title().is_none());
        });
    }

    #[gpui::test]
    async fn test_custom_title_marks_needs_serialize(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let (project, workspace) = init_test(cx).await;

        let terminal = project
            .update(cx, |project, cx| project.create_terminal_shell(None, cx))
            .await
            .unwrap();

        let terminal_view = cx
            .add_window(|window, cx| {
                TerminalView::new(
                    terminal,
                    workspace.downgrade(),
                    None,
                    project.downgrade(),
                    window,
                    cx,
                )
            })
            .root(cx)
            .unwrap();

        terminal_view.update(cx, |view, cx| {
            view.needs_serialize = false;
            view.set_custom_title(Some("new_label".to_string()), cx);
            assert!(view.needs_serialize);
        });
    }

    #[gpui::test]
    async fn test_tab_content_uses_custom_title(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let (project, workspace) = init_test(cx).await;

        let terminal = project
            .update(cx, |project, cx| project.create_terminal_shell(None, cx))
            .await
            .unwrap();

        let terminal_view = cx
            .add_window(|window, cx| {
                TerminalView::new(
                    terminal,
                    workspace.downgrade(),
                    None,
                    project.downgrade(),
                    window,
                    cx,
                )
            })
            .root(cx)
            .unwrap();

        terminal_view.update(cx, |view, cx| {
            view.set_custom_title(Some("my-server".to_string()), cx);
            let text = view.tab_content_text(0, cx);
            assert_eq!(text.as_ref(), "my-server");
        });

        terminal_view.update(cx, |view, cx| {
            view.set_custom_title(None, cx);
            let text = view.tab_content_text(0, cx);
            assert_ne!(text.as_ref(), "my-server");
        });
    }

    async fn draw_standalone_terminal(
        output: &[u8],
        cx: &mut TestAppContext,
    ) -> (gpui::Bounds<Pixels>, gpui::Size<Pixels>) {
        let (project, workspace) = init_test(cx).await;
        let terminal = cx.new(|cx| {
            terminal::TerminalBuilder::new_display_only(
                CursorShape::default(),
                terminal::terminal_settings::AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });
        terminal.update(cx, |terminal, cx| {
            terminal.write_output(output, cx);
        });

        let (terminal_view, cx) = cx.add_window_view(|window, cx| {
            TerminalView::new(
                terminal.clone(),
                workspace.downgrade(),
                None,
                project.downgrade(),
                window,
                cx,
            )
        });

        let draw_size = gpui::size(px(400.), px(201.));
        cx.simulate_resize(draw_size);
        cx.draw(gpui::Point::default(), draw_size, |_, _| {
            terminal_view.clone().into_any_element()
        });
        cx.run_until_parked();
        cx.draw(gpui::Point::default(), draw_size, |_, _| {
            terminal_view.clone().into_any_element()
        });

        let bounds = terminal.read_with(cx, |terminal, _| {
            terminal.last_content().terminal_bounds.bounds
        });
        (bounds, draw_size)
    }

    #[gpui::test]
    async fn test_short_standalone_terminal_stays_top_anchored_on_resize(cx: &mut TestAppContext) {
        let (bounds, _) = draw_standalone_terminal(b"$ ", cx).await;
        assert_eq!(bounds.origin.y, px(0.));
    }

    #[gpui::test]
    async fn test_full_standalone_terminal_stays_bottom_anchored_on_resize(
        cx: &mut TestAppContext,
    ) {
        let (bounds, draw_size) = draw_standalone_terminal(
            b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n",
            cx,
        )
        .await;
        assert!(bounds.origin.y > px(0.));
        assert_eq!(bounds.bottom(), draw_size.height);
    }

    #[gpui::test]
    async fn test_short_alt_screen_stays_bottom_anchored_on_resize(cx: &mut TestAppContext) {
        let (bounds, draw_size) = draw_standalone_terminal(b"\x1b[?1049h$ ", cx).await;
        assert!(bounds.origin.y > px(0.));
        assert_eq!(bounds.bottom(), draw_size.height);
    }

    #[gpui::test]
    async fn test_inline_terminal_displays_all_of_its_lines(cx: &mut TestAppContext) {
        let (project, workspace) = init_test(cx).await;
        let terminal = cx.new(|cx| {
            terminal::TerminalBuilder::new_display_only(
                CursorShape::default(),
                terminal::terminal_settings::AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });
        let (terminal_view, cx) = cx.add_window_view(|window, cx| {
            let mut terminal_view = TerminalView::new(
                terminal.clone(),
                workspace.downgrade(),
                None,
                project.downgrade(),
                window,
                cx,
            );
            terminal_view.set_embedded_mode(None, cx);
            terminal_view
        });

        for _ in 1..=20 {
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"line\n", cx);
            });
            cx.draw(
                gpui::Point::default(),
                gpui::size(px(400.), px(100.)),
                |_, _| terminal_view.clone().into_any_element(),
            );
            terminal.read_with(cx, |terminal, _| {
                assert_eq!(terminal.viewport_lines(), terminal.total_lines());
            })
        }
    }

    #[gpui::test]
    async fn test_inline_terminal_shrinks_after_clear(cx: &mut TestAppContext) {
        let (project, workspace) = init_test(cx).await;
        let terminal = cx.new(|cx| {
            terminal::TerminalBuilder::new_display_only(
                CursorShape::default(),
                terminal::terminal_settings::AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });
        let (terminal_view, cx) = cx.add_window_view(|window, cx| {
            let mut terminal_view = TerminalView::new(
                terminal.clone(),
                workspace.downgrade(),
                None,
                project.downgrade(),
                window,
                cx,
            );
            terminal_view.set_embedded_mode(None, cx);
            terminal_view
        });

        for _ in 1..=20 {
            terminal.update(cx, |terminal, cx| {
                terminal.write_output(b"line\n", cx);
            });
            cx.draw(
                gpui::Point::default(),
                gpui::size(px(400.), px(100.)),
                |_, _| terminal_view.clone().into_any_element(),
            );
        }
        terminal.read_with(cx, |terminal, _| {
            assert_eq!(terminal.total_lines(), 21);
        });

        terminal.update(cx, |terminal, _| terminal.clear());
        for _ in 1..=2 {
            cx.draw(
                gpui::Point::default(),
                gpui::size(px(400.), px(100.)),
                |_, _| terminal_view.clone().into_any_element(),
            );
        }
        terminal.read_with(cx, |terminal, _| {
            assert_eq!(terminal.total_lines(), 1);
            assert_eq!(terminal.viewport_lines(), 1);
        });
    }

    #[gpui::test]
    async fn test_tab_content_shows_terminal_title_when_custom_title_directly_set_empty(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();

        let (project, workspace) = init_test(cx).await;

        let terminal = project
            .update(cx, |project, cx| project.create_terminal_shell(None, cx))
            .await
            .unwrap();

        let terminal_view = cx
            .add_window(|window, cx| {
                TerminalView::new(
                    terminal,
                    workspace.downgrade(),
                    None,
                    project.downgrade(),
                    window,
                    cx,
                )
            })
            .root(cx)
            .unwrap();

        terminal_view.update(cx, |view, cx| {
            view.custom_title = Some("".to_string());
            let text = view.tab_content_text(0, cx);
            assert!(
                !text.is_empty(),
                "Tab should show terminal title, not empty string; got: '{}'",
                text
            );
        });

        terminal_view.update(cx, |view, cx| {
            view.custom_title = Some("   ".to_string());
            let text = view.tab_content_text(0, cx);
            assert!(
                !text.is_empty() && text.as_ref() != "   ",
                "Tab should show terminal title, not whitespace; got: '{}'",
                text
            );
        });
    }
}
