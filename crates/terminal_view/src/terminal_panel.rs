use std::{cmp, path::PathBuf, process::ExitStatus, sync::Arc, time::Duration};

use crate::{
    TerminalView, default_working_directory,
    persistence::{
        SerializedItems, SerializedTerminalPanel, deserialize_terminal_panel, serialize_pane_group,
    },
};
use breadcrumbs::Breadcrumbs;
use collections::{HashMap, HashSet};
use db::kvp::KeyValueStore;
use futures::{channel::oneshot, future::join_all};
use gpui::{
    Action, Anchor, AnyElement, App, AsyncApp, AsyncWindowContext, Context, Entity, EventEmitter,
    FocusHandle, Focusable, IntoElement, ParentElement, Pixels, Render, SharedString, Styled, Task,
    TaskExt, WeakEntity, Window, actions,
};
use itertools::Itertools;
use project::{Fs, Project};
use schemars::JsonSchema;
use serde::Deserialize;

use settings::{Settings, TerminalDockPosition};
use task::{RevealStrategy, RevealTarget, Shell, ShellBuilder, SpawnInTerminal, TaskId};
use terminal::{Terminal, terminal_settings::TerminalSettings};
use ui::{
    ButtonLike, Clickable, ContextMenu, FluentBuilder, PopoverMenu, SplitButton, Toggleable,
    Tooltip, prelude::*,
};
use util::{ResultExt, TryFutureExt};
use workspace::{
    ActivateNextPane, ActivatePane, ActivatePaneDown, ActivatePaneLeft, ActivatePaneRight,
    ActivatePaneUp, ActivatePreviousPane, DraggedTab, MoveItemToPane, MoveItemToPaneInDirection,
    MovePaneDown, MovePaneLeft, MovePaneRight, MovePaneUp, Pane, PaneGroup, SplitDirection,
    SplitDown, SplitLeft, SplitMode, SplitRight, SplitUp, SwapPaneDown, SwapPaneLeft,
    SwapPaneRight, SwapPaneUp, ToggleZoom, Workspace,
    dock::{DockPosition, Panel, PanelEvent, PanelHandle},
    move_active_item, pane,
};

use anyhow::{Result, anyhow};
use zed_actions::assistant::InlineAssist;

const TERMINAL_PANEL_KEY: &str = "TerminalPanel";

/// Upper bound per side of [`SplitGrid`]. 4×4 already leaves each pane a few
/// columns wide on a docked panel; beyond that the splits are unusable.
const MAX_GRID_SIDE: u8 = 4;

actions!(
    terminal_panel,
    [
        /// Toggles the terminal panel.
        Toggle,
        /// Toggles focus on the terminal panel.
        ToggleFocus
    ]
);

/// Splits the active terminal pane into a grid of new shells in one go, instead
/// of repeating 右に分割 / 下に分割 by hand.
#[derive(Clone, Debug, Deserialize, PartialEq, JsonSchema, Action)]
#[action(namespace = terminal)]
#[serde(deny_unknown_fields)]
pub struct SplitGrid {
    /// Number of columns, 1 to 4.
    #[serde(default = "default_grid_side")]
    pub columns: u8,
    /// Number of rows, 1 to 4.
    #[serde(default = "default_grid_side")]
    pub rows: u8,
}

fn default_grid_side() -> u8 {
    2
}

impl Default for SplitGrid {
    fn default() -> Self {
        Self {
            columns: default_grid_side(),
            rows: default_grid_side(),
        }
    }
}

pub fn init(cx: &mut App) {
    cx.observe_new(
        |workspace: &mut Workspace, _window, _: &mut Context<Workspace>| {
            workspace.register_action(TerminalPanel::new_terminal);
            workspace.register_action(TerminalPanel::open_terminal);
            workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
                if is_enabled_in_workspace(workspace, cx) {
                    workspace.toggle_panel_focus::<TerminalPanel>(window, cx);
                }
            });
            workspace.register_action(|workspace, _: &Toggle, window, cx| {
                if is_enabled_in_workspace(workspace, cx) {
                    if !workspace.toggle_panel_focus::<TerminalPanel>(window, cx) {
                        workspace.close_panel::<TerminalPanel>(window, cx);
                    }
                }
            });
            workspace.register_action(|workspace, action: &SplitGrid, window, cx| {
                let Some(terminal_panel) = workspace.panel::<TerminalPanel>(cx) else {
                    return;
                };
                let (columns, rows) = (action.columns, action.rows);
                terminal_panel.update(cx, |terminal_panel, cx| {
                    terminal_panel.split_grid(columns, rows, window, cx);
                });
            });
            workspace.register_action(|workspace, _: &crate::ToggleBroadcastInput, _window, cx| {
                let Some(terminal_panel) = workspace.panel::<TerminalPanel>(cx) else {
                    return;
                };
                terminal_panel.update(cx, |terminal_panel, cx| {
                    terminal_panel.toggle_broadcast_input(cx);
                });
            });
        },
    )
    .detach();
}

const LAUNCH_CLAUDE_CODE_LABEL: &str = "Claude Code を起動";

/// Starts the official `claude` CLI in a new terminal tab (ADR-F20).
///
/// The click dispatches rather than calling the handler directly, because
/// `Window::dispatch_action` defers to the end of the effect cycle: the handler
/// leases the workspace, which must not happen while this pane is still leased
/// by its own click listener.
fn launch_claude_code_button() -> FoldableTabBarButton {
    let button = IconButton::new("terminal-launch-claude-code", IconName::AiClaude)
        .icon_size(IconSize::Small)
        .tooltip(Tooltip::text("Claude Code を起動 (新しいターミナル)"))
        .on_click(|_, window, cx| {
            window.dispatch_action(
                Box::new(zed_actions::terminal::LaunchClaudeCode { effort: None }),
                cx,
            );
        });
    FoldableTabBarButton {
        button: button.into_any_element(),
        menu_label: LAUNCH_CLAUDE_CODE_LABEL.into(),
        menu_action: Box::new(zed_actions::terminal::LaunchClaudeCode { effort: None }),
        fold_rank: FOLD_RANK_LAUNCH_CLAUDE_CODE,
    }
}

/// The terminal the tab bar's recording toggle acts on, together with the log
/// it is writing. `None` when the pane is showing something that is not a
/// terminal.
///
/// This is resolved on every render rather than captured with the rest of the
/// tab bar, so switching to another tab while one of them records moves the red
/// dot with the active tab.
fn session_log_toggle_target(
    pane: &Pane,
    cx: &App,
) -> Option<(Entity<TerminalView>, Option<PathBuf>)> {
    let terminal_view = pane.active_item()?.downcast::<TerminalView>()?;
    let log_path = terminal_view.read(cx).session_log_path(cx);
    Some((terminal_view, log_path))
}

/// The tab bar's one-click recording toggle.
fn session_log_toggle_button(pane: &Pane, cx: &App) -> Option<FoldableTabBarButton> {
    let (terminal_view, log_path) = session_log_toggle_target(pane, cx)?;
    let recording = log_path.is_some();
    let (label, detail) = match log_path {
        Some(path) => (crate::STOP_SESSION_LOG_LABEL, path.display().to_string()),
        None => {
            let directory = terminal::session_log::resolve_log_directory(
                TerminalSettings::get_global(cx)
                    .session_log
                    .directory
                    .as_deref(),
            );
            (
                crate::START_SESSION_LOG_LABEL,
                format!("保存先: {}", directory.display()),
            )
        }
    };
    let terminal_view = terminal_view.downgrade();
    let button = IconButton::new("terminal-toggle-session-log", IconName::Circle)
        .icon_size(IconSize::Small)
        .toggle_state(recording)
        .selected_icon_color(Color::Error)
        .tooltip(move |_window, cx| {
            Tooltip::with_meta(label, Some(&crate::ToggleSessionLog), detail.clone(), cx)
        })
        .on_click(move |_, _window, cx| {
            terminal_view
                .update(cx, |terminal_view, cx| terminal_view.toggle_session_log(cx))
                .log_err();
        });
    Some(FoldableTabBarButton {
        button: button.into_any_element(),
        menu_label: label.into(),
        menu_action: Box::new(crate::ToggleSessionLog),
        fold_rank: FOLD_RANK_SESSION_LOG,
    })
}

// The order in which the tab bar gives its buttons up once the pane is too
// narrow for the whole row: the lowest rank moves into the 「…」 menu first, so
// what survives longest is what a narrow split still needs at a glance.
const FOLD_RANK_RUN_MACRO: u8 = 10;
const FOLD_RANK_FILE_TRANSFER: u8 = 15;
const FOLD_RANK_QUICK_CONNECT: u8 = 20;
const FOLD_RANK_INLINE_ASSIST: u8 = 30;
const FOLD_RANK_BROADCAST_INPUT: u8 = 40;
const FOLD_RANK_LAUNCH_CLAUDE_CODE: u8 = 50;
const FOLD_RANK_SESSION_LOG: u8 = 60;
const FOLD_RANK_ZOOM: u8 = 70;

/// Horizontal room one tab bar icon button takes, its gap included, expressed in
/// rems: `ButtonSize::Default` is 22px square and the row's gap is
/// `DynamicSpacing::Base02`, both at a 16px rem.
const TAB_BAR_BUTTON_REMS: f32 = 24. / 16.;

/// Room the fold leaves to the tabs themselves, so that a full button row never
/// costs the pane the last readable tab title.
const TAB_BAR_TABS_REMS: f32 = 128. / 16.;

/// One entry of the terminal tab bar's button row, in the order it is drawn.
enum TabBarItem {
    /// A button the 「…」 menu can take over when the row does not fit.
    Foldable(FoldableTabBarButton),
    /// A button whose own popover menu would have to nest inside the 「…」 menu,
    /// so it is always drawn.
    Fixed(AnyElement),
}

/// A tab bar button together with the menu entry that stands in for it once it
/// has been folded away.
struct FoldableTabBarButton {
    button: AnyElement,
    menu_label: SharedString,
    menu_action: Box<dyn Action>,
    fold_rank: u8,
}

/// A tab bar shortcut whose whole behaviour is one action, so its button and its
/// folded menu entry can be built from the same action.
///
/// The click dispatches instead of running the handler directly, because
/// `Window::dispatch_action` defers to the end of the effect cycle: the handler
/// leases the workspace or the panel, which must not happen while this pane is
/// still leased by its own click listener.
fn action_tab_bar_button(
    id: &'static str,
    icon: IconName,
    label: &'static str,
    action: Box<dyn Action>,
    fold_rank: u8,
) -> FoldableTabBarButton {
    let button = IconButton::new(id, icon)
        .icon_size(IconSize::Small)
        .tooltip(Tooltip::for_action_title(label, action.as_ref()))
        .on_click({
            let action = action.boxed_clone();
            move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx)
        })
        .into_any_element();
    FoldableTabBarButton {
        button,
        menu_label: label.into(),
        menu_action: action,
        fold_rank,
    }
}

/// Splits the button row into what the pane is still wide enough to draw and
/// what the 「…」 menu takes over, keeping the drawing order on both sides.
///
/// `pane_width` is the width of the pane's previous layout; before the pane has
/// ever been laid out nothing folds, so a tab bar never opens up collapsed and
/// then expands.
fn fold_tab_bar_items(
    items: Vec<TabBarItem>,
    pane_width: Option<Pixels>,
    rem_size: Pixels,
) -> (Vec<TabBarItem>, Vec<(SharedString, Box<dyn Action>)>) {
    let Some(pane_width) = pane_width else {
        return (items, Vec::new());
    };
    let mut ranked_foldables = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| match item {
            TabBarItem::Foldable(button) => Some((button.fold_rank, index)),
            TabBarItem::Fixed(_) => None,
        })
        .collect::<Vec<_>>();
    let fixed_count = items.len() - ranked_foldables.len();

    let room_for_buttons = (pane_width - rem_size * TAB_BAR_TABS_REMS).max(Pixels::ZERO);
    let buttons_that_fit = (room_for_buttons / (rem_size * TAB_BAR_BUTTON_REMS)) as usize;
    let capacity = buttons_that_fit.saturating_sub(fixed_count);
    if ranked_foldables.len() <= capacity {
        return (items, Vec::new());
    }

    // The 「…」 trigger needs a slot of its own.
    let kept_count = capacity.saturating_sub(1);
    ranked_foldables.sort_by_key(|(fold_rank, index)| (cmp::Reverse(*fold_rank), *index));
    let kept = ranked_foldables
        .into_iter()
        .take(kept_count)
        .map(|(_, index)| index)
        .collect::<HashSet<_>>();

    let mut drawn = Vec::with_capacity(fixed_count + kept_count);
    let mut folded = Vec::new();
    for (index, item) in items.into_iter().enumerate() {
        match item {
            TabBarItem::Foldable(button) if !kept.contains(&index) => {
                folded.push((button.menu_label, button.menu_action));
            }
            item => drawn.push(item),
        }
    }
    (drawn, folded)
}

/// The 「…」 popover holding the buttons the tab bar had no room for.
///
/// `context` is the focus handle the entries dispatch into, so that
/// `terminal::ToggleSessionLog` and `workspace::ToggleZoom` reach this pane's
/// terminal rather than whatever happens to be focused elsewhere.
fn folded_buttons_menu(
    folded: Vec<(SharedString, Box<dyn Action>)>,
    context: FocusHandle,
) -> impl IntoElement {
    PopoverMenu::new("terminal-tab-bar-overflow")
        .trigger_with_tooltip(
            IconButton::new("terminal-tab-bar-overflow-trigger", IconName::Ellipsis)
                .icon_size(IconSize::Small),
            Tooltip::text("その他のボタン"),
        )
        .anchor(Anchor::TopRight)
        .menu(move |window, cx| {
            let folded = folded
                .iter()
                .map(|(label, action)| (label.clone(), action.boxed_clone()))
                .collect::<Vec<_>>();
            let context = context.clone();
            Some(ContextMenu::build(window, cx, |mut menu, _, _| {
                menu = menu.context(context);
                for (label, action) in folded {
                    menu = menu.action(label, action);
                }
                menu
            }))
        })
}

pub struct TerminalPanel {
    pub(crate) active_pane: Entity<Pane>,
    pub(crate) center: PaneGroup,
    focus_handle: FocusHandle,
    fs: Arc<dyn Fs>,
    workspace: WeakEntity<Workspace>,
    pending_serialization: Task<Option<()>>,
    /// 保存したレイアウトを組み立てている最中。この間の保存は「まだ空のパネル」を
    /// 書いてしまうので止める ([`deserialize_terminal_panel`])。
    pub(crate) restoring: bool,
    pending_terminals_to_add: usize,
    deferred_tasks: HashMap<TaskId, Task<()>>,
    assistant_enabled: bool,
    /// TeraTerm-style 全ペイン同時入力: what the focused terminal is typed into
    /// also goes to every other shell in this panel.
    broadcast_input: bool,
    active: bool,
}

impl TerminalPanel {
    pub fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let project = workspace.project();
        let pane = new_terminal_pane(workspace.weak_handle(), project.clone(), false, window, cx);
        let center = PaneGroup::new(pane.clone());
        let terminal_panel = Self {
            center,
            active_pane: pane,
            focus_handle: cx.focus_handle(),
            fs: workspace.app_state().fs.clone(),
            workspace: workspace.weak_handle(),
            pending_serialization: Task::ready(None),
            restoring: false,
            pending_terminals_to_add: 0,
            deferred_tasks: HashMap::default(),
            assistant_enabled: false,
            broadcast_input: false,
            active: false,
        };
        terminal_panel.apply_tab_bar_buttons(&terminal_panel.active_pane, cx);
        terminal_panel
    }

    pub fn set_assistant_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.assistant_enabled = enabled;
        for pane in self.center.panes() {
            self.apply_tab_bar_buttons(pane, cx);
        }
    }

    /// Whether typing into one of this panel's shells also types into the
    /// others.
    pub fn broadcast_input(&self) -> bool {
        self.broadcast_input
    }

    /// Flips 全ペイン同時入力. Turning it on with a single pane would have
    /// nothing to broadcast to, so it is refused rather than silently armed.
    pub fn toggle_broadcast_input(&mut self, cx: &mut Context<Self>) {
        self.set_broadcast_input(!self.broadcast_input && self.center.panes().len() > 1, cx);
    }

    fn set_broadcast_input(&mut self, broadcast_input: bool, cx: &mut Context<Self>) {
        if self.broadcast_input == broadcast_input {
            return;
        }
        self.broadcast_input = broadcast_input;
        self.refresh_tab_bar_buttons(cx);
        cx.notify();
    }

    /// Rebuilds every pane's tab bar. The buttons capture the panel's state by
    /// value, so what they show (同時入力 and the pane count it needs) only
    /// changes when they are built again.
    pub(crate) fn refresh_tab_bar_buttons(&self, cx: &mut Context<Self>) {
        for pane in self.center.panes() {
            self.apply_tab_bar_buttons(pane, cx);
        }
    }

    pub(crate) fn apply_tab_bar_buttons(
        &self,
        terminal_pane: &Entity<Pane>,
        cx: &mut Context<Self>,
    ) {
        let assistant_enabled = self.assistant_enabled;
        let broadcast_input = self.broadcast_input;
        let can_broadcast_input = self.center.panes().len() > 1;
        terminal_pane.update(cx, |pane, cx| {
            pane.set_render_tab_bar_buttons(cx, move |pane, window, cx| {
                let split_context = pane
                    .active_item()
                    .and_then(|item| item.downcast::<TerminalView>())
                    .map(|terminal_view| terminal_view.read(cx).focus_handle.clone());
                let has_focused_rename_editor = pane
                    .active_item()
                    .and_then(|item| item.downcast::<TerminalView>())
                    .is_some_and(|view| view.read(cx).rename_editor_is_focused(window, cx));
                // 起動ボタンだけはペインにフォーカスが無くても出す: Claude Code を
                // 始めたい人はまだターミナルを触っていないので、フォーカスしてから
                // でないと現れないボタンでは導線にならない。
                let launch_claude_code = launch_claude_code_button();
                // 記録トグルも同じ理由でフォーカス無しでも出す: 記録中かどうかは
                // ペインを触っていない人こそ知りたい情報で、ワンクリックで止め
                // られることに意味がある。
                let session_log_toggle = session_log_toggle_button(pane, cx);
                if !pane.should_render_tab_bar_buttons(window, cx) && !has_focused_rename_editor {
                    return (
                        None,
                        Some(
                            h_flex()
                                .gap(DynamicSpacing::Base02.rems(cx))
                                .debug_selector(|| "terminal_tab_bar_buttons".into())
                                .child(launch_claude_code.button)
                                .children(session_log_toggle.map(|toggle| toggle.button))
                                .into_any_element(),
                        ),
                    );
                }
                let focus_handle = pane.focus_handle(cx);
                let new_item_menu = PopoverMenu::new("terminal-tab-bar-popover-menu")
                    .trigger_with_tooltip(
                        IconButton::new("plus", IconName::Plus).icon_size(IconSize::Small),
                        Tooltip::text("新規…"),
                    )
                    .anchor(Anchor::TopRight)
                    .with_handle(pane.new_item_context_menu_handle.clone())
                    .menu({
                        let focus_handle = focus_handle.clone();
                        move |window, cx| {
                            let focus_handle = focus_handle.clone();
                            let menu = ContextMenu::build(window, cx, |menu, _, _| {
                                menu.context(focus_handle.clone())
                                    .action(
                                        "新規ターミナル",
                                        workspace::NewTerminal::default().boxed_clone(),
                                    )
                                    // We want the focus to go back to terminal panel once task modal is dismissed,
                                    // hence we focus that first. Otherwise, we'd end up without a focused element, as
                                    // context menu will be gone the moment we spawn the modal.
                                    .action(
                                        "タスクを実行",
                                        zed_actions::Spawn::modal().boxed_clone(),
                                    )
                                    .action(
                                        LAUNCH_CLAUDE_CODE_LABEL,
                                        zed_actions::terminal::LaunchClaudeCode { effort: None }
                                            .boxed_clone(),
                                    )
                                    .action(
                                        "Claude Code をサンドボックスの外で起動",
                                        zed_actions::terminal::LaunchClaudeCodeOutsideSandbox {
                                            effort: None,
                                        }
                                        .boxed_clone(),
                                    )
                                    .separator()
                                    .action(
                                        "クイック接続…",
                                        crate::session_tools::QuickConnect.boxed_clone(),
                                    )
                                    .action(
                                        "マクロを実行…",
                                        crate::session_tools::RunMacro.boxed_clone(),
                                    )
                                    .action(
                                        "マクロの編集…",
                                        crate::session_tools::EditMacros.boxed_clone(),
                                    )
                            });

                            Some(menu)
                        }
                    })
                    .into_any_element();
                let split_menu = PopoverMenu::new("terminal-pane-tab-bar-split")
                    .trigger_with_tooltip(
                        IconButton::new("terminal-pane-split", IconName::Split)
                            .icon_size(IconSize::Small),
                        Tooltip::text("ペインを分割"),
                    )
                    .anchor(Anchor::TopRight)
                    .with_handle(pane.split_item_context_menu_handle.clone())
                    .menu({
                        let split_context = split_context.clone();
                        move |window, cx| {
                            ContextMenu::build(window, cx, |menu, _, _| {
                                menu.when_some(split_context.clone(), |menu, split_context| {
                                    menu.context(split_context)
                                })
                                .action("右に分割", SplitRight::default().boxed_clone())
                                .action("左に分割", SplitLeft::default().boxed_clone())
                                .action("上に分割", SplitUp::default().boxed_clone())
                                .action("下に分割", SplitDown::default().boxed_clone())
                                .separator()
                                .action(
                                    "2×2 に分割",
                                    SplitGrid {
                                        columns: 2,
                                        rows: 2,
                                    }
                                    .boxed_clone(),
                                )
                                .action(
                                    "横 3 分割",
                                    SplitGrid {
                                        columns: 3,
                                        rows: 1,
                                    }
                                    .boxed_clone(),
                                )
                            })
                            .into()
                        }
                    })
                    .into_any_element();
                let zoomed = pane.is_zoomed();
                let zoom_label = if zoomed {
                    "ズーム解除"
                } else {
                    "ズーム"
                };
                let zoom_button = FoldableTabBarButton {
                    button: IconButton::new("toggle_zoom", IconName::Maximize)
                        .icon_size(IconSize::Small)
                        .toggle_state(zoomed)
                        .selected_icon(IconName::Minimize)
                        .on_click(cx.listener(|pane, _, window, cx| {
                            pane.toggle_zoom(&workspace::ToggleZoom, window, cx);
                        }))
                        .tooltip(move |_window, cx| {
                            Tooltip::for_action(zoom_label, &ToggleZoom, cx)
                        })
                        .into_any_element(),
                    menu_label: zoom_label.into(),
                    menu_action: Box::new(ToggleZoom),
                    fold_rank: FOLD_RANK_ZOOM,
                };

                let mut items = vec![
                    TabBarItem::Foldable(launch_claude_code),
                    TabBarItem::Foldable(action_tab_bar_button(
                        "terminal-quick-connect",
                        IconName::Server,
                        "クイック接続…",
                        Box::new(crate::session_tools::QuickConnect),
                        FOLD_RANK_QUICK_CONNECT,
                    )),
                    TabBarItem::Foldable(action_tab_bar_button(
                        "terminal-run-macro",
                        IconName::PlayOutlined,
                        "マクロを実行…",
                        Box::new(crate::session_tools::RunMacro),
                        FOLD_RANK_RUN_MACRO,
                    )),
                ];
                items.push(TabBarItem::Foldable(action_tab_bar_button(
                    "terminal-file-transfer",
                    IconName::ArrowRightLeft,
                    "ファイル転送…",
                    Box::new(zed_actions::terminal::OpenFileTransfer::default()),
                    FOLD_RANK_FILE_TRANSFER,
                )));
                items.extend(session_log_toggle.map(TabBarItem::Foldable));
                items.push(TabBarItem::Fixed(new_item_menu));
                if assistant_enabled && let Some(focus_handle) = split_context.clone() {
                    items.push(TabBarItem::Foldable(FoldableTabBarButton {
                        button: InlineAssistTabBarButton { focus_handle }.into_any_element(),
                        menu_label: "インラインアシスト".into(),
                        menu_action: Box::new(InlineAssist::default()),
                        fold_rank: FOLD_RANK_INLINE_ASSIST,
                    }));
                }
                if can_broadcast_input || broadcast_input {
                    let broadcast_label = if broadcast_input {
                        "全ペインへの同時入力をやめる"
                    } else {
                        "全ペインに同時入力"
                    };
                    items.push(TabBarItem::Foldable(FoldableTabBarButton {
                        button: IconButton::new("terminal-broadcast-input", IconName::Keyboard)
                            .icon_size(IconSize::Small)
                            .toggle_state(broadcast_input)
                            .tooltip(move |_window, cx| {
                                Tooltip::for_action(
                                    broadcast_label,
                                    &crate::ToggleBroadcastInput,
                                    cx,
                                )
                            })
                            // The handler leases the panel, which must not
                            // happen while this pane is still leased by its
                            // own click listener; dispatching defers it to
                            // the end of the effect cycle.
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(crate::ToggleBroadcastInput), cx);
                            })
                            .into_any_element(),
                        menu_label: broadcast_label.into(),
                        menu_action: Box::new(crate::ToggleBroadcastInput),
                        fold_rank: FOLD_RANK_BROADCAST_INPUT,
                    }));
                }
                items.push(TabBarItem::Fixed(split_menu));
                items.push(TabBarItem::Foldable(zoom_button));

                let (drawn, folded) =
                    fold_tab_bar_items(items, pane.last_layout_width(), window.rem_size());
                let menu_context = split_context.unwrap_or(focus_handle);
                let right_children = h_flex()
                    .gap(DynamicSpacing::Base02.rems(cx))
                    .debug_selector(|| "terminal_tab_bar_buttons".into())
                    .children(drawn.into_iter().map(|item| match item {
                        TabBarItem::Foldable(button) => button.button,
                        TabBarItem::Fixed(element) => element,
                    }))
                    .when(!folded.is_empty(), |this| {
                        this.child(
                            div()
                                .debug_selector(|| "terminal_tab_bar_overflow".into())
                                .child(folded_buttons_menu(folded, menu_context)),
                        )
                    })
                    .into_any_element()
                    .into();
                (None, right_children)
            });
        });
    }

    /// The view hosting `terminal`, if it lives in this panel. Used after
    /// spawning a quick-connect terminal, whose spawn only hands back the
    /// `Terminal` itself.
    pub fn terminal_view_for(
        &self,
        terminal: &Entity<Terminal>,
        cx: &App,
    ) -> Option<Entity<TerminalView>> {
        self.center.panes().into_iter().find_map(|pane| {
            pane.read(cx).items().find_map(|item| {
                let view = item.downcast::<TerminalView>()?;
                (view.read(cx).terminal() == terminal).then_some(view)
            })
        })
    }

    fn serialization_key(workspace: &Workspace) -> Option<String> {
        workspace
            .database_id()
            .map(|id| i64::from(id).to_string())
            .or(workspace.session_id())
            .map(|id| format!("{:?}-{:?}", TERMINAL_PANEL_KEY, id))
    }

    /// 復元しなかったアイテムの行の掃除は、中央とすべてのパネルの復元が揃ってから
    /// ワークスペースが 1 回だけ走らせる (ADR-F37)。ここでやることは待ち合わせへの
    /// 参加だけで、失敗しても必ず抜ける (抜けないとそのウィンドウで掃除が止まる)。
    ///
    /// このパネルが workspace に載る (`add_panel`) のは `load` が返ってからなので、
    /// ここで数えを戻した時点ではまだタブが「どのペインにも居ない」ように見える。
    /// 掃除が実際に走るのは zed の `initialize_panels` が全パネルを載せ終えてから
    /// (向こうでもう 1 つ数えている) なので、この順序に依存している。
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace
            .update(&mut cx, |workspace, _| workspace.begin_item_restore())
            .log_err();
        let panel = Self::load_inner(workspace.clone(), cx.clone()).await;
        workspace
            .update_in(&mut cx, |workspace, window, cx| {
                workspace.end_item_restore(window, cx);
            })
            .log_err();
        panel
    }

    async fn load_inner(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        let mut terminal_panel = None;

        if let Some((database_id, serialization_key, kvp)) = workspace
            .read_with(&cx, |workspace, cx| {
                workspace
                    .database_id()
                    .zip(TerminalPanel::serialization_key(workspace))
                    .map(|(id, key)| (id, key, KeyValueStore::global(cx)))
            })
            .ok()
            .flatten()
            && let Some(serialized_panel) = cx
                .background_spawn(async move { kvp.read_kvp(&serialization_key) })
                .await
                .log_err()
                .flatten()
                .map(|panel| serde_json::from_str::<SerializedTerminalPanel>(&panel))
                .transpose()
                .log_err()
                .flatten()
            && let Ok(serialized) = workspace
                .update_in(&mut cx, |workspace, window, cx| {
                    deserialize_terminal_panel(
                        workspace.weak_handle(),
                        workspace.project().clone(),
                        database_id,
                        serialized_panel,
                        window,
                        cx,
                    )
                })?
                .await
        {
            terminal_panel = Some(serialized);
        }

        let terminal_panel = if let Some(panel) = terminal_panel {
            panel
        } else {
            workspace.update_in(&mut cx, |workspace, window, cx| {
                cx.new(|cx| TerminalPanel::new(workspace, window, cx))
            })?
        };

        if let Some(workspace) = workspace.upgrade() {
            workspace.update(&mut cx, |workspace, _| {
                workspace.set_terminal_provider(TerminalProvider(terminal_panel.clone()))
            });
        }

        if let Some(workspace) = workspace.upgrade() {
            let should_focus = workspace
                .update_in(&mut cx, |workspace, window, cx| {
                    workspace.active_item(cx).is_none()
                        && workspace
                            .is_dock_at_position_open(terminal_panel.position(window, cx), cx)
                })
                .unwrap_or(false);

            if should_focus {
                terminal_panel
                    .update_in(&mut cx, |panel, window, cx| {
                        panel.active_pane.update(cx, |pane, cx| {
                            pane.focus_active_item(window, cx);
                        });
                    })
                    .ok();
            }
        }
        Ok(terminal_panel)
    }

    fn handle_pane_event(
        &mut self,
        pane: &Entity<Pane>,
        event: &pane::Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            pane::Event::ActivateItem { .. } => self.serialize(cx),
            pane::Event::RemovedItem { .. } => self.serialize(cx),
            pane::Event::Remove { focus_on_pane } => {
                let pane_count_before_removal = self.center.panes().len();
                let _removal_result = self.center.remove(pane, cx);
                // 同時入力 has nothing left to broadcast to once a single pane
                // remains, and leaving it armed would surprise the next split.
                if self.center.panes().len() <= 1 {
                    self.set_broadcast_input(false, cx);
                }
                self.refresh_tab_bar_buttons(cx);
                if pane_count_before_removal == 1 {
                    self.center.first_pane().update(cx, |pane, cx| {
                        pane.set_zoomed(false, cx);
                    });
                    cx.emit(PanelEvent::Close);
                } else if let Some(focus_on_pane) =
                    focus_on_pane.as_ref().or_else(|| self.center.panes().pop())
                {
                    focus_on_pane.focus_handle(cx).focus(window, cx);
                }
            }
            pane::Event::ZoomIn => {
                for pane in self.center.panes() {
                    pane.update(cx, |pane, cx| {
                        pane.set_zoomed(true, cx);
                    })
                }
                cx.emit(PanelEvent::ZoomIn);
                cx.notify();
            }
            pane::Event::ZoomOut => {
                for pane in self.center.panes() {
                    pane.update(cx, |pane, cx| {
                        pane.set_zoomed(false, cx);
                    })
                }
                cx.emit(PanelEvent::ZoomOut);
                cx.notify();
            }
            pane::Event::AddItem { item } => {
                if let Some(workspace) = self.workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        item.added_to_pane(workspace, pane.clone(), window, cx)
                    })
                }
                self.serialize(cx);
            }
            &pane::Event::Split { direction, mode } => {
                match mode {
                    SplitMode::ClonePane | SplitMode::EmptyPane => {
                        let clone = matches!(mode, SplitMode::ClonePane);
                        let new_pane = self.new_pane_with_active_terminal(clone, window, cx);
                        let pane = pane.clone();
                        cx.spawn_in(window, async move |panel, cx| {
                            let Some(new_pane) = new_pane.await else {
                                return;
                            };
                            panel
                                .update_in(cx, |panel, window, cx| {
                                    panel.center.split(&pane, &new_pane, direction, cx);
                                    panel.refresh_tab_bar_buttons(cx);
                                    window.focus(&new_pane.focus_handle(cx), cx);
                                })
                                .ok();
                        })
                        .detach();
                    }
                    SplitMode::MovePane => {
                        let Some(item) =
                            pane.update(cx, |pane, cx| pane.take_active_item(window, cx))
                        else {
                            return;
                        };
                        let Ok(project) = self
                            .workspace
                            .update(cx, |workspace, _| workspace.project().clone())
                        else {
                            return;
                        };
                        let new_pane =
                            new_terminal_pane(self.workspace.clone(), project, false, window, cx);
                        new_pane.update(cx, |pane, cx| {
                            pane.add_item(item, true, true, None, window, cx);
                        });
                        self.center.split(&pane, &new_pane, direction, cx);
                        self.refresh_tab_bar_buttons(cx);
                        window.focus(&new_pane.focus_handle(cx), cx);
                    }
                };
            }
            pane::Event::Focus => {
                self.active_pane = pane.clone();
            }
            pane::Event::ItemPinned | pane::Event::ItemUnpinned => {
                self.serialize(cx);
            }

            _ => {}
        }
    }

    /// Splits the active pane into a `columns` × `rows` grid of fresh shells that
    /// start in the active terminal's directory (the same rule as a single split).
    ///
    /// Each split has to wait for its shell to be created before the next one
    /// has something to attach to, so the splits run one after another rather
    /// than concurrently. Focus goes back to the pane the grid grew out of,
    /// which is the top-left one.
    pub fn split_grid(
        &mut self,
        columns: u8,
        rows: u8,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let columns = columns.clamp(1, MAX_GRID_SIDE) as usize;
        let rows = rows.clamp(1, MAX_GRID_SIDE) as usize;
        if columns * rows <= 1 {
            return;
        }
        let origin = self.active_pane.clone();

        cx.spawn_in(window, async move |panel, cx| {
            let completed = Self::lay_out_grid(&panel, &origin, columns, rows, cx).await;
            if !completed {
                log::warn!("terminal grid split stopped early: a terminal could not be created");
            }
            panel
                .update_in(cx, |_, window, cx| {
                    window.focus(&origin.focus_handle(cx), cx);
                })
                .ok();
        })
        .detach();
    }

    /// Builds the top row left to right, then grows every column downwards.
    /// Returns whether every split it wanted to make actually happened.
    async fn lay_out_grid(
        panel: &WeakEntity<Self>,
        origin: &Entity<Pane>,
        columns: usize,
        rows: usize,
        cx: &mut AsyncWindowContext,
    ) -> bool {
        let mut column_tops = vec![origin.clone()];
        for _ in 1..columns {
            let Some(previous) = column_tops.last().cloned() else {
                return false;
            };
            let Some(new_pane) =
                Self::split_pane_with_new_terminal(panel, &previous, SplitDirection::Right, cx)
                    .await
            else {
                return false;
            };
            column_tops.push(new_pane);
        }

        for column_top in column_tops {
            let mut previous = column_top;
            for _ in 1..rows {
                let Some(new_pane) =
                    Self::split_pane_with_new_terminal(panel, &previous, SplitDirection::Down, cx)
                        .await
                else {
                    return false;
                };
                previous = new_pane;
            }
        }
        true
    }

    async fn split_pane_with_new_terminal(
        panel: &WeakEntity<Self>,
        old_pane: &Entity<Pane>,
        direction: SplitDirection,
        cx: &mut AsyncWindowContext,
    ) -> Option<Entity<Pane>> {
        let new_pane = panel
            .update_in(cx, |panel, window, cx| {
                panel.new_pane_with_active_terminal(true, window, cx)
            })
            .ok()?
            .await?;
        panel
            .update(cx, |panel, cx| {
                panel.center.split(old_pane, &new_pane, direction, cx);
                panel.refresh_tab_bar_buttons(cx);
            })
            .ok()?;
        Some(new_pane)
    }

    fn new_pane_with_active_terminal(
        &mut self,
        clone: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Pane>>> {
        let Some(workspace) = self.workspace.upgrade() else {
            return Task::ready(None);
        };
        let workspace = workspace.read(cx);
        let database_id = workspace.database_id();
        let weak_workspace = self.workspace.clone();
        let project = workspace.project().clone();
        let active_pane = &self.active_pane;
        let terminal_view = if clone {
            active_pane
                .read(cx)
                .active_item()
                .and_then(|item| item.downcast::<TerminalView>())
        } else {
            None
        };
        let working_directory = if clone {
            terminal_view
                .as_ref()
                .and_then(|terminal_view| {
                    terminal_view
                        .read(cx)
                        .terminal()
                        .read(cx)
                        .working_directory()
                })
                .or_else(|| default_working_directory(workspace, cx))
        } else {
            default_working_directory(workspace, cx)
        };

        let is_zoomed = if clone {
            active_pane.read(cx).is_zoomed()
        } else {
            false
        };
        cx.spawn_in(window, async move |panel, cx| {
            let terminal = project
                .update(cx, |project, cx| match terminal_view {
                    Some(view) => project.clone_terminal(
                        &view.read(cx).terminal.clone(),
                        cx,
                        working_directory,
                    ),
                    None => project.create_terminal_shell(working_directory, cx),
                })
                .await
                .log_err()?;

            panel
                .update_in(cx, move |terminal_panel, window, cx| {
                    let terminal_view = Box::new(cx.new(|cx| {
                        TerminalView::new(
                            terminal.clone(),
                            weak_workspace.clone(),
                            database_id,
                            project.downgrade(),
                            window,
                            cx,
                        )
                    }));
                    let pane = new_terminal_pane(weak_workspace, project, is_zoomed, window, cx);
                    terminal_panel.apply_tab_bar_buttons(&pane, cx);
                    pane.update(cx, |pane, cx| {
                        pane.add_item(terminal_view, true, true, None, window, cx);
                    });
                    Some(pane)
                })
                .ok()
                .flatten()
        })
    }

    pub fn open_terminal(
        workspace: &mut Workspace,
        action: &workspace::OpenTerminal,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(terminal_panel) = workspace.panel::<Self>(cx) else {
            return;
        };

        terminal_panel
            .update(cx, |panel, cx| {
                if action.local {
                    panel.add_local_terminal_shell(RevealStrategy::Always, window, cx)
                } else {
                    panel.add_terminal_shell(
                        Some(action.working_directory.clone()),
                        RevealStrategy::Always,
                        window,
                        cx,
                    )
                }
            })
            .detach_and_log_err(cx);
    }

    pub fn spawn_task(
        &mut self,
        task: &SpawnInTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        let Some(workspace) = self.workspace.upgrade() else {
            return Task::ready(Err(anyhow!("failed to read workspace")));
        };

        let project = workspace.read(cx).project().read(cx);

        if project.is_via_collab() {
            return Task::ready(Err(anyhow!("cannot spawn tasks as a guest")));
        }

        let remote_client = project.remote_client();
        let is_windows = project.path_style(cx).is_windows();
        let remote_shell = remote_client
            .as_ref()
            .and_then(|remote_client| remote_client.read(cx).shell());

        let shell = if let Some(remote_shell) = remote_shell
            && task.shell == Shell::System
        {
            Shell::Program(remote_shell)
        } else {
            task.shell.clone()
        };

        let task = prepare_task_for_spawn(task, &shell, is_windows);

        if task.allow_concurrent_runs && task.use_new_terminal {
            return self.spawn_in_new_terminal(task, window, cx);
        }

        let mut terminals_for_task = self.terminals_for_task(&task.full_label, cx);
        let Some(existing) = terminals_for_task.pop() else {
            return self.spawn_in_new_terminal(task, window, cx);
        };

        let (existing_item_index, task_pane, existing_terminal) = existing;
        if task.allow_concurrent_runs {
            return self.replace_terminal(
                task,
                task_pane,
                existing_item_index,
                existing_terminal,
                window,
                cx,
            );
        }

        let (tx, rx) = oneshot::channel();

        self.deferred_tasks.insert(
            task.id.clone(),
            cx.spawn_in(window, async move |terminal_panel, cx| {
                wait_for_terminals_tasks(terminals_for_task, cx).await;
                let task = terminal_panel.update_in(cx, |terminal_panel, window, cx| {
                    if task.use_new_terminal {
                        terminal_panel.spawn_in_new_terminal(task, window, cx)
                    } else {
                        terminal_panel.replace_terminal(
                            task,
                            task_pane,
                            existing_item_index,
                            existing_terminal,
                            window,
                            cx,
                        )
                    }
                });
                if let Ok(task) = task {
                    tx.send(task.await).ok();
                }
            }),
        );

        cx.spawn(async move |_, _| rx.await?)
    }

    fn spawn_in_new_terminal(
        &mut self,
        spawn_task: SpawnInTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        let reveal = spawn_task.reveal;
        let reveal_target = spawn_task.reveal_target;
        match reveal_target {
            RevealTarget::Center => self
                .workspace
                .update(cx, |workspace, cx| {
                    Self::add_center_terminal(workspace, window, cx, |project, cx| {
                        project.create_terminal_task(spawn_task, cx)
                    })
                })
                .unwrap_or_else(|e| Task::ready(Err(e))),
            RevealTarget::Dock => self.add_terminal_task(spawn_task, reveal, window, cx),
        }
    }

    /// Create a new Terminal in the current working directory or the user's home directory
    fn new_terminal(
        workspace: &mut Workspace,
        action: &workspace::NewTerminal,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let center_pane = workspace.active_pane();
        let center_pane_has_focus = center_pane.focus_handle(cx).contains_focused(window, cx);
        let active_center_item_is_terminal = center_pane
            .read(cx)
            .active_item()
            .is_some_and(|item| item.downcast::<TerminalView>().is_some());

        if center_pane_has_focus && active_center_item_is_terminal {
            let working_directory = default_working_directory(workspace, cx);
            let local = action.local;
            Self::add_center_terminal(workspace, window, cx, move |project, cx| {
                if local {
                    project.create_local_terminal(cx)
                } else {
                    project.create_terminal_shell(working_directory, cx)
                }
            })
            .detach_and_log_err(cx);
            return;
        }

        let Some(terminal_panel) = workspace.panel::<Self>(cx) else {
            return;
        };

        terminal_panel
            .update(cx, |this, cx| {
                if action.local {
                    this.add_local_terminal_shell(RevealStrategy::Always, window, cx)
                } else {
                    this.add_terminal_shell(
                        default_working_directory(workspace, cx),
                        RevealStrategy::Always,
                        window,
                        cx,
                    )
                }
            })
            .detach_and_log_err(cx);
    }

    fn terminals_for_task(
        &self,
        label: &str,
        cx: &mut App,
    ) -> Vec<(usize, Entity<Pane>, Entity<TerminalView>)> {
        let Some(workspace) = self.workspace.upgrade() else {
            return Vec::new();
        };

        let pane_terminal_views = |pane: Entity<Pane>| {
            pane.read(cx)
                .items()
                .enumerate()
                .filter_map(|(index, item)| Some((index, item.act_as::<TerminalView>(cx)?)))
                .filter_map(|(index, terminal_view)| {
                    let task_state = terminal_view.read(cx).terminal().read(cx).task()?;
                    if &task_state.spawned_task.full_label == label {
                        Some((index, terminal_view))
                    } else {
                        None
                    }
                })
                .map(move |(index, terminal_view)| (index, pane.clone(), terminal_view))
        };

        self.center
            .panes()
            .into_iter()
            .cloned()
            .flat_map(pane_terminal_views)
            .chain(
                workspace
                    .read(cx)
                    .panes()
                    .iter()
                    .cloned()
                    .flat_map(pane_terminal_views),
            )
            .sorted_by_key(|(_, _, terminal_view)| terminal_view.entity_id())
            .collect()
    }

    fn activate_terminal_view(
        &self,
        pane: &Entity<Pane>,
        item_index: usize,
        focus: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        pane.update(cx, |pane, cx| {
            pane.activate_item(item_index, true, focus, window, cx)
        })
    }

    pub fn add_center_terminal(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
        create_terminal: impl FnOnce(
            &mut Project,
            &mut Context<Project>,
        ) -> Task<Result<Entity<Terminal>>>
        + 'static,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        if !is_enabled_in_workspace(workspace, cx) {
            return Task::ready(Err(anyhow!(
                "terminal not yet supported for remote projects"
            )));
        }
        let project = workspace.project().downgrade();
        cx.spawn_in(window, async move |workspace, cx| {
            let terminal = project.update(cx, create_terminal)?.await?;

            workspace.update_in(cx, |workspace, window, cx| {
                let terminal_view = cx.new(|cx| {
                    TerminalView::new(
                        terminal.clone(),
                        workspace.weak_handle(),
                        workspace.database_id(),
                        workspace.project().downgrade(),
                        window,
                        cx,
                    )
                });
                // Don't steal focus from an open modal (e.g. the command palette):
                // a background terminal can finish starting up after the user has
                // moved on, and focusing it would dismiss whatever they opened.
                let focus_item = !workspace.has_active_modal(window, cx);
                workspace.add_item_to_active_pane(
                    Box::new(terminal_view),
                    None,
                    focus_item,
                    window,
                    cx,
                );
            })?;
            Ok(terminal.downgrade())
        })
    }

    pub fn add_terminal_task(
        &mut self,
        task: SpawnInTerminal,
        reveal_strategy: RevealStrategy,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        self.add_terminal_with(reveal_strategy, window, cx, move |project, cx| {
            project.create_terminal_task(task, cx)
        })
    }

    /// [`Self::add_terminal_task`] with the terminal built by `create_terminal`,
    /// such as a sandboxed session from `Project::create_sandboxed_terminal_task`.
    pub fn add_terminal_with(
        &mut self,
        reveal_strategy: RevealStrategy,
        window: &mut Window,
        cx: &mut Context<Self>,
        create_terminal: impl FnOnce(
            &mut Project,
            &mut Context<Project>,
        ) -> Task<Result<Entity<Terminal>>>
        + 'static,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |terminal_panel, cx| {
            if workspace.update(cx, |workspace, cx| !is_enabled_in_workspace(workspace, cx))? {
                anyhow::bail!("terminal not yet supported for remote projects");
            }
            let pane = terminal_panel.update(cx, |terminal_panel, _| {
                terminal_panel.pending_terminals_to_add += 1;
                terminal_panel.active_pane.clone()
            })?;
            let project = workspace.read_with(cx, |workspace, _| workspace.project().clone())?;
            let terminal = project.update(cx, create_terminal).await?;
            let result = workspace.update_in(cx, |workspace, window, cx| {
                let terminal_view = Box::new(cx.new(|cx| {
                    TerminalView::new(
                        terminal.clone(),
                        workspace.weak_handle(),
                        workspace.database_id(),
                        workspace.project().downgrade(),
                        window,
                        cx,
                    )
                }));

                let reveal_strategy =
                    reveal_strategy_for_started_terminal(reveal_strategy, workspace, window, cx);
                match reveal_strategy {
                    RevealStrategy::Always => {
                        workspace.focus_panel::<Self>(window, cx);
                    }
                    RevealStrategy::NoFocus => {
                        workspace.open_panel::<Self>(window, cx);
                    }
                    RevealStrategy::Never => {}
                }

                pane.update(cx, |pane, cx| {
                    let focus = matches!(reveal_strategy, RevealStrategy::Always);
                    pane.add_item(terminal_view, true, focus, None, window, cx);
                });

                Ok(terminal.downgrade())
            })?;
            terminal_panel.update(cx, |terminal_panel, cx| {
                terminal_panel.pending_terminals_to_add =
                    terminal_panel.pending_terminals_to_add.saturating_sub(1);
                terminal_panel.serialize(cx)
            })?;
            result
        })
    }

    fn add_terminal_shell(
        &mut self,
        cwd: Option<PathBuf>,
        reveal_strategy: RevealStrategy,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        self.add_terminal_shell_internal(false, cwd, reveal_strategy, window, cx)
    }

    fn add_local_terminal_shell(
        &mut self,
        reveal_strategy: RevealStrategy,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        self.add_terminal_shell_internal(true, None, reveal_strategy, window, cx)
    }

    fn add_terminal_shell_internal(
        &mut self,
        force_local: bool,
        cwd: Option<PathBuf>,
        reveal_strategy: RevealStrategy,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        let workspace = self.workspace.clone();

        cx.spawn_in(window, async move |terminal_panel, cx| {
            if workspace.update(cx, |workspace, cx| !is_enabled_in_workspace(workspace, cx))? {
                anyhow::bail!("terminal not yet supported for collaborative projects");
            }
            let pane = terminal_panel.update(cx, |terminal_panel, _| {
                terminal_panel.pending_terminals_to_add += 1;
                terminal_panel.active_pane.clone()
            })?;
            let project = workspace.read_with(cx, |workspace, _| workspace.project().clone())?;
            let terminal = if force_local {
                project
                    .update(cx, |project, cx| project.create_local_terminal(cx))
                    .await
            } else {
                project
                    .update(cx, |project, cx| project.create_terminal_shell(cwd, cx))
                    .await
            };

            match terminal {
                Ok(terminal) => {
                    let result = workspace.update_in(cx, |workspace, window, cx| {
                        let terminal_view = Box::new(cx.new(|cx| {
                            TerminalView::new(
                                terminal.clone(),
                                workspace.weak_handle(),
                                workspace.database_id(),
                                workspace.project().downgrade(),
                                window,
                                cx,
                            )
                        }));

                        let reveal_strategy = reveal_strategy_for_started_terminal(
                            reveal_strategy,
                            workspace,
                            window,
                            cx,
                        );
                        match reveal_strategy {
                            RevealStrategy::Always => {
                                workspace.focus_panel::<Self>(window, cx);
                            }
                            RevealStrategy::NoFocus => {
                                workspace.open_panel::<Self>(window, cx);
                            }
                            RevealStrategy::Never => {}
                        }

                        pane.update(cx, |pane, cx| {
                            let focus = matches!(reveal_strategy, RevealStrategy::Always);
                            pane.add_item(terminal_view, true, focus, None, window, cx);
                        });

                        Ok(terminal.downgrade())
                    })?;
                    terminal_panel.update(cx, |terminal_panel, cx| {
                        terminal_panel.pending_terminals_to_add =
                            terminal_panel.pending_terminals_to_add.saturating_sub(1);
                        terminal_panel.serialize(cx)
                    })?;
                    result
                }
                Err(error) => {
                    pane.update_in(cx, |pane, window, cx| {
                        let focus = pane.has_focus(window, cx);
                        let failed_to_spawn = cx.new(|cx| FailedToSpawnTerminal {
                            error: error.to_string(),
                            focus_handle: cx.focus_handle(),
                        });
                        pane.add_item(Box::new(failed_to_spawn), true, focus, None, window, cx);
                    })?;
                    Err(error)
                }
            }
        })
    }

    fn serialize(&mut self, cx: &mut Context<Self>) {
        if self.restoring {
            return;
        }
        let Some(serialization_key) = self
            .workspace
            .read_with(cx, |workspace, _| {
                TerminalPanel::serialization_key(workspace)
            })
            .ok()
            .flatten()
        else {
            return;
        };
        let kvp = KeyValueStore::global(cx);
        self.pending_serialization = cx.spawn(async move |terminal_panel, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(50))
                .await;
            let terminal_panel = terminal_panel.upgrade()?;
            let items = terminal_panel.update(cx, |terminal_panel, cx| {
                SerializedItems::WithSplits(serialize_pane_group(
                    &terminal_panel.center,
                    &terminal_panel.active_pane,
                    cx,
                ))
            });
            cx.background_spawn(
                async move {
                    kvp.write_kvp(
                        serialization_key,
                        serde_json::to_string(&SerializedTerminalPanel {
                            items,
                            active_item_id: None,
                        })?,
                    )
                    .await?;
                    anyhow::Ok(())
                }
                .log_err(),
            )
            .await;
            Some(())
        });
    }

    fn replace_terminal(
        &self,
        spawn_task: SpawnInTerminal,
        task_pane: Entity<Pane>,
        terminal_item_index: usize,
        terminal_to_replace: Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        let reveal = spawn_task.reveal;
        let task_workspace = self.workspace.clone();
        cx.spawn_in(window, async move |terminal_panel, cx| {
            let project = terminal_panel.update(cx, |this, cx| {
                this.workspace
                    .update(cx, |workspace, _| workspace.project().clone())
            })??;
            let new_terminal = project
                .update(cx, |project, cx| {
                    project.create_terminal_task(spawn_task, cx)
                })
                .await?;
            terminal_to_replace.update_in(cx, |terminal_to_replace, window, cx| {
                terminal_to_replace.set_terminal(new_terminal.clone(), window, cx);
            })?;

            let reveal_target = terminal_panel.update(cx, |panel, _| {
                if panel.center.panes().iter().any(|p| **p == task_pane) {
                    RevealTarget::Dock
                } else {
                    RevealTarget::Center
                }
            })?;

            match reveal {
                RevealStrategy::Always => match reveal_target {
                    RevealTarget::Center => {
                        task_workspace.update_in(cx, |workspace, window, cx| {
                            let did_activate = workspace.activate_item(
                                &terminal_to_replace,
                                true,
                                true,
                                window,
                                cx,
                            );

                            anyhow::ensure!(did_activate, "Failed to retrieve terminal pane");

                            anyhow::Ok(())
                        })??;
                    }
                    RevealTarget::Dock => {
                        terminal_panel.update_in(cx, |terminal_panel, window, cx| {
                            terminal_panel.activate_terminal_view(
                                &task_pane,
                                terminal_item_index,
                                true,
                                window,
                                cx,
                            )
                        })?;

                        cx.spawn(async move |cx| {
                            task_workspace
                                .update_in(cx, |workspace, window, cx| {
                                    workspace.focus_panel::<Self>(window, cx)
                                })
                                .ok()
                        })
                        .detach();
                    }
                },
                RevealStrategy::NoFocus => match reveal_target {
                    RevealTarget::Center => {
                        task_workspace.update_in(cx, |workspace, window, cx| {
                            workspace.active_pane().focus_handle(cx).focus(window, cx);
                        })?;
                    }
                    RevealTarget::Dock => {
                        terminal_panel.update_in(cx, |terminal_panel, window, cx| {
                            terminal_panel.activate_terminal_view(
                                &task_pane,
                                terminal_item_index,
                                false,
                                window,
                                cx,
                            )
                        })?;

                        cx.spawn(async move |cx| {
                            task_workspace
                                .update_in(cx, |workspace, window, cx| {
                                    workspace.open_panel::<Self>(window, cx)
                                })
                                .ok()
                        })
                        .detach();
                    }
                },
                RevealStrategy::Never => {}
            }

            Ok(new_terminal.downgrade())
        })
    }

    fn has_no_terminals(&self, cx: &App) -> bool {
        self.active_pane.read(cx).items_len() == 0 && self.pending_terminals_to_add == 0
    }

    pub fn assistant_enabled(&self) -> bool {
        self.assistant_enabled
    }

    /// Returns all panes in the terminal panel.
    pub fn panes(&self) -> Vec<&Entity<Pane>> {
        self.center.panes()
    }

    /// Returns all non-empty terminal selections from all terminal views in all panes.
    pub fn terminal_selections(&self, cx: &App) -> Vec<String> {
        self.center
            .panes()
            .iter()
            .flat_map(|pane| {
                pane.read(cx).items().filter_map(|item| {
                    let terminal_view = item.downcast::<crate::TerminalView>()?;
                    terminal_view
                        .read(cx)
                        .terminal()
                        .read(cx)
                        .last_content
                        .selection_text
                        .clone()
                        .filter(|text| !text.is_empty())
                })
            })
            .collect()
    }

    fn is_enabled(&self, cx: &App) -> bool {
        self.workspace
            .upgrade()
            .is_some_and(|workspace| is_enabled_in_workspace(workspace.read(cx), cx))
    }

    fn activate_pane_in_direction(
        &mut self,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(pane) = self
            .center
            .find_pane_in_direction(&self.active_pane, direction, cx)
        {
            window.focus(&pane.focus_handle(cx), cx);
        } else {
            self.workspace
                .update(cx, |workspace, cx| {
                    workspace.activate_pane_in_direction(direction, window, cx)
                })
                .ok();
        }
    }

    fn swap_pane_in_direction(&mut self, direction: SplitDirection, cx: &mut Context<Self>) {
        if let Some(to) = self
            .center
            .find_pane_in_direction(&self.active_pane, direction, cx)
            .cloned()
        {
            self.center.swap(&self.active_pane, &to, cx);
            cx.notify();
        }
    }

    fn move_pane_to_border(&mut self, direction: SplitDirection, cx: &mut Context<Self>) {
        if self
            .center
            .move_to_border(&self.active_pane, direction, cx)
            .unwrap()
        {
            cx.notify();
        }
    }
}

/// Prepares a `SpawnInTerminal` by computing the command, args, and command_label
/// based on the shell configuration. This is a pure function that can be tested
/// without spawning actual terminals.
pub fn prepare_task_for_spawn(
    task: &SpawnInTerminal,
    shell: &Shell,
    is_windows: bool,
) -> SpawnInTerminal {
    let builder = ShellBuilder::new(shell, is_windows);
    let command_label = builder.command_label(task.command.as_deref().unwrap_or(""));
    let (command, args) = builder.build_no_quote(task.command.clone(), &task.args);

    SpawnInTerminal {
        command_label,
        command: Some(command),
        args,
        ..task.clone()
    }
}

fn is_enabled_in_workspace(workspace: &Workspace, cx: &App) -> bool {
    workspace.project().read(cx).supports_terminal(cx)
}

/// A terminal finishes starting well after it was asked for: the shell the
/// panel opens for itself at startup lands after the worktree trust prompt is
/// up. Focusing it then would send the keystrokes meant for the modal to the
/// shell, so it is only revealed, the way `NoFocus` reveals it.
fn reveal_strategy_for_started_terminal(
    reveal_strategy: RevealStrategy,
    workspace: &Workspace,
    window: &mut Window,
    cx: &mut App,
) -> RevealStrategy {
    if reveal_strategy == RevealStrategy::Always && workspace.has_active_modal(window, cx) {
        RevealStrategy::NoFocus
    } else {
        reveal_strategy
    }
}

pub fn new_terminal_pane(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    zoomed: bool,
    window: &mut Window,
    cx: &mut Context<TerminalPanel>,
) -> Entity<Pane> {
    let terminal_panel = cx.entity();
    let pane = cx.new(|cx| {
        let mut pane = Pane::new(
            workspace.clone(),
            project.clone(),
            Default::default(),
            None,
            workspace::NewTerminal::default().boxed_clone(),
            false,
            window,
            cx,
        );
        pane.set_zoomed(zoomed, cx);
        pane.set_can_navigate(false, cx);
        pane.display_nav_history_buttons(None);
        pane.set_should_display_tab_bar(|_, _| true);
        pane.set_zoom_out_on_close(false);

        let split_closure_terminal_panel = terminal_panel.downgrade();
        pane.set_can_split(Some(Arc::new(move |pane, dragged_item, _window, cx| {
            if let Some(tab) = dragged_item.downcast_ref::<DraggedTab>() {
                let is_current_pane = tab.pane == cx.entity();
                let Some(can_drag_away) = split_closure_terminal_panel
                    .read_with(cx, |terminal_panel, _| {
                        let current_panes = terminal_panel.center.panes();
                        !current_panes.contains(&&tab.pane)
                            || current_panes.len() > 1
                            || (!is_current_pane || pane.items_len() > 1)
                    })
                    .ok()
                else {
                    return false;
                };
                if can_drag_away {
                    let item = if is_current_pane {
                        pane.item_for_index(tab.ix)
                    } else {
                        tab.pane.read(cx).item_for_index(tab.ix)
                    };
                    // 第 12 便: IDE 内のどのタブでも同じドラッグ機構に乗せる。
                    // ターミナル以外がターミナルパネルに載っても、パネルは
                    // アクティブアイテムを `Option` で扱うので壊れない。
                    return item.is_some();
                }
            }
            false
        })));

        // 端に落として分割するときは、ワークスペースの中央ではなくこのパネルの
        // `PaneGroup` を割る (`Workspace::split_pane` は中央しか割れない)。
        let split_handler_terminal_panel = terminal_panel.downgrade();
        let split_handler_workspace = workspace.clone();
        let split_handler_project = project.clone();
        pane.set_tab_drop_split_handler(Some(Arc::new(
            move |pane_to_split, direction, window, cx| {
                let terminal_panel = split_handler_terminal_panel.upgrade()?;
                terminal_panel.update(cx, |terminal_panel, cx| {
                    if !terminal_panel.center.panes().contains(&pane_to_split) {
                        return None;
                    }
                    let is_zoomed = terminal_panel.active_pane.read(cx).is_zoomed();
                    let new_pane = new_terminal_pane(
                        split_handler_workspace.clone(),
                        split_handler_project.clone(),
                        is_zoomed,
                        window,
                        cx,
                    );
                    terminal_panel.apply_tab_bar_buttons(&new_pane, cx);
                    terminal_panel
                        .center
                        .split(pane_to_split, &new_pane, direction, cx);
                    Some(new_pane)
                })
            },
        )));

        let toolbar = pane.toolbar().clone();
        if let Some(callbacks) = cx.try_global::<workspace::PaneSearchBarCallbacks>() {
            let languages = Some(project.read(cx).languages().clone());
            (callbacks.setup_search_bar)(languages, &toolbar, window, cx);
        }
        let breadcrumbs = cx.new(|_| Breadcrumbs::new());
        toolbar.update(cx, |toolbar, cx| {
            toolbar.add_item(breadcrumbs, window, cx);
        });

        pane
    });

    cx.subscribe_in(&pane, window, TerminalPanel::handle_pane_event)
        .detach();
    cx.observe(&pane, |_, _, cx| cx.notify()).detach();

    pane
}

async fn wait_for_terminals_tasks(
    terminals_for_task: Vec<(usize, Entity<Pane>, Entity<TerminalView>)>,
    cx: &mut AsyncApp,
) {
    let pending_tasks = terminals_for_task.iter().map(|(_, _, terminal)| {
        terminal.update(cx, |terminal_view, cx| {
            terminal_view
                .terminal()
                .update(cx, |terminal, cx| terminal.wait_for_completed_task(cx))
        })
    });
    join_all(pending_tasks).await;
}

struct FailedToSpawnTerminal {
    error: String,
    focus_handle: FocusHandle,
}

impl Focusable for FailedToSpawnTerminal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for FailedToSpawnTerminal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let popover_menu = PopoverMenu::new("settings-popover")
            .trigger(
                IconButton::new("icon-button-popover", IconName::ChevronDown)
                    .icon_size(IconSize::XSmall),
            )
            .menu(move |window, cx| {
                Some(ContextMenu::build(window, cx, |context_menu, _, _| {
                    context_menu
                        .action("設定を開く", zed_actions::OpenSettings.boxed_clone())
                        .action(
                            "settings.json を編集",
                            zed_actions::OpenSettingsFile.boxed_clone(),
                        )
                }))
            })
            .anchor(Anchor::TopRight)
            .offset(gpui::Point {
                x: px(0.0),
                y: px(2.0),
            });

        v_flex()
            .track_focus(&self.focus_handle)
            .size_full()
            .p_4()
            .items_center()
            .justify_center()
            .bg(cx.theme().colors().editor_background)
            .child(
                v_flex()
                    .max_w_112()
                    .items_center()
                    .justify_center()
                    .text_center()
                    .child(Label::new("ターミナルを起動できませんでした"))
                    .child(
                        Label::new(self.error.to_string())
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .mb_4(),
                    )
                    .child(SplitButton::new(
                        ButtonLike::new("open-settings-ui")
                            .child(Label::new("設定を編集").size(LabelSize::Small))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(zed_actions::OpenSettings.boxed_clone(), cx);
                            }),
                        popover_menu.into_any_element(),
                    )),
            )
    }
}

impl EventEmitter<()> for FailedToSpawnTerminal {}

impl workspace::Item for FailedToSpawnTerminal {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        SharedString::new_static("ターミナルの起動失敗")
    }
}

impl EventEmitter<PanelEvent> for TerminalPanel {}

impl Render for TerminalPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let registrar = cx
            .try_global::<workspace::PaneSearchBarCallbacks>()
            .map(|callbacks| {
                (callbacks.wrap_div_with_search_actions)(div(), self.active_pane.clone())
            })
            .unwrap_or_else(div);
        self.workspace
            .update(cx, |workspace, cx| {
                registrar
                    .track_focus(&self.focus_handle)
                    .size_full()
                    .child(self.center.render(
                        workspace.zoomed_item(),
                        None,
                        &workspace::PaneRenderContext {
                            follower_states: &HashMap::default(),
                            active_call: workspace.active_call(),
                            active_pane: &self.active_pane,
                            app_state: workspace.app_state(),
                            project: workspace.project(),
                            workspace: &workspace.weak_handle(),
                        },
                        window,
                        cx,
                    ))
            })
            .ok()
            .map(|div| {
                div.on_action({
                    cx.listener(|terminal_panel, _: &ActivatePaneLeft, window, cx| {
                        terminal_panel.activate_pane_in_direction(SplitDirection::Left, window, cx);
                    })
                })
                .on_action({
                    cx.listener(|terminal_panel, _: &ActivatePaneRight, window, cx| {
                        terminal_panel.activate_pane_in_direction(
                            SplitDirection::Right,
                            window,
                            cx,
                        );
                    })
                })
                .on_action({
                    cx.listener(|terminal_panel, _: &ActivatePaneUp, window, cx| {
                        terminal_panel.activate_pane_in_direction(SplitDirection::Up, window, cx);
                    })
                })
                .on_action({
                    cx.listener(|terminal_panel, _: &ActivatePaneDown, window, cx| {
                        terminal_panel.activate_pane_in_direction(SplitDirection::Down, window, cx);
                    })
                })
                .on_action(
                    cx.listener(|terminal_panel, _action: &ActivateNextPane, window, cx| {
                        let panes = terminal_panel.center.panes();
                        if let Some(ix) = panes
                            .iter()
                            .position(|pane| **pane == terminal_panel.active_pane)
                        {
                            let next_ix = (ix + 1) % panes.len();
                            window.focus(&panes[next_ix].focus_handle(cx), cx);
                        }
                    }),
                )
                .on_action(cx.listener(
                    |terminal_panel, _action: &ActivatePreviousPane, window, cx| {
                        let panes = terminal_panel.center.panes();
                        if let Some(ix) = panes
                            .iter()
                            .position(|pane| **pane == terminal_panel.active_pane)
                        {
                            let prev_ix = cmp::min(ix.wrapping_sub(1), panes.len() - 1);
                            window.focus(&panes[prev_ix].focus_handle(cx), cx);
                        }
                    },
                ))
                .on_action(
                    cx.listener(|terminal_panel, action: &ActivatePane, window, cx| {
                        let panes = terminal_panel.center.panes();
                        if let Some(&pane) = panes.get(action.0) {
                            window.focus(&pane.read(cx).focus_handle(cx), cx);
                        } else {
                            let future =
                                terminal_panel.new_pane_with_active_terminal(true, window, cx);
                            cx.spawn_in(window, async move |terminal_panel, cx| {
                                if let Some(new_pane) = future.await {
                                    _ = terminal_panel.update_in(
                                        cx,
                                        |terminal_panel, window, cx| {
                                            terminal_panel.center.split(
                                                &terminal_panel.active_pane,
                                                &new_pane,
                                                SplitDirection::Right,
                                                cx,
                                            );
                                            let new_pane = new_pane.read(cx);
                                            window.focus(&new_pane.focus_handle(cx), cx);
                                        },
                                    );
                                }
                            })
                            .detach();
                        }
                    }),
                )
                .on_action(cx.listener(|terminal_panel, _: &SwapPaneLeft, _, cx| {
                    terminal_panel.swap_pane_in_direction(SplitDirection::Left, cx);
                }))
                .on_action(cx.listener(|terminal_panel, _: &SwapPaneRight, _, cx| {
                    terminal_panel.swap_pane_in_direction(SplitDirection::Right, cx);
                }))
                .on_action(cx.listener(|terminal_panel, _: &SwapPaneUp, _, cx| {
                    terminal_panel.swap_pane_in_direction(SplitDirection::Up, cx);
                }))
                .on_action(cx.listener(|terminal_panel, _: &SwapPaneDown, _, cx| {
                    terminal_panel.swap_pane_in_direction(SplitDirection::Down, cx);
                }))
                .on_action(cx.listener(|terminal_panel, _: &MovePaneLeft, _, cx| {
                    terminal_panel.move_pane_to_border(SplitDirection::Left, cx);
                }))
                .on_action(cx.listener(|terminal_panel, _: &MovePaneRight, _, cx| {
                    terminal_panel.move_pane_to_border(SplitDirection::Right, cx);
                }))
                .on_action(cx.listener(|terminal_panel, _: &MovePaneUp, _, cx| {
                    terminal_panel.move_pane_to_border(SplitDirection::Up, cx);
                }))
                .on_action(cx.listener(|terminal_panel, _: &MovePaneDown, _, cx| {
                    terminal_panel.move_pane_to_border(SplitDirection::Down, cx);
                }))
                .on_action(
                    cx.listener(|terminal_panel, action: &MoveItemToPane, window, cx| {
                        let Some(&target_pane) =
                            terminal_panel.center.panes().get(action.destination)
                        else {
                            return;
                        };
                        move_active_item(
                            &terminal_panel.active_pane,
                            target_pane,
                            action.focus,
                            true,
                            window,
                            cx,
                        );
                    }),
                )
                .on_action(cx.listener(
                    |terminal_panel, action: &MoveItemToPaneInDirection, window, cx| {
                        let source_pane = &terminal_panel.active_pane;
                        if let Some(destination_pane) = terminal_panel
                            .center
                            .find_pane_in_direction(source_pane, action.direction, cx)
                        {
                            move_active_item(
                                source_pane,
                                destination_pane,
                                action.focus,
                                true,
                                window,
                                cx,
                            );
                        };
                    },
                ))
            })
            .unwrap_or_else(|| div())
    }
}

impl Focusable for TerminalPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for TerminalPanel {
    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        self.active_pane.focus_handle(cx)
    }

    fn position(&self, _window: &Window, cx: &App) -> DockPosition {
        TerminalSettings::get_global(cx).dock.into()
    }

    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }

    fn starts_open(&self, _: &Window, cx: &App) -> bool {
        TerminalSettings::get_global(cx).starts_open
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        settings::update_settings_file(self.fs.clone(), cx, move |settings, _| {
            let dock = match position {
                DockPosition::Left => TerminalDockPosition::Left,
                DockPosition::Bottom => TerminalDockPosition::Bottom,
                DockPosition::Right => TerminalDockPosition::Right,
            };
            settings.terminal.get_or_insert_default().dock = Some(dock);
        });
    }

    fn default_size(&self, window: &Window, cx: &App) -> Pixels {
        let settings = TerminalSettings::get_global(cx);
        match self.position(window, cx) {
            DockPosition::Left | DockPosition::Right => settings.default_width,
            DockPosition::Bottom => settings.default_height,
        }
    }

    fn supports_flexible_size(&self) -> bool {
        true
    }

    fn has_flexible_size(&self, _window: &Window, cx: &App) -> bool {
        TerminalSettings::get_global(cx).flexible
    }

    fn set_flexible_size(&mut self, flexible: bool, _window: &mut Window, cx: &mut Context<Self>) {
        settings::update_settings_file(self.fs.clone(), cx, move |settings, _| {
            settings.terminal.get_or_insert_default().flexible = Some(flexible);
        });
    }

    fn is_zoomed(&self, _window: &Window, cx: &App) -> bool {
        self.active_pane.read(cx).is_zoomed()
    }

    fn set_zoomed(&mut self, zoomed: bool, _: &mut Window, cx: &mut Context<Self>) {
        for pane in self.center.panes() {
            pane.update(cx, |pane, cx| {
                pane.set_zoomed(zoomed, cx);
            })
        }
        cx.notify();
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        let old_active = self.active;
        self.active = active;
        if !active {
            // Closing the panel is the clearest "I am done broadcasting"
            // signal there is, and an armed toggle nobody can see is a trap.
            self.set_broadcast_input(false, cx);
        }
        if !active || old_active == active || !self.has_no_terminals(cx) {
            return;
        }
        cx.defer_in(window, |this, window, cx| {
            // A tab dropped into the panel (or a restored session) can land in
            // the pane between the activation and this deferred spawn, and a
            // terminal opened with `RevealStrategy::Always` would take the
            // active tab away from it.
            if !this.has_no_terminals(cx) {
                return;
            }
            let Ok(kind) = this
                .workspace
                .update(cx, |workspace, cx| default_working_directory(workspace, cx))
            else {
                return;
            };

            this.add_terminal_shell(kind, RevealStrategy::Always, window, cx)
                .detach_and_log_err(cx)
        })
    }

    fn icon_label(&self, _window: &Window, cx: &App) -> Option<String> {
        if !TerminalSettings::get_global(cx).show_count_badge {
            return None;
        }
        let count = self
            .center
            .panes()
            .into_iter()
            .map(|pane| pane.read(cx).items_len())
            .sum::<usize>();
        if count == 0 {
            None
        } else {
            Some(count.to_string())
        }
    }

    fn persistent_name() -> &'static str {
        "TerminalPanel"
    }

    fn panel_key() -> &'static str {
        TERMINAL_PANEL_KEY
    }

    fn icon(&self, _window: &Window, cx: &App) -> Option<IconName> {
        if (self.is_enabled(cx) || !self.has_no_terminals(cx))
            && TerminalSettings::get_global(cx).button
        {
            Some(IconName::TerminalAlt)
        } else {
            None
        }
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("ターミナルパネル")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(Toggle)
    }

    fn pane(&self) -> Option<Entity<Pane>> {
        Some(self.active_pane.clone())
    }

    fn panes(&self) -> Vec<Entity<Pane>> {
        self.center.panes().into_iter().cloned().collect()
    }

    fn activation_priority(&self) -> u32 {
        2
    }

    fn hide_button_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        Some(workspace::HideStatusItem::new(|settings| {
            settings.terminal.get_or_insert_default().button = Some(false);
        }))
    }
}

struct TerminalProvider(Entity<TerminalPanel>);

impl workspace::TerminalProvider for TerminalProvider {
    fn spawn(
        &self,
        task: SpawnInTerminal,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Option<Result<ExitStatus>>> {
        let terminal_panel = self.0.clone();
        window.spawn(cx, async move |cx| {
            let terminal = terminal_panel
                .update_in(cx, |terminal_panel, window, cx| {
                    terminal_panel.spawn_task(&task, window, cx)
                })
                .ok()?
                .await;
            match terminal {
                Ok(terminal) => {
                    let exit_status = terminal
                        .read_with(cx, |terminal, cx| terminal.wait_for_completed_task(cx))
                        .ok()?
                        .await?;
                    Some(Ok(exit_status))
                }
                Err(e) => Some(Err(e)),
            }
        })
    }
}

#[derive(IntoElement)]
struct InlineAssistTabBarButton {
    focus_handle: FocusHandle,
}

impl RenderOnce for InlineAssistTabBarButton {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let focus_handle = self.focus_handle;
        IconButton::new("terminal_inline_assistant", IconName::ZedAssistant)
            .icon_size(IconSize::Small)
            .on_click({
                let focus_handle = focus_handle.clone();
                move |_, window, cx| {
                    focus_handle.dispatch_action(&InlineAssist::default(), window, cx);
                }
            })
            .tooltip(move |_window, cx| {
                Tooltip::for_action_in(
                    "インラインアシスト",
                    &InlineAssist::default(),
                    &focus_handle,
                    cx,
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use std::{num::NonZero, path::Path};

    use super::*;
    use crate::persistence::{SerializedPane, SerializedPaneGroup, SerializedPanelItem};
    use gpui::{Modifiers, TestAppContext, UpdateGlobal as _, VisualTestContext};
    use pretty_assertions::assert_eq;
    use project::FakeFs;
    use settings::SettingsStore;
    use workspace::{MultiWorkspace, item::test::TestItem};

    #[test]
    fn test_prepare_empty_task() {
        let input = SpawnInTerminal::default();
        let shell = Shell::System;

        let result = prepare_task_for_spawn(&input, &shell, false);

        let expected_shell = util::get_system_shell();
        assert_eq!(result.env, HashMap::default());
        assert_eq!(result.cwd, None);
        assert_eq!(result.shell, Shell::System);
        assert_eq!(
            result.command,
            Some(expected_shell.clone()),
            "Empty tasks should spawn a -i shell"
        );
        assert_eq!(result.args, Vec::<String>::new());
        assert_eq!(
            result.command_label, expected_shell,
            "We show the shell launch for empty commands"
        );
    }

    fn foldable_test_button(fold_rank: u8, label: &'static str) -> TabBarItem {
        TabBarItem::Foldable(FoldableTabBarButton {
            button: gpui::Empty.into_any_element(),
            menu_label: label.into(),
            menu_action: Box::new(crate::session_tools::RunMacro),
            fold_rank,
        })
    }

    /// The button row the terminal tab bar builds for a split pane with the
    /// assistant off: six foldable buttons around the 新規 and 分割 popovers.
    fn tab_bar_row() -> Vec<TabBarItem> {
        vec![
            foldable_test_button(FOLD_RANK_LAUNCH_CLAUDE_CODE, "Claude Code を起動"),
            foldable_test_button(FOLD_RANK_QUICK_CONNECT, "クイック接続…"),
            foldable_test_button(FOLD_RANK_RUN_MACRO, "マクロを実行…"),
            foldable_test_button(FOLD_RANK_SESSION_LOG, "記録"),
            TabBarItem::Fixed(gpui::Empty.into_any_element()),
            foldable_test_button(FOLD_RANK_BROADCAST_INPUT, "全ペインに同時入力"),
            TabBarItem::Fixed(gpui::Empty.into_any_element()),
            foldable_test_button(FOLD_RANK_ZOOM, "ズーム"),
        ]
    }

    const FIXED_BUTTON: &str = "（固定）";

    fn drawn_labels(drawn: &[TabBarItem]) -> Vec<String> {
        drawn
            .iter()
            .map(|item| match item {
                TabBarItem::Foldable(button) => button.menu_label.to_string(),
                TabBarItem::Fixed(_) => FIXED_BUTTON.to_string(),
            })
            .collect()
    }

    fn folded_labels(folded: &[(SharedString, Box<dyn Action>)]) -> Vec<String> {
        folded.iter().map(|(label, _)| label.to_string()).collect()
    }

    #[test]
    fn the_tab_bar_keeps_every_button_until_it_has_been_laid_out() {
        let (drawn, folded) = fold_tab_bar_items(tab_bar_row(), None, px(16.));

        assert_eq!(
            drawn_labels(&drawn),
            drawn_labels(&tab_bar_row()),
            "a tab bar that has never been laid out must not open up collapsed"
        );
        assert!(folded.is_empty());
    }

    #[test]
    fn a_wide_tab_bar_folds_nothing() {
        let (drawn, folded) = fold_tab_bar_items(tab_bar_row(), Some(px(1000.)), px(16.));

        assert_eq!(drawn.len(), 8);
        assert!(
            folded.is_empty(),
            "1000px is room enough for the whole row and its tabs"
        );
    }

    #[test]
    fn a_narrow_tab_bar_folds_its_least_important_buttons() {
        // 280px leaves room for six buttons next to the tabs; two of those go to
        // the 新規 and 分割 popovers and one to the 「…」 trigger.
        let (drawn, folded) = fold_tab_bar_items(tab_bar_row(), Some(px(280.)), px(16.));

        assert_eq!(
            drawn_labels(&drawn),
            vec![
                "Claude Code を起動",
                "記録",
                FIXED_BUTTON,
                FIXED_BUTTON,
                "ズーム",
            ],
            "the buttons a narrow split still needs at a glance should survive"
        );
        assert_eq!(
            folded_labels(&folded),
            vec!["クイック接続…", "マクロを実行…", "全ペインに同時入力"],
            "the folded buttons should keep the order they are drawn in"
        );
    }

    #[test]
    fn a_tab_bar_with_no_room_at_all_folds_everything_foldable() {
        let (drawn, folded) = fold_tab_bar_items(tab_bar_row(), Some(px(100.)), px(16.));

        assert_eq!(
            drawn_labels(&drawn),
            vec![FIXED_BUTTON, FIXED_BUTTON],
            "the 新規 and 分割 popovers cannot nest inside the 「…」 menu, so they stay"
        );
        assert_eq!(folded.len(), 6);
    }

    #[gpui::test]
    async fn test_bypass_max_tabs_limit(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let terminal_panel = window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    cx.new(|cx| TerminalPanel::new(workspace, window, cx))
                })
            })
            .unwrap();

        set_max_tabs(cx, Some(3));

        for _ in 0..5 {
            let task = window_handle
                .update(cx, |_, window, cx| {
                    terminal_panel.update(cx, |panel, cx| {
                        panel.add_terminal_shell(None, RevealStrategy::Always, window, cx)
                    })
                })
                .unwrap();
            task.await.unwrap();
        }

        cx.run_until_parked();

        let item_count =
            terminal_panel.read_with(cx, |panel, cx| panel.active_pane.read(cx).items_len());

        assert_eq!(
            item_count, 5,
            "Terminal panel should bypass max_tabs limit and have all 5 terminals"
        );
    }

    #[gpui::test]
    async fn test_split_grid_fills_the_grid_and_keeps_focus(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let terminal_panel = window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    cx.new(|cx| TerminalPanel::new(workspace, window, cx))
                })
            })
            .unwrap();

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |panel, cx| {
                    panel.add_terminal_shell(None, RevealStrategy::Always, window, cx)
                })
            })
            .unwrap()
            .await
            .unwrap();
        cx.run_until_parked();

        let origin = terminal_panel.read_with(cx, |panel, _| panel.active_pane.clone());

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |panel, cx| {
                    panel.split_grid(2, 2, window, cx);
                })
            })
            .unwrap();
        cx.run_until_parked();

        terminal_panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.center.panes().len(),
                4,
                "a 2×2 grid should end up with four panes"
            );
        });

        terminal_panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.active_pane, origin,
                "focus should come back to the pane the grid grew out of"
            );
        });
    }

    #[gpui::test]
    async fn test_split_grid_of_one_does_nothing(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let terminal_panel = window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    cx.new(|cx| TerminalPanel::new(workspace, window, cx))
                })
            })
            .unwrap();

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |panel, cx| {
                    panel.split_grid(1, 1, window, cx);
                })
            })
            .unwrap();
        cx.run_until_parked();

        terminal_panel.read_with(cx, |panel, _| {
            assert_eq!(panel.center.panes().len(), 1);
        });
    }

    /// Keeps the panel from opening — and so from spawning a shell of its own —
    /// while a test sets up the terminals it is going to count.
    /// `terminal.starts_open` defaults to true, and only
    /// `test_terminal_panel_starts_open_follows_setting` is about that default.
    fn pin_panel_closed_at_startup(cx: &mut TestAppContext) {
        cx.update_global(|store: &mut SettingsStore, cx| {
            store.update_user_settings(cx, |settings| {
                settings.terminal.get_or_insert_default().starts_open = Some(false);
            });
        });
    }

    /// A panel with `panes` side-by-side shells, plus the view in each of them
    /// in left-to-right order.
    async fn panel_with_panes(
        panes: u8,
        cx: &mut TestAppContext,
    ) -> (
        gpui::WindowHandle<MultiWorkspace>,
        Entity<TerminalPanel>,
        Vec<Entity<TerminalView>>,
    ) {
        cx.executor().allow_parking();
        init_test(cx);
        pin_panel_closed_at_startup(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let terminal_panel = window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    let terminal_panel = cx.new(|cx| TerminalPanel::new(workspace, window, cx));
                    // 同時入力 finds its siblings through the workspace's panel
                    // registry, so the panel has to actually be installed.
                    workspace.add_panel(terminal_panel.clone(), window, cx);
                    terminal_panel
                })
            })
            .unwrap();

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |panel, cx| {
                    panel.add_terminal_shell(None, RevealStrategy::Always, window, cx)
                })
            })
            .unwrap()
            .await
            .unwrap();
        cx.run_until_parked();

        if panes > 1 {
            window_handle
                .update(cx, |_, window, cx| {
                    terminal_panel.update(cx, |panel, cx| {
                        panel.split_grid(panes, 1, window, cx);
                    })
                })
                .unwrap();
            cx.run_until_parked();
        }

        let views = terminal_panel.read_with(cx, |panel, cx| {
            panel
                .center
                .panes()
                .into_iter()
                .filter_map(|pane| {
                    pane.read(cx)
                        .items()
                        .find_map(|item| item.downcast::<TerminalView>())
                })
                .collect::<Vec<_>>()
        });
        (window_handle, terminal_panel, views)
    }

    #[gpui::test]
    async fn broadcast_input_reaches_the_other_panes(cx: &mut TestAppContext) {
        let (window_handle, terminal_panel, views) = panel_with_panes(2, cx).await;
        assert_eq!(views.len(), 2, "the split should have produced two shells");

        for view in &views {
            assert!(
                view.read_with(cx, |view, cx| view.broadcast_targets(cx).is_empty()),
                "nothing is broadcast while the toggle is off"
            );
        }

        window_handle
            .update(cx, |_, _, cx| {
                terminal_panel.update(cx, |panel, cx| panel.toggle_broadcast_input(cx));
            })
            .unwrap();
        assert!(terminal_panel.read_with(cx, |panel, _| panel.broadcast_input()));

        for (index, view) in views.iter().enumerate() {
            let targets = view.read_with(cx, |view, cx| view.broadcast_targets(cx));
            let other = views[1 - index].read_with(cx, |view, _| view.terminal().clone());
            assert_eq!(
                targets,
                vec![other],
                "each pane should type into the other one"
            );
        }
    }

    #[gpui::test]
    async fn broadcast_input_skips_terminals_that_are_not_the_panel_s(cx: &mut TestAppContext) {
        let (window_handle, terminal_panel, views) = panel_with_panes(2, cx).await;
        window_handle
            .update(cx, |_, _, cx| {
                terminal_panel.update(cx, |panel, cx| panel.toggle_broadcast_input(cx));
            })
            .unwrap();

        // An agent's terminal is embedded rather than a panel item, and it must
        // neither receive the broadcast nor start one.
        views[1].update(cx, |view, cx| {
            view.set_embedded_mode(None, cx);
        });

        assert!(
            views[0].read_with(cx, |view, cx| view.broadcast_targets(cx).is_empty()),
            "an embedded terminal must not be typed into"
        );
        assert!(
            views[1].read_with(cx, |view, cx| view.broadcast_targets(cx).is_empty()),
            "an embedded terminal must not drive the panel's shells"
        );
    }

    #[gpui::test]
    async fn broadcast_input_turns_itself_off_when_one_pane_is_left(cx: &mut TestAppContext) {
        let (window_handle, terminal_panel, _views) = panel_with_panes(2, cx).await;
        window_handle
            .update(cx, |_, _, cx| {
                terminal_panel.update(cx, |panel, cx| panel.toggle_broadcast_input(cx));
            })
            .unwrap();
        assert!(terminal_panel.read_with(cx, |panel, _| panel.broadcast_input()));

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |panel, cx| {
                    let Some(pane) = panel.center.panes().last().map(|pane| (*pane).clone()) else {
                        return;
                    };
                    panel.handle_pane_event(
                        &pane,
                        &pane::Event::Remove {
                            focus_on_pane: None,
                        },
                        window,
                        cx,
                    );
                })
            })
            .unwrap();
        cx.run_until_parked();

        terminal_panel.read_with(cx, |panel, _| {
            assert_eq!(panel.center.panes().len(), 1);
            assert!(
                !panel.broadcast_input(),
                "a single pane has nothing to broadcast to"
            );
        });
    }

    #[gpui::test]
    async fn broadcast_input_cannot_be_armed_with_a_single_pane(cx: &mut TestAppContext) {
        let (window_handle, terminal_panel, _views) = panel_with_panes(1, cx).await;
        window_handle
            .update(cx, |_, _, cx| {
                terminal_panel.update(cx, |panel, cx| panel.toggle_broadcast_input(cx));
            })
            .unwrap();

        assert!(terminal_panel.read_with(cx, |panel, _| !panel.broadcast_input()));
    }

    /// A directory of its own under the system temp directory for one test's
    /// session logs. Real files are written, so `FakeFs` cannot stand in here.
    struct TempLogDirectory {
        path: PathBuf,
    }

    impl TempLogDirectory {
        fn new(name: &str) -> Self {
            static NEXT_DIRECTORY: std::sync::atomic::AtomicUsize =
                std::sync::atomic::AtomicUsize::new(0);
            let unique = NEXT_DIRECTORY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join("zed-session-log-tests")
                .join(format!("{name}-{}-{unique}", std::process::id()));
            Self { path }
        }
    }

    impl Drop for TempLogDirectory {
        fn drop(&mut self) {
            // A log that was just stopped can still be held open by its writer
            // task, and Windows refuses to delete an open file. A stray temp
            // directory must not fail the test that has otherwise passed.
            std::fs::remove_dir_all(&self.path).ok();
        }
    }

    fn set_session_log_settings(
        directory: &Path,
        file_name_template: &str,
        cx: &mut TestAppContext,
    ) {
        let directory = directory.display().to_string();
        let file_name_template = file_name_template.to_string();
        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    let session_log = settings
                        .terminal
                        .get_or_insert_default()
                        .session_log
                        .get_or_insert_default();
                    session_log.directory = Some(directory.clone());
                    session_log.file_name_template = Some(file_name_template.clone());
                });
            });
        });
    }

    fn active_session_log_path(pane: &Entity<Pane>, cx: &mut TestAppContext) -> Option<PathBuf> {
        pane.read_with(cx, |pane, cx| {
            session_log_toggle_target(pane, cx).and_then(|(_, log_path)| log_path)
        })
    }

    #[gpui::test]
    async fn the_recording_toggle_starts_and_stops_at_the_configured_path(cx: &mut TestAppContext) {
        let directory = TempLogDirectory::new("toggle");
        let (window_handle, terminal_panel, views) = panel_with_panes(1, cx).await;
        set_session_log_settings(&directory.path, "toggle.log", cx);
        let terminal_view = views
            .first()
            .expect("the panel starts with one terminal")
            .clone();
        let pane = terminal_panel.read_with(cx, |panel, _| panel.active_pane.clone());

        window_handle
            .update(cx, |_, _, cx| {
                terminal_view.update(cx, |terminal_view, cx| terminal_view.toggle_session_log(cx));
            })
            .unwrap();
        cx.run_until_parked();

        let expected_path = directory.path.join("toggle.log");
        assert_eq!(
            active_session_log_path(&pane, cx),
            Some(expected_path.clone()),
            "the first press should record to the configured directory and file name"
        );
        assert!(
            expected_path.exists(),
            "the log file should exist as soon as recording starts"
        );

        window_handle
            .update(cx, |_, _, cx| {
                terminal_view.update(cx, |terminal_view, cx| terminal_view.toggle_session_log(cx));
            })
            .unwrap();
        cx.run_until_parked();

        assert_eq!(
            active_session_log_path(&pane, cx),
            None,
            "the second press should stop recording"
        );
    }

    #[gpui::test]
    async fn the_recording_toggle_follows_the_active_terminal(cx: &mut TestAppContext) {
        let directory = TempLogDirectory::new("follows");
        let (window_handle, terminal_panel, _views) = panel_with_panes(1, cx).await;
        set_session_log_settings(&directory.path, "follows.log", cx);

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |panel, cx| {
                    panel.add_terminal_shell(None, RevealStrategy::Always, window, cx)
                })
            })
            .unwrap()
            .await
            .unwrap();
        cx.run_until_parked();

        let pane = terminal_panel.read_with(cx, |panel, _| panel.active_pane.clone());
        let terminal_views = pane.read_with(cx, |pane, _| {
            pane.items()
                .filter_map(|item| item.downcast::<TerminalView>())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            terminal_views.len(),
            2,
            "the second shell should join the same pane as a new tab"
        );
        let second_view = terminal_views
            .last()
            .expect("the pane holds two terminals")
            .clone();

        window_handle
            .update(cx, |_, _, cx| {
                second_view.update(cx, |terminal_view, cx| terminal_view.toggle_session_log(cx));
            })
            .unwrap();
        cx.run_until_parked();

        let expected_path = directory.path.join("follows.log");
        assert_eq!(
            active_session_log_path(&pane, cx),
            Some(expected_path.clone()),
            "the toggle should show the log of the terminal that is recording"
        );

        window_handle
            .update(cx, |_, window, cx| {
                pane.update(cx, |pane, cx| pane.activate_item(0, true, true, window, cx));
            })
            .unwrap();
        cx.run_until_parked();

        assert_eq!(
            active_session_log_path(&pane, cx),
            None,
            "switching to a terminal that is not recording should reset the toggle"
        );

        window_handle
            .update(cx, |_, window, cx| {
                pane.update(cx, |pane, cx| pane.activate_item(1, true, true, window, cx));
            })
            .unwrap();
        cx.run_until_parked();

        assert_eq!(
            active_session_log_path(&pane, cx),
            Some(expected_path),
            "switching back should show the recording again"
        );

        window_handle
            .update(cx, |_, _, cx| {
                second_view.update(cx, |terminal_view, cx| terminal_view.toggle_session_log(cx));
            })
            .unwrap();
        cx.run_until_parked();
    }

    #[cfg(unix)]
    #[test]
    fn test_prepare_script_like_task() {
        let user_command = r#"REPO_URL=$(git remote get-url origin | sed -e \"s/^git@\\(.*\\):\\(.*\\)\\.git$/https:\\/\\/\\1\\/\\2/\"); COMMIT_SHA=$(git log -1 --format=\"%H\" -- \"${ZED_RELATIVE_FILE}\"); echo \"${REPO_URL}/blob/${COMMIT_SHA}/${ZED_RELATIVE_FILE}#L${ZED_ROW}-$(echo $(($(wc -l <<< \"$ZED_SELECTED_TEXT\") + $ZED_ROW - 1)))\" | xclip -selection clipboard"#.to_string();
        let expected_cwd = PathBuf::from("/some/work");

        let input = SpawnInTerminal {
            command: Some(user_command.clone()),
            cwd: Some(expected_cwd.clone()),
            ..SpawnInTerminal::default()
        };
        let shell = Shell::System;

        let result = prepare_task_for_spawn(&input, &shell, false);

        let system_shell = util::get_system_shell();
        assert_eq!(result.env, HashMap::default());
        assert_eq!(result.cwd, Some(expected_cwd));
        assert_eq!(result.shell, Shell::System);
        assert_eq!(result.command, Some(system_shell.clone()));
        assert_eq!(
            result.args,
            vec!["-i".to_string(), "-c".to_string(), user_command.clone()],
            "User command should have been moved into the arguments, as we're spawning a new -i shell",
        );
        assert_eq!(
            result.command_label,
            format!(
                "{system_shell} {interactive}-c '{user_command}'",
                interactive = if cfg!(windows) { "" } else { "-i " }
            ),
            "We want to show to the user the entire command spawned"
        );
    }

    #[gpui::test]
    async fn renders_error_if_default_shell_fails(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.terminal.get_or_insert_default().project.shell =
                        Some(settings::Shell::Program("__nonexistent_shell__".to_owned()));
                });
            });
        });

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let terminal_panel = window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    cx.new(|cx| TerminalPanel::new(workspace, window, cx))
                })
            })
            .unwrap();

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    terminal_panel.add_terminal_shell(None, RevealStrategy::Always, window, cx)
                })
            })
            .unwrap()
            .await
            .unwrap_err();

        window_handle
            .update(cx, |_, _, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    assert!(
                        terminal_panel
                            .active_pane
                            .read(cx)
                            .items()
                            .any(|item| item.downcast::<FailedToSpawnTerminal>().is_some()),
                        "should spawn `FailedToSpawnTerminal` pane"
                    );
                })
            })
            .unwrap();
    }

    #[gpui::test]
    async fn test_local_terminal_in_local_project(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let terminal_panel = window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    cx.new(|cx| TerminalPanel::new(workspace, window, cx))
                })
            })
            .unwrap();

        let result = window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    terminal_panel.add_local_terminal_shell(RevealStrategy::Always, window, cx)
                })
            })
            .unwrap()
            .await;

        assert!(
            result.is_ok(),
            "local terminal should successfully create in local project"
        );
    }

    struct FocusOnlyModal {
        focus_handle: gpui::FocusHandle,
    }
    impl gpui::EventEmitter<gpui::DismissEvent> for FocusOnlyModal {}
    impl gpui::Focusable for FocusOnlyModal {
        fn focus_handle(&self, _: &gpui::App) -> gpui::FocusHandle {
            self.focus_handle.clone()
        }
    }
    impl Render for FocusOnlyModal {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            gpui::div().track_focus(&self.focus_handle)
        }
    }
    impl workspace::ModalView for FocusOnlyModal {}

    async fn open_center_display_terminal(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) {
        workspace
            .update_in(cx, |workspace, window, cx| {
                TerminalPanel::add_center_terminal(workspace, window, cx, |_, cx| {
                    let terminal = cx.new(|cx| {
                        terminal::TerminalBuilder::new_display_only(
                            terminal::terminal_settings::CursorShape::default(),
                            terminal::terminal_settings::AlternateScroll::On,
                            None,
                            0,
                            cx.background_executor(),
                            util::paths::PathStyle::local(),
                        )
                        .subscribe(cx)
                    });
                    gpui::Task::ready(Ok(terminal))
                })
            })
            .await
            .unwrap();
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_center_terminal_keeps_focus_on_active_modal(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window_handle
            .update(cx, |multi_workspace, _, _| {
                multi_workspace.workspace().clone()
            })
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        let modal_focus_handle = workspace.update_in(cx, |workspace, window, cx| {
            let focus_handle = cx.focus_handle();
            workspace.toggle_modal(window, cx, {
                let focus_handle = focus_handle.clone();
                move |_, _| FocusOnlyModal { focus_handle }
            });
            focus_handle
        });

        workspace.update_in(cx, |workspace, window, cx| {
            assert!(workspace.has_active_modal(window, cx));
            assert!(
                modal_focus_handle.is_focused(window),
                "the modal should hold focus before the terminal is created"
            );
        });

        open_center_display_terminal(&workspace, cx).await;

        workspace.update_in(cx, |_, window, _| {
            assert!(
                modal_focus_handle.is_focused(window),
                "a background center terminal must not steal focus from an active modal"
            );
        });
    }

    #[gpui::test]
    async fn test_center_terminal_takes_focus_without_modal(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window_handle
            .update(cx, |multi_workspace, _, _| {
                multi_workspace.workspace().clone()
            })
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        open_center_display_terminal(&workspace, cx).await;

        workspace.update_in(cx, |workspace, window, cx| {
            assert!(!workspace.has_active_modal(window, cx));
            let terminal_view = workspace
                .active_pane()
                .read(cx)
                .active_item()
                .and_then(|item| item.downcast::<TerminalView>())
                .expect("the new center terminal should be the active item");
            assert!(
                terminal_view.focus_handle(cx).contains_focused(window, cx),
                "with no modal open, a new center terminal should take focus"
            );
        });
    }

    fn display_only_terminal(cx: &mut App) -> Entity<Terminal> {
        cx.new(|cx| {
            terminal::TerminalBuilder::new_display_only(
                terminal::terminal_settings::CursorShape::default(),
                terminal::terminal_settings::AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                util::paths::PathStyle::local(),
            )
            .subscribe(cx)
        })
    }

    /// The shell the panel starts for itself when it opens at startup can
    /// finish after the worktree trust prompt is already up. The prompt keeps
    /// the keyboard, so Enter trusts the project instead of reaching the shell.
    #[gpui::test]
    async fn test_panel_terminal_keeps_focus_on_the_trust_prompt(cx: &mut TestAppContext) {
        init_test(cx);
        pin_panel_closed_at_startup(cx);
        cx.update(|cx| {
            project::trusted_worktrees::init(Default::default(), cx);
            cx.bind_keys([gpui::KeyBinding::new(
                "enter",
                menu::Confirm,
                Some("SecurityModal"),
            )]);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            util::path!("/project"),
            serde_json::json!({ "main.rs": "" }),
        )
        .await;
        let project =
            Project::test_with_worktree_trust(fs, [Path::new(util::path!("/project"))], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let (workspace, terminal_panel) = window_handle
            .update(cx, |multi_workspace, window, cx| {
                let workspace = multi_workspace.workspace().clone();
                let terminal_panel = workspace.update(cx, |workspace, cx| {
                    let panel = cx.new(|cx| TerminalPanel::new(workspace, window, cx));
                    workspace.add_panel(panel.clone(), window, cx);
                    panel
                });
                (workspace, terminal_panel)
            })
            .expect("failed to set up the workspace");
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.show_worktree_trust_security_modal(false, window, cx);
        });
        cx.run_until_parked();
        let security_modal = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_modal::<workspace::security_modal::SecurityModal>(cx)
            })
            .expect("an untrusted worktree should bring up the trust prompt");

        terminal_panel
            .update_in(cx, |terminal_panel, window, cx| {
                terminal_panel.add_terminal_with(RevealStrategy::Always, window, cx, |_, cx| {
                    gpui::Task::ready(Ok(display_only_terminal(cx)))
                })
            })
            .await
            .expect("the terminal should be added to the panel");
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            assert_eq!(terminal_panel.read(cx).active_pane.read(cx).items_len(), 1);
            assert!(
                workspace.bottom_dock().read(cx).is_open(),
                "the panel should still open on its new terminal"
            );
            assert!(
                security_modal
                    .read(cx)
                    .focus_handle(cx)
                    .contains_focused(window, cx),
                "a terminal that finished starting after the trust prompt must not take its focus"
            );
        });

        cx.simulate_keystrokes("enter");

        workspace.update_in(cx, |workspace, _, cx| {
            assert!(
                workspace
                    .active_modal::<workspace::security_modal::SecurityModal>(cx)
                    .is_none(),
                "Enter should confirm the trust prompt"
            );
            assert!(
                !project::trusted_worktrees::TrustedWorktrees::has_restricted_worktrees(
                    &workspace.project().read(cx).worktree_store(),
                    cx
                ),
                "confirming the prompt should trust the project"
            );
        });

        terminal_panel
            .update_in(cx, |terminal_panel, window, cx| {
                terminal_panel.add_terminal_with(RevealStrategy::Always, window, cx, |_, cx| {
                    gpui::Task::ready(Ok(display_only_terminal(cx)))
                })
            })
            .await
            .expect("the second terminal should be added to the panel");
        cx.run_until_parked();

        terminal_panel.update_in(cx, |terminal_panel, window, cx| {
            let terminal_view = terminal_panel
                .active_pane
                .read(cx)
                .active_item()
                .and_then(|item| item.downcast::<TerminalView>())
                .expect("the second terminal should be the panel's active item");
            assert!(
                terminal_view.focus_handle(cx).contains_focused(window, cx),
                "with no modal open, the new terminal should take focus as before"
            );
        });
    }

    #[gpui::test]
    async fn test_inline_assist_tooltip_shows_keybinding_of_active_terminal(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        init_test(cx);

        cx.update(|cx| {
            cx.bind_keys([gpui::KeyBinding::new(
                "ctrl-enter",
                InlineAssist::default(),
                Some("Terminal"),
            )])
        });

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        terminal_panel.update(cx, |panel, cx| panel.set_assistant_enabled(true, cx));
        terminal_panel
            .update_in(cx, |panel, window, cx| {
                panel.add_terminal_shell(None, RevealStrategy::Always, window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();

        let button_bounds = cx
            .debug_bounds("ICON-ZedAssistant")
            .expect("inline assist button should be rendered in the terminal tab bar");
        cx.simulate_mouse_move(button_bounds.center(), None, Modifiers::default());

        cx.executor().advance_clock(Duration::from_millis(600));
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("KEY_BINDING-enter").is_some(),
            "tooltip should show the InlineAssist keybinding resolved in the terminal's context"
        );
    }

    async fn init_workspace_with_panel(
        cx: &mut TestAppContext,
    ) -> (gpui::WindowHandle<MultiWorkspace>, Entity<TerminalPanel>) {
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let terminal_panel = window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    let panel = cx.new(|cx| TerminalPanel::new(workspace, window, cx));
                    workspace.add_panel(panel.clone(), window, cx);
                    panel
                })
            })
            .expect("Failed to initialize workspace with terminal panel");

        (window_handle, terminal_panel)
    }

    #[gpui::test]
    async fn test_terminal_panel_starts_open_follows_setting(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    assert!(
                        terminal_panel.starts_open(window, cx),
                        "terminal panel should start open by default"
                    );
                });
            })
            .expect("Failed to read terminal panel starts_open default");

        cx.update_global(|store: &mut SettingsStore, cx| {
            store.update_user_settings(cx, |settings| {
                settings.terminal.get_or_insert_default().starts_open = Some(false);
            });
        });

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    assert!(
                        !terminal_panel.starts_open(window, cx),
                        "terminal panel should stay closed when configured"
                    );
                });
            })
            .expect("Failed to read configured terminal panel starts_open");

        cx.update_global(|store: &mut SettingsStore, cx| {
            store.update_user_settings(cx, |settings| {
                settings.terminal.get_or_insert_default().starts_open = Some(true);
            });
        });

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    assert!(
                        terminal_panel.starts_open(window, cx),
                        "terminal panel should start open when configured"
                    );
                });
            })
            .expect("Failed to read reconfigured terminal panel starts_open");
    }

    /// 空のパネルが表になったら既定のターミナルを 1 本開くが、開くのは次の
    /// ティック。同じ効果サイクルでタブが流れてきた (ドラッグでの移動、
    /// セッション復元) なら、`RevealStrategy::Always` のターミナルがその
    /// タブから表を奪ってしまうので開かない。
    #[gpui::test]
    async fn test_activating_the_panel_skips_the_default_terminal_for_a_dropped_tab(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        init_test(cx);

        // The panel is built without being added to a dock, so nothing has
        // activated it yet and this test owns the first activation.
        let (window_handle, panel) = panel_for_persistence_test(cx).await;

        let dropped = window_handle
            .update(cx, |_, window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.set_active(true, window, cx);
                    let dropped = cx.new(TestItem::new);
                    panel.active_pane.update(cx, |pane, cx| {
                        pane.add_item(Box::new(dropped.clone()), true, true, None, window, cx);
                    });
                    dropped.entity_id()
                })
            })
            .expect("failed to activate the terminal panel");
        cx.run_until_parked();

        panel.read_with(cx, |panel, cx| {
            assert_eq!(
                panel.pending_terminals_to_add, 0,
                "no default terminal was asked for"
            );
            let pane = panel.active_pane.read(cx);
            assert_eq!(
                pane.items_len(),
                1,
                "the tab that landed in the panel is the only one"
            );
            assert_eq!(
                pane.active_item().map(|item| item.item_id()),
                Some(dropped),
                "and it kept the active slot"
            );
        });
    }

    #[gpui::test]
    async fn test_new_terminal_opens_in_panel_by_default(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);
        pin_panel_closed_at_startup(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;

        let panel_items_before =
            terminal_panel.read_with(cx, |panel, cx| panel.active_pane.read(cx).items_len());
        let center_items_before = window_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .items_len()
            })
            .expect("Failed to read center pane items");

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    TerminalPanel::new_terminal(
                        workspace,
                        &workspace::NewTerminal::default(),
                        window,
                        cx,
                    );
                })
            })
            .expect("Failed to dispatch new_terminal");

        cx.run_until_parked();

        let panel_items_after =
            terminal_panel.read_with(cx, |panel, cx| panel.active_pane.read(cx).items_len());
        let center_items_after = window_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .items_len()
            })
            .expect("Failed to read center pane items");

        assert_eq!(
            panel_items_after,
            panel_items_before + 1,
            "Terminal should be added to the panel when no center terminal is focused"
        );
        assert_eq!(
            center_items_after, center_items_before,
            "Center pane should not gain a new terminal"
        );
    }

    #[gpui::test]
    async fn test_new_terminal_opens_in_center_when_center_terminal_focused(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    TerminalPanel::add_center_terminal(workspace, window, cx, |project, cx| {
                        project.create_terminal_shell(None, cx)
                    })
                })
            })
            .expect("Failed to update workspace")
            .await
            .expect("Failed to create center terminal");
        cx.run_until_parked();

        let center_items_before = window_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .items_len()
            })
            .expect("Failed to read center pane items");
        assert_eq!(center_items_before, 1, "Center pane should have 1 terminal");

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    let active_item = workspace
                        .active_pane()
                        .read(cx)
                        .active_item()
                        .expect("Center pane should have an active item");
                    let terminal_view = active_item
                        .downcast::<TerminalView>()
                        .expect("Active center item should be a TerminalView");
                    window.focus(&terminal_view.focus_handle(cx), cx);
                })
            })
            .expect("Failed to focus terminal view");
        cx.run_until_parked();

        let panel_items_before =
            terminal_panel.read_with(cx, |panel, cx| panel.active_pane.read(cx).items_len());

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    TerminalPanel::new_terminal(
                        workspace,
                        &workspace::NewTerminal::default(),
                        window,
                        cx,
                    );
                })
            })
            .expect("Failed to dispatch new_terminal");
        cx.run_until_parked();

        let center_items_after = window_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .items_len()
            })
            .expect("Failed to read center pane items");
        let panel_items_after =
            terminal_panel.read_with(cx, |panel, cx| panel.active_pane.read(cx).items_len());

        assert_eq!(
            center_items_after,
            center_items_before + 1,
            "New terminal should be added to the center pane"
        );
        assert_eq!(
            panel_items_after, panel_items_before,
            "Terminal panel should not gain a new terminal"
        );
    }

    #[gpui::test]
    async fn test_new_terminal_opens_in_panel_when_panel_focused(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |panel, cx| {
                    panel.add_terminal_shell(None, RevealStrategy::Always, window, cx)
                })
            })
            .expect("Failed to update workspace")
            .await
            .expect("Failed to create panel terminal");
        cx.run_until_parked();

        window_handle
            .update(cx, |_, window, cx| {
                window.focus(&terminal_panel.read(cx).focus_handle(cx), cx);
            })
            .expect("Failed to focus terminal panel");
        cx.run_until_parked();

        let panel_items_before =
            terminal_panel.read_with(cx, |panel, cx| panel.active_pane.read(cx).items_len());

        let center_items_before = window_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .items_len()
            })
            .expect("Failed to read center pane items");

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    TerminalPanel::new_terminal(
                        workspace,
                        &workspace::NewTerminal::default(),
                        window,
                        cx,
                    );
                })
            })
            .expect("Failed to dispatch new_terminal");
        cx.run_until_parked();

        let panel_items_after =
            terminal_panel.read_with(cx, |panel, cx| panel.active_pane.read(cx).items_len());
        let center_items_after = window_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .items_len()
            })
            .expect("Failed to read center pane items");

        assert_eq!(
            panel_items_after,
            panel_items_before + 1,
            "New terminal should be added to the panel when panel is focused"
        );
        assert_eq!(
            center_items_after, center_items_before,
            "Center pane should not gain a new terminal"
        );
    }

    #[gpui::test]
    async fn test_new_local_terminal_opens_in_center_when_center_terminal_focused(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    TerminalPanel::add_center_terminal(workspace, window, cx, |project, cx| {
                        project.create_terminal_shell(None, cx)
                    })
                })
            })
            .expect("Failed to update workspace")
            .await
            .expect("Failed to create center terminal");
        cx.run_until_parked();

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    let active_item = workspace
                        .active_pane()
                        .read(cx)
                        .active_item()
                        .expect("Center pane should have an active item");
                    let terminal_view = active_item
                        .downcast::<TerminalView>()
                        .expect("Active center item should be a TerminalView");
                    window.focus(&terminal_view.focus_handle(cx), cx);
                })
            })
            .expect("Failed to focus terminal view");
        cx.run_until_parked();

        let center_items_before = window_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .items_len()
            })
            .expect("Failed to read center pane items");
        let panel_items_before =
            terminal_panel.read_with(cx, |panel, cx| panel.active_pane.read(cx).items_len());

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    TerminalPanel::new_terminal(
                        workspace,
                        &workspace::NewTerminal { local: true },
                        window,
                        cx,
                    );
                })
            })
            .expect("Failed to dispatch new_terminal with local=true");
        cx.run_until_parked();

        let center_items_after = window_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .items_len()
            })
            .expect("Failed to read center pane items");
        let panel_items_after =
            terminal_panel.read_with(cx, |panel, cx| panel.active_pane.read(cx).items_len());

        assert_eq!(
            center_items_after,
            center_items_before + 1,
            "New local terminal should be added to the center pane"
        );
        assert_eq!(
            panel_items_after, panel_items_before,
            "Terminal panel should not gain a new terminal"
        );
    }

    #[gpui::test]
    async fn test_new_terminal_opens_in_panel_when_panel_focused_and_center_has_terminal(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    TerminalPanel::add_center_terminal(workspace, window, cx, |project, cx| {
                        project.create_terminal_shell(None, cx)
                    })
                })
            })
            .expect("Failed to update workspace")
            .await
            .expect("Failed to create center terminal");
        cx.run_until_parked();

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |panel, cx| {
                    panel.add_terminal_shell(None, RevealStrategy::Always, window, cx)
                })
            })
            .expect("Failed to update workspace")
            .await
            .expect("Failed to create panel terminal");
        cx.run_until_parked();

        window_handle
            .update(cx, |_, window, cx| {
                window.focus(&terminal_panel.read(cx).focus_handle(cx), cx);
            })
            .expect("Failed to focus terminal panel");
        cx.run_until_parked();

        let panel_items_before =
            terminal_panel.read_with(cx, |panel, cx| panel.active_pane.read(cx).items_len());
        let center_items_before = window_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .items_len()
            })
            .expect("Failed to read center pane items");

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    TerminalPanel::new_terminal(
                        workspace,
                        &workspace::NewTerminal::default(),
                        window,
                        cx,
                    );
                })
            })
            .expect("Failed to dispatch new_terminal");
        cx.run_until_parked();

        let panel_items_after =
            terminal_panel.read_with(cx, |panel, cx| panel.active_pane.read(cx).items_len());
        let center_items_after = window_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .items_len()
            })
            .expect("Failed to read center pane items");

        assert_eq!(
            panel_items_after,
            panel_items_before + 1,
            "New terminal should go to panel when panel is focused, even if center has a terminal"
        );
        assert_eq!(
            center_items_after, center_items_before,
            "Center pane should not gain a new terminal when panel is focused"
        );
    }

    fn set_max_tabs(cx: &mut TestAppContext, value: Option<usize>) {
        cx.update_global(|store: &mut SettingsStore, cx| {
            store.update_user_settings(cx, |settings| {
                settings.workspace.max_tabs = value.map(|v| NonZero::new(v).unwrap())
            });
        });
    }

    pub fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
            // ドラッグでパネルに流れてくる「ターミナル以外のタブ」の代役。
            workspace::register_serializable_item::<TestItem>(cx);
        });
    }

    async fn panel_for_persistence_test(
        cx: &mut TestAppContext,
    ) -> (gpui::WindowHandle<MultiWorkspace>, Entity<TerminalPanel>) {
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let panel = window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    cx.new(|cx| TerminalPanel::new(workspace, window, cx))
                })
            })
            .expect("failed to build the terminal panel");
        (window_handle, panel)
    }

    /// ADR-F37: ドラッグで流れてきたタブも種類ごとに書き出す。
    #[gpui::test]
    async fn test_the_panel_serializes_tabs_of_every_kind(cx: &mut TestAppContext) {
        init_test(cx);
        let (window_handle, panel) = panel_for_persistence_test(cx).await;

        window_handle
            .update(cx, |_, window, cx| {
                panel.update(cx, |panel, cx| {
                    let pane = panel.active_pane.clone();
                    pane.update(cx, |pane, cx| {
                        for _ in 0..2 {
                            let item = cx.new(TestItem::new);
                            pane.add_item(Box::new(item), false, false, None, window, cx);
                        }
                    });
                });
            })
            .expect("failed to fill the terminal panel's pane");
        cx.run_until_parked();

        let serialized = panel.update(cx, |panel, cx| {
            serialize_pane_group(&panel.center, &panel.active_pane, cx)
        });
        let SerializedPaneGroup::Pane(pane) = &serialized else {
            panic!("the terminal panel has a single pane");
        };
        assert_eq!(
            pane.items
                .iter()
                .map(|item| item.kind.as_ref())
                .collect::<Vec<_>>(),
            vec!["TestItem", "TestItem"]
        );
        assert!(pane.items[1].active);
        assert!(
            pane.children.is_empty(),
            "the terminal-only format is not written any more"
        );

        // 読み書きの往復で形が崩れないこと。
        let text = serde_json::to_string(&SerializedTerminalPanel {
            items: SerializedItems::WithSplits(serialized),
            active_item_id: None,
        })
        .expect("the layout should serialize");
        let round_tripped: SerializedTerminalPanel =
            serde_json::from_str(&text).expect("the layout should deserialize");
        let SerializedItems::WithSplits(SerializedPaneGroup::Pane(pane)) = &round_tripped.items
        else {
            panic!("the round trip should keep a single pane with splits");
        };
        assert_eq!(pane.items.len(), 2);
    }

    /// ADR-F37: 種類ごとの復元。知らない種類は読み飛ばす。
    #[gpui::test]
    async fn test_the_panel_restores_tabs_of_every_kind(cx: &mut TestAppContext) {
        init_test(cx);
        let (window_handle, _panel) = panel_for_persistence_test(cx).await;

        let serialized = SerializedTerminalPanel {
            items: SerializedItems::WithSplits(SerializedPaneGroup::Pane(SerializedPane {
                active: true,
                items: vec![
                    SerializedPanelItem {
                        kind: Arc::from("TestItem"),
                        item_id: 1,
                        active: false,
                        preview: false,
                    },
                    SerializedPanelItem {
                        kind: Arc::from("SomethingThisBuildDoesNotKnow"),
                        item_id: 2,
                        active: false,
                        preview: false,
                    },
                    SerializedPanelItem {
                        kind: Arc::from("TestItem"),
                        item_id: 3,
                        active: true,
                        preview: false,
                    },
                ],
                children: Vec::new(),
                active_item: None,
                pinned_count: 0,
            })),
            active_item_id: None,
        };

        let restored = window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    deserialize_terminal_panel(
                        workspace.weak_handle(),
                        workspace.project().clone(),
                        workspace::WorkspaceId::from_i64(1),
                        serialized,
                        window,
                        cx,
                    )
                })
            })
            .expect("failed to start the restore")
            .await
            .expect("failed to restore the terminal panel");
        cx.run_until_parked();

        restored.read_with(cx, |panel, cx| {
            let pane = panel.active_pane.read(cx);
            assert_eq!(
                pane.items_len(),
                2,
                "the tab of an unknown kind is skipped, the others come back"
            );
            assert_eq!(
                pane.active_item_index(),
                1,
                "the tab that was on top is put back on top"
            );
        });
    }
}
