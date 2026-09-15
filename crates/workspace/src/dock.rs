use crate::focus_follows_mouse::FocusFollowsMouse as _;
use crate::persistence::model::DockData;
use crate::status_bar::HideStatusItem;
use crate::{DraggedDock, Event, FocusFollowsMouse, ModalLayer, Pane, WorkspaceSettings};
use crate::{Workspace, status_bar::StatusItemView};
use anyhow::Context as _;
use client::proto;
use db::kvp::KeyValueStore;

use gpui::{
    Action, Anchor, AnyView, App, Axis, Bounds, ClickEvent, Context, Div, DragMoveEvent, Entity,
    EntityId, EventEmitter, FocusHandle, Focusable, IntoElement, KeyContext, MouseButton,
    MouseDownEvent, MouseUpEvent, ParentElement, Point, Render, SharedString, StyleRefinement,
    Styled, Subscription, WeakEntity, Window, deferred, div, px, relative,
};
use serde::{Deserialize, Serialize};
use settings::{Settings, SettingsStore, TerminalDockPosition};
use std::sync::Arc;
use ui::{
    ContextMenu, CountBadge, Divider, DividerColor, IconButton, Tooltip, prelude::*,
    right_click_menu,
};
use util::ResultExt as _;

pub(crate) const RESIZE_HANDLE_SIZE: Pixels = px(6.);

/// Share of a dock given to a newly stacked secondary panel.
pub const DEFAULT_SECONDARY_RATIO: f32 = 0.5;
const MIN_SECONDARY_RATIO: f32 = 0.15;
const MAX_SECONDARY_RATIO: f32 = 0.85;

fn clamp_secondary_ratio(ratio: f32) -> f32 {
    ratio.clamp(MIN_SECONDARY_RATIO, MAX_SECONDARY_RATIO)
}

/// Half of a stacked pair that is off screen: the user hid it by clicking its
/// status bar button, or the dock opened at startup for the other half only
/// (ADR-F48). Remembering it lets the same button put the panel back where it
/// was instead of falling back to the "switch panels" click, so a stacked pair
/// behaves as two independent show/hide toggles.
#[derive(Clone, Copy)]
struct HiddenStackedPanel {
    index: usize,
    was_primary: bool,
}

/// What a dock shows: whether it is open, the panels in its two slots, and
/// the half of a pair kept off screen for its button to stack again. Every
/// change to a dock's arrangement is worked out as one of these and applied
/// with [`Dock::arrange`].
#[derive(Clone, Copy)]
struct Arrangement {
    open: bool,
    active: Option<usize>,
    secondary: Option<usize>,
    hidden: Option<HiddenStackedPanel>,
}

impl Arrangement {
    /// Whether `index` is on screen once the dock is open: the primary panel,
    /// or the one stacked beside it. The dock draws nothing without a primary
    /// panel, so a stacked panel on its own does not count.
    fn shows(&self, index: usize) -> bool {
        self.active.is_some() && (self.active == Some(index) || self.secondary == Some(index))
    }

    /// Makes `index` the primary panel the way activating it does: a stacked
    /// panel swaps slots with the primary one, so both stay on screen, and
    /// any other panel replaces it.
    fn promote(&mut self, index: usize) {
        if self.secondary == Some(index) {
            self.secondary = self.active;
        }
        self.active = Some(index);
    }

    /// Puts `index` on top with the panel that had the primary slot stacked
    /// below it, so that one stays on screen.
    fn move_in_above(&mut self, index: usize) {
        self.secondary = self.active.filter(|active| *active != index);
        self.active = Some(index);
    }

    /// Keeps `index` off screen the way a click on half of a pair hides it,
    /// so a click on its button stacks it again, on top if `was_primary`. A
    /// different panel already kept off screen that way keeps its place.
    fn keep_off_screen(&mut self, index: usize, was_primary: bool) {
        let other_panel_kept = self
            .hidden
            .is_some_and(|hidden| hidden.index != index && !self.shows(hidden.index));
        if !other_panel_kept {
            self.hidden = Some(HiddenStackedPanel { index, was_primary });
        }
    }
}

/// Drag state for the divider between two stacked panels inside a dock.
#[derive(Clone)]
pub(crate) struct DraggedDockSplit(DockPosition);

impl Render for DraggedDockSplit {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// A whole panel dragged by its status bar button, so it can be dropped on
/// another dock or released outside the window to be torn off into one of its
/// own (ADR-F40). The icon and label travel with the drag because the preview
/// keeps rendering after the panel's own dock has let go of it.
#[derive(Clone)]
pub struct DraggedPanel {
    pub panel: Arc<dyn PanelHandle>,
    icon: IconName,
    label: SharedString,
}

impl Render for DraggedPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_1()
            .px_1p5()
            .py_1()
            .rounded_sm()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().elevated_surface_background)
            .shadow_md()
            .child(
                Icon::new(self.icon)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(Label::new(self.label.clone()).size(LabelSize::Small))
    }
}

pub enum PanelEvent {
    ZoomIn,
    ZoomOut,
    Activate,
    Close,
}

pub use proto::PanelId;

pub trait Panel: Focusable + EventEmitter<PanelEvent> + Render + Sized {
    fn persistent_name() -> &'static str;
    fn panel_key() -> &'static str;
    /// The `Focusable::focus_handle` root identifies the panel's subtree for containment checks
    /// and must be tracked by the panel's root element. This method returns the handle that should
    /// receive focus when the panel is activated, such as a filter, commit, or message editor; it
    /// must be a focus-tree descendant of the root or containment checks such as Zen-mode auto-close
    /// and toggle-focus will misbehave.
    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        self.focus_handle(cx)
    }
    fn position(&self, window: &Window, cx: &App) -> DockPosition;
    fn position_is_valid(&self, position: DockPosition) -> bool;
    fn set_position(&mut self, position: DockPosition, window: &mut Window, cx: &mut Context<Self>);
    fn default_size(&self, window: &Window, cx: &App) -> Pixels;
    fn min_size(&self, _window: &Window, _cx: &App) -> Option<Pixels> {
        None
    }
    fn initial_size_state(&self, _window: &Window, _cx: &App) -> PanelSizeState {
        PanelSizeState::default()
    }
    fn size_state_changed(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}
    fn supports_flexible_size(&self) -> bool {
        false
    }
    fn has_flexible_size(&self, _window: &Window, _cx: &App) -> bool {
        false
    }
    fn set_flexible_size(
        &mut self,
        _flexible: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }
    fn icon(&self, window: &Window, cx: &App) -> Option<ui::IconName>;
    fn icon_tooltip(&self, window: &Window, cx: &App) -> Option<&'static str>;
    fn toggle_action(&self) -> Box<dyn Action>;
    fn icon_label(&self, _window: &Window, _: &App) -> Option<String> {
        None
    }
    fn is_zoomed(&self, _window: &Window, _cx: &App) -> bool {
        false
    }
    fn starts_open(&self, _window: &Window, _cx: &App) -> bool {
        false
    }
    fn set_zoomed(&mut self, _zoomed: bool, _window: &mut Window, _cx: &mut Context<Self>) {}
    fn set_active(&mut self, _active: bool, _window: &mut Window, _cx: &mut Context<Self>) {}
    /// The user asked for this panel by name: its status bar button, its toggle
    /// action, a drop onto a dock. `set_active` cannot answer that question,
    /// because a dock also shows the other half of a stacked pair (ADR-F30) when
    /// the user only asked for the panel it is paired with. Panels that hold work
    /// back until someone actually turns to them - the browser does not load its
    /// home page before that (ADR-F48) - are told here.
    fn revealed_by_user(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}
    fn pane(&self) -> Option<Entity<Pane>> {
        None
    }
    /// 葉ペインをすべて返す。分割を持つパネル (ターミナル / ブラウザー) は
    /// [`Panel::pane`] がアクティブな 1 枚しか返さないので、そこに置かれた
    /// タブを数え落とさないための入口 (ADR-F37 の後始末が使う)。
    fn panes(&self) -> Vec<Entity<Pane>> {
        self.pane().into_iter().collect()
    }
    fn remote_id() -> Option<proto::PanelId> {
        None
    }
    fn activation_priority(&self) -> u32;
    fn enabled(&self, _cx: &App) -> bool {
        true
    }
    fn is_agent_panel(&self) -> bool {
        false
    }
    /// Returns metadata describing how to hide this panel's button from the
    /// status bar by writing to user settings. Implementors should return
    /// `None` if the panel button cannot be hidden through settings.
    fn hide_button_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}

pub trait PanelHandle: Send + Sync {
    fn panel_id(&self) -> EntityId;
    fn persistent_name(&self) -> &'static str;
    fn panel_key(&self) -> &'static str;
    fn position(&self, window: &Window, cx: &App) -> DockPosition;
    fn position_is_valid(&self, position: DockPosition, cx: &App) -> bool;
    fn set_position(&self, position: DockPosition, window: &mut Window, cx: &mut App);
    fn is_zoomed(&self, window: &Window, cx: &App) -> bool;
    fn starts_open(&self, window: &Window, cx: &App) -> bool;
    fn set_zoomed(&self, zoomed: bool, window: &mut Window, cx: &mut App);
    fn set_active(&self, active: bool, window: &mut Window, cx: &mut App);
    /// See `Panel::revealed_by_user`.
    fn revealed_by_user(&self, window: &mut Window, cx: &mut App);
    fn remote_id(&self) -> Option<proto::PanelId>;
    fn pane(&self, cx: &App) -> Option<Entity<Pane>>;
    fn panes(&self, cx: &App) -> Vec<Entity<Pane>>;
    fn default_size(&self, window: &Window, cx: &App) -> Pixels;
    fn min_size(&self, window: &Window, cx: &App) -> Option<Pixels>;
    fn initial_size_state(&self, window: &Window, cx: &App) -> PanelSizeState;
    fn size_state_changed(&self, window: &mut Window, cx: &mut App);
    fn supports_flexible_size(&self, cx: &App) -> bool;
    fn has_flexible_size(&self, window: &Window, cx: &App) -> bool;
    fn set_flexible_size(&self, flexible: bool, window: &mut Window, cx: &mut App);
    fn icon(&self, window: &Window, cx: &App) -> Option<ui::IconName>;
    fn icon_tooltip(&self, window: &Window, cx: &App) -> Option<&'static str>;
    fn toggle_action(&self, window: &Window, cx: &App) -> Box<dyn Action>;
    fn icon_label(&self, window: &Window, cx: &App) -> Option<String>;
    fn panel_focus_handle(&self, cx: &App) -> FocusHandle;
    /// See `Panel::activation_focus_handle`.
    fn activation_focus_handle(&self, cx: &App) -> FocusHandle;
    fn to_any(&self) -> AnyView;
    fn activation_priority(&self, cx: &App) -> u32;
    fn enabled(&self, cx: &App) -> bool;
    fn is_agent_panel(&self, cx: &App) -> bool;
    fn hide_button_setting(&self, cx: &App) -> Option<HideStatusItem>;
    fn move_to_next_position(&self, window: &mut Window, cx: &mut App) {
        let current_position = self.position(window, cx);
        let next_position = [
            DockPosition::Left,
            DockPosition::Bottom,
            DockPosition::Right,
        ]
        .into_iter()
        .filter(|position| self.position_is_valid(*position, cx))
        .skip_while(|valid_position| *valid_position != current_position)
        .nth(1)
        .unwrap_or(DockPosition::Left);

        self.set_position(next_position, window, cx);
    }
}

impl<T> PanelHandle for Entity<T>
where
    T: Panel,
{
    fn panel_id(&self) -> EntityId {
        Entity::entity_id(self)
    }

    fn persistent_name(&self) -> &'static str {
        T::persistent_name()
    }

    fn panel_key(&self) -> &'static str {
        T::panel_key()
    }

    fn position(&self, window: &Window, cx: &App) -> DockPosition {
        self.read(cx).position(window, cx)
    }

    fn position_is_valid(&self, position: DockPosition, cx: &App) -> bool {
        self.read(cx).position_is_valid(position)
    }

    fn set_position(&self, position: DockPosition, window: &mut Window, cx: &mut App) {
        self.update(cx, |this, cx| this.set_position(position, window, cx))
    }

    fn is_zoomed(&self, window: &Window, cx: &App) -> bool {
        self.read(cx).is_zoomed(window, cx)
    }

    fn starts_open(&self, window: &Window, cx: &App) -> bool {
        self.read(cx).starts_open(window, cx)
    }

    fn set_zoomed(&self, zoomed: bool, window: &mut Window, cx: &mut App) {
        self.update(cx, |this, cx| this.set_zoomed(zoomed, window, cx))
    }

    fn set_active(&self, active: bool, window: &mut Window, cx: &mut App) {
        self.update(cx, |this, cx| this.set_active(active, window, cx))
    }

    fn revealed_by_user(&self, window: &mut Window, cx: &mut App) {
        self.update(cx, |this, cx| this.revealed_by_user(window, cx))
    }

    fn pane(&self, cx: &App) -> Option<Entity<Pane>> {
        self.read(cx).pane()
    }

    fn panes(&self, cx: &App) -> Vec<Entity<Pane>> {
        self.read(cx).panes()
    }

    fn remote_id(&self) -> Option<PanelId> {
        T::remote_id()
    }

    fn default_size(&self, window: &Window, cx: &App) -> Pixels {
        self.read(cx).default_size(window, cx)
    }

    fn min_size(&self, window: &Window, cx: &App) -> Option<Pixels> {
        self.read(cx).min_size(window, cx)
    }

    fn initial_size_state(&self, window: &Window, cx: &App) -> PanelSizeState {
        self.read(cx).initial_size_state(window, cx)
    }

    fn size_state_changed(&self, window: &mut Window, cx: &mut App) {
        self.update(cx, |this, cx| this.size_state_changed(window, cx))
    }

    fn supports_flexible_size(&self, cx: &App) -> bool {
        self.read(cx).supports_flexible_size()
    }

    fn has_flexible_size(&self, window: &Window, cx: &App) -> bool {
        self.read(cx).has_flexible_size(window, cx)
    }

    fn set_flexible_size(&self, flexible: bool, window: &mut Window, cx: &mut App) {
        self.update(cx, |this, cx| this.set_flexible_size(flexible, window, cx))
    }

    fn icon(&self, window: &Window, cx: &App) -> Option<ui::IconName> {
        self.read(cx).icon(window, cx)
    }

    fn icon_tooltip(&self, window: &Window, cx: &App) -> Option<&'static str> {
        self.read(cx).icon_tooltip(window, cx)
    }

    fn toggle_action(&self, _: &Window, cx: &App) -> Box<dyn Action> {
        self.read(cx).toggle_action()
    }

    fn icon_label(&self, window: &Window, cx: &App) -> Option<String> {
        self.read(cx).icon_label(window, cx)
    }

    fn to_any(&self) -> AnyView {
        self.clone().into()
    }

    fn panel_focus_handle(&self, cx: &App) -> FocusHandle {
        self.read(cx).focus_handle(cx)
    }

    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        self.read(cx).activation_focus_handle(cx)
    }

    fn activation_priority(&self, cx: &App) -> u32 {
        self.read(cx).activation_priority()
    }

    fn enabled(&self, cx: &App) -> bool {
        self.read(cx).enabled(cx)
    }

    fn is_agent_panel(&self, cx: &App) -> bool {
        self.read(cx).is_agent_panel()
    }

    fn hide_button_setting(&self, cx: &App) -> Option<HideStatusItem> {
        self.read(cx).hide_button_setting(cx)
    }
}

impl From<&dyn PanelHandle> for AnyView {
    fn from(val: &dyn PanelHandle) -> Self {
        val.to_any()
    }
}

/// A container with a fixed [`DockPosition`] adjacent to a certain widown edge.
/// Can contain multiple panels and show/hide itself with all contents.
pub struct Dock {
    position: DockPosition,
    panel_entries: Vec<PanelEntry>,
    workspace: WeakEntity<Workspace>,
    is_open: bool,
    active_panel_index: Option<usize>,
    /// A second panel shown alongside the active ("primary") one, split by a
    /// draggable divider. `None` means the dock shows a single panel, which is
    /// the upstream behaviour.
    secondary_panel_index: Option<usize>,
    /// Share of the dock the stacked secondary panel occupies, along
    /// [`Dock::stack_axis`].
    secondary_ratio: f32,
    hidden_stacked_panel: Option<HiddenStackedPanel>,
    /// Whether a panel asking to start open may pull this dock open on its own.
    /// A window torn off by [`Workspace::move_item_to_new_window`] shows nothing
    /// but the tab that was dragged into it, and its panels are added long after
    /// it is on screen, so the automatic open is refused there. Opening the dock
    /// by hand still works.
    starts_open_suppressed: bool,
    /// Set once anything but the startup rules changes what the dock shows: a
    /// click, a toggle, a drop, a panel showing itself. The startup rules
    /// leave such a dock alone from then on, so the saved state or
    /// `Panel::starts_open` never undo it. Restoring the saved state clears
    /// it, since that replaces whatever the dock had.
    arranged_by_user: bool,
    focus_handle: FocusHandle,
    focus_follows_mouse: FocusFollowsMouse,
    pub(crate) serialized_dock: Option<DockData>,
    zoom_layer_open: bool,
    modal_layer: Entity<ModalLayer>,
    _subscriptions: [Subscription; 2],
}

impl Focusable for Dock {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DockPosition {
    Left,
    Bottom,
    Right,
}

impl From<settings::DockPosition> for DockPosition {
    fn from(value: settings::DockPosition) -> Self {
        match value {
            settings::DockPosition::Left => Self::Left,
            settings::DockPosition::Bottom => Self::Bottom,
            settings::DockPosition::Right => Self::Right,
        }
    }
}

impl Into<settings::DockPosition> for DockPosition {
    fn into(self) -> settings::DockPosition {
        match self {
            Self::Left => settings::DockPosition::Left,
            Self::Bottom => settings::DockPosition::Bottom,
            Self::Right => settings::DockPosition::Right,
        }
    }
}

impl From<TerminalDockPosition> for DockPosition {
    fn from(value: TerminalDockPosition) -> Self {
        match value {
            TerminalDockPosition::Left => DockPosition::Left,
            TerminalDockPosition::Bottom => DockPosition::Bottom,
            TerminalDockPosition::Right => DockPosition::Right,
        }
    }
}

impl DockPosition {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Left => "左",
            Self::Bottom => "下",
            Self::Right => "右",
        }
    }

    pub fn axis(&self) -> Axis {
        match self {
            Self::Left | Self::Right => Axis::Horizontal,
            Self::Bottom => Axis::Vertical,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PanelSizeState {
    pub size: Option<Pixels>,
    #[serde(default)]
    pub flex: Option<f32>,
}

struct PanelEntry {
    panel: Arc<dyn PanelHandle>,
    size_state: PanelSizeState,
    /// The panel's `Panel::starts_open` answer as of the last pass that
    /// placed panels asking to start open, so that the pass run after
    /// startup acts only on answers that changed since. `add_panel` seeds it
    /// with the answer the panel arrives with, which keeps a panel added after
    /// startup out of that pass.
    starts_open_seen: bool,
    _subscriptions: [Subscription; 4],
}

pub struct PanelButtons {
    dock: Entity<Dock>,
    _settings_subscription: Subscription,
}

pub(crate) const PANEL_SIZE_STATE_KEY: &str = "dock_panel_size";

fn panel_uses_flexible_width(
    position: DockPosition,
    panel: &dyn PanelHandle,
    window: &Window,
    cx: &App,
) -> bool {
    position.axis() == Axis::Horizontal && panel.has_flexible_size(window, cx)
}

fn resize_panel_entry(
    position: DockPosition,
    entry: &mut PanelEntry,
    size: Option<Pixels>,
    flex: Option<f32>,
    window: &mut Window,
    cx: &mut App,
) -> (&'static str, PanelSizeState) {
    let size = size.map(|size| size.max(RESIZE_HANDLE_SIZE).round());
    let uses_flexible_width = panel_uses_flexible_width(position, entry.panel.as_ref(), window, cx);
    if uses_flexible_width {
        entry.size_state.flex = flex;
    } else {
        entry.size_state.size = size;
    }
    entry.panel.size_state_changed(window, cx);
    (entry.panel.panel_key(), entry.size_state)
}

impl Dock {
    pub fn new(
        position: DockPosition,
        modal_layer: Entity<ModalLayer>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let focus_handle = cx.focus_handle();
        let workspace = cx.entity();
        let dock = cx.new(|cx| {
            let focus_subscription =
                cx.on_focus(&focus_handle, window, |dock: &mut Dock, window, cx| {
                    if let Some(active_entry) = dock.active_panel_entry() {
                        active_entry
                            .panel
                            .activation_focus_handle(cx)
                            .focus(window, cx)
                    }
                });
            let zoom_subscription = cx.subscribe(&workspace, |dock, workspace, e: &Event, cx| {
                if matches!(e, Event::ZoomChanged) {
                    let is_zoomed = workspace.read(cx).zoomed.is_some();
                    dock.zoom_layer_open = is_zoomed;
                }
            });
            Self {
                position,
                workspace: workspace.downgrade(),
                panel_entries: Default::default(),
                active_panel_index: None,
                secondary_panel_index: None,
                secondary_ratio: DEFAULT_SECONDARY_RATIO,
                hidden_stacked_panel: None,
                starts_open_suppressed: false,
                arranged_by_user: false,
                is_open: false,
                focus_handle: focus_handle.clone(),
                focus_follows_mouse: WorkspaceSettings::get_global(cx).focus_follows_mouse,
                _subscriptions: [focus_subscription, zoom_subscription],
                serialized_dock: None,
                zoom_layer_open: false,
                modal_layer,
            }
        });

        cx.on_focus_in(&focus_handle, window, {
            let dock = dock.downgrade();
            move |workspace, window, cx| {
                let Some(dock) = dock.upgrade() else {
                    return;
                };
                let Some(panel) = dock.read(cx).active_panel() else {
                    return;
                };
                if panel.is_zoomed(window, cx) {
                    workspace.zoomed = Some(panel.to_any().downgrade());
                    workspace.zoomed_position = Some(position);
                } else {
                    workspace.zoomed = None;
                    workspace.zoomed_position = None;
                }
                cx.emit(Event::ZoomChanged);
                workspace.dismiss_zoomed_items_to_reveal(Some(position), window, cx);
                workspace.update_active_view_for_followers(window, cx)
            }
        })
        .detach();

        cx.observe_in(&dock, window, move |workspace, dock, window, cx| {
            if dock.read(cx).is_open()
                && let Some(panel) = dock.read(cx).active_panel()
                && panel.is_zoomed(window, cx)
            {
                workspace.zoomed = Some(panel.to_any().downgrade());
                workspace.zoomed_position = Some(position);
                cx.emit(Event::ZoomChanged);
                return;
            }
            if workspace.zoomed_position == Some(position) {
                workspace.zoomed = None;
                workspace.zoomed_position = None;
                cx.emit(Event::ZoomChanged);
            }
        })
        .detach();

        dock
    }

    pub fn position(&self) -> DockPosition {
        self.position
    }

    pub fn is_open(&self) -> bool {
        self.is_open
    }

    fn resizable(&self, cx: &App) -> bool {
        !(self.zoom_layer_open || self.modal_layer.read(cx).has_active_modal())
    }

    pub fn panel<T: Panel>(&self) -> Option<Entity<T>> {
        self.panel_entries
            .iter()
            .find_map(|entry| entry.panel.to_any().downcast().ok())
    }

    /// このドックが抱えているパネル全部 (開いているかどうかは問わない)。
    /// 畳んだドックのパネルにもタブは載ったままなので、生死を数えるときは
    /// [`Dock::visible_panels`] ではなくこちらを使う。
    pub fn panels(&self) -> impl Iterator<Item = &Arc<dyn PanelHandle>> {
        self.panel_entries.iter().map(|entry| &entry.panel)
    }

    pub fn panel_index_for_type<T: Panel>(&self) -> Option<usize> {
        self.panel_entries
            .iter()
            .position(|entry| entry.panel.to_any().downcast::<T>().is_ok())
    }

    pub fn panel_index_for_persistent_name(&self, ui_name: &str, _cx: &App) -> Option<usize> {
        self.panel_entries
            .iter()
            .position(|entry| entry.panel.persistent_name() == ui_name)
    }

    pub fn panel_index_for_proto_id(&self, panel_id: PanelId) -> Option<usize> {
        self.panel_entries
            .iter()
            .position(|entry| entry.panel.remote_id() == Some(panel_id))
    }

    pub fn panel_for_id(&self, panel_id: EntityId) -> Option<&Arc<dyn PanelHandle>> {
        self.panel_entries
            .iter()
            .find(|entry| entry.panel.panel_id() == panel_id)
            .map(|entry| &entry.panel)
    }

    pub fn first_enabled_panel_idx(&mut self, cx: &mut Context<Self>) -> anyhow::Result<usize> {
        self.panel_entries
            .iter()
            .position(|entry| entry.panel.enabled(cx))
            .with_context(|| {
                format!(
                    "Couldn't find any enabled panel for the {} dock.",
                    self.position.label()
                )
            })
    }

    fn active_panel_entry(&self) -> Option<&PanelEntry> {
        self.active_panel_index
            .and_then(|index| self.panel_entries.get(index))
    }

    fn active_panel_entry_mut(&mut self) -> Option<&mut PanelEntry> {
        self.active_panel_index
            .and_then(|index| self.panel_entries.get_mut(index))
    }

    pub fn active_panel_index(&self) -> Option<usize> {
        self.active_panel_index
    }

    /// The index of the panel stacked next to the active one, if any. Always
    /// distinct from [`Dock::active_panel_index`] and always in range. `None`
    /// without an active panel: the dock draws nothing then.
    pub fn secondary_panel_index(&self) -> Option<usize> {
        self.secondary_panel_index
            .filter(|_| self.active_panel_index.is_some())
            .filter(|index| Some(*index) != self.active_panel_index)
            .filter(|index| *index < self.panel_entries.len())
    }

    /// Indices of the panels the dock lays out, primary first. Independent of
    /// whether the dock is open.
    fn stacked_panel_indices(&self) -> [Option<usize>; 2] {
        [self.active_panel_index, self.secondary_panel_index()]
    }

    pub fn panel_at(&self, index: usize) -> Option<&Arc<dyn PanelHandle>> {
        self.panel_entries.get(index).map(|entry| &entry.panel)
    }

    pub fn secondary_panel(&self) -> Option<&Arc<dyn PanelHandle>> {
        self.panel_at(self.secondary_panel_index()?)
    }

    /// Every panel the open dock currently shows: the primary panel first, then
    /// the stacked secondary one when there is one. Empty while the dock is closed.
    pub fn visible_panels(&self) -> impl Iterator<Item = &Arc<dyn PanelHandle>> {
        let indices = if self.is_open {
            self.stacked_panel_indices()
        } else {
            [None, None]
        };
        indices
            .into_iter()
            .flatten()
            .filter_map(|index| self.panel_at(index))
    }

    /// Axis the primary and secondary panels are stacked along. Side docks stack
    /// vertically (one panel above the other); the bottom dock stacks horizontally
    /// so both panels keep the dock's height.
    fn stack_axis(&self) -> Axis {
        match self.position {
            DockPosition::Left | DockPosition::Right => Axis::Vertical,
            DockPosition::Bottom => Axis::Horizontal,
        }
    }

    pub fn secondary_ratio(&self) -> f32 {
        self.secondary_ratio
    }

    pub fn set_secondary_ratio(&mut self, ratio: f32, cx: &mut Context<Self>) {
        let ratio = clamp_secondary_ratio(ratio);
        if ratio != self.secondary_ratio {
            self.secondary_ratio = ratio;
            cx.notify();
        }
    }

    /// Stacks `index` next to the active panel, or unstacks the current secondary
    /// when passed `None`. Panels that are already primary, or out of range, are
    /// ignored so the two slots can never collide.
    pub fn set_secondary_panel(
        &mut self,
        index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.arranged_by_user = true;
        let index = index
            .filter(|index| *index < self.panel_entries.len())
            .filter(|index| Some(*index) != self.active_panel_index);
        if index == self.secondary_panel_index() {
            // Keep the stored index normalized so it can never become visible
            // again once it has been filtered out.
            self.secondary_panel_index = index;
        } else {
            let mut target = self.arrangement();
            target.secondary = index;
            // Any new pairing supersedes the one the hidden panel belonged to.
            target.hidden = None;
            self.arrange(target, window, cx);
        }
        // Stacking a panel is the panel's own button's context menu entry, so
        // it is that panel being asked for; unstacking (`None`) is not.
        if let Some(index) = index {
            self.reveal_panel_to_user(index, window, cx);
        }
    }

    /// The panel that [`Dock::hide_stacked_panel`] took out of a stacked pair,
    /// as long as it is still the one [`Dock::restack_hidden_panel`] would
    /// bring back.
    pub fn hidden_stacked_panel_index(&self) -> Option<usize> {
        self.hidden_stacked_panel
            .map(|hidden| hidden.index)
            .filter(|index| *index < self.panel_entries.len())
            .filter(|index| Some(*index) != self.active_panel_index)
            .filter(|_| self.is_open && self.secondary_panel_index().is_none())
    }

    /// Hides one half of a stacked pair, leaving the other half in the dock.
    /// Returns whether `index` was part of a pair the dock is showing.
    pub fn hide_stacked_panel(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(secondary_index) = self.secondary_panel_index().filter(|_| self.is_open) else {
            return false;
        };
        let was_primary = self.active_panel_index == Some(index);
        if !was_primary && secondary_index != index {
            return false;
        }
        self.arranged_by_user = true;
        let mut target = self.arrangement();
        // Promoting the stacked panel is what frees the primary slot; the dock
        // keeps showing the other half either way.
        if was_primary {
            target.active = Some(secondary_index);
        }
        target.secondary = None;
        target.hidden = Some(HiddenStackedPanel { index, was_primary });
        self.arrange(target, window, cx);
        true
    }

    /// Puts the panel hidden by [`Dock::hide_stacked_panel`] back into the slot
    /// it had. Returns whether `index` is that panel.
    pub fn restack_hidden_panel(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.hidden_stacked_panel_index() != Some(index) {
            return false;
        }
        self.arranged_by_user = true;
        let was_primary = self
            .hidden_stacked_panel
            .is_some_and(|hidden| hidden.was_primary);
        let mut target = self.arrangement();
        if was_primary {
            target.move_in_above(index);
        } else {
            target.secondary = Some(index);
        }
        target.hidden = None;
        self.arrange(target, window, cx);
        self.reveal_panel_to_user(index, window, cx);
        true
    }

    /// Stacks `index` next to whatever the dock shows, opening the dock first.
    /// Returns false only when `index` isn't a panel of this dock.
    pub fn stack_panel_beside_primary(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if index >= self.panel_entries.len() {
            return false;
        }
        self.arranged_by_user = true;
        let mut target = self.arrangement();
        target.open = true;
        match target.active {
            None => target.promote(index),
            // Nothing to pair the dock's only visible panel with; opening it is
            // still better than the plain click's "close the dock".
            Some(active_index) if active_index == index => {}
            Some(_) => {
                target.secondary = Some(index);
                // Any new pairing supersedes the one the hidden panel belonged to.
                target.hidden = None;
            }
        }
        self.arrange(target, window, cx);
        self.reveal_panel_to_user(index, window, cx);
        true
    }

    /// Fork default for a project that has no persisted dock layout yet:
    /// arranges `primary_name` with `secondary_name` stacked below it for
    /// whenever the dock is shown. It never opens the dock; that is left to
    /// the user and to `Panel::starts_open` (ADR-F48). Panels are added
    /// asynchronously during startup, so this reports whether the pairing is
    /// decided, and the caller retries until it is.
    pub(crate) fn apply_default_stacked_layout(
        &mut self,
        primary_name: &str,
        secondary_name: &str,
        during_startup: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(primary_index) = self.panel_index_for_persistent_name(primary_name, cx) else {
            return false;
        };
        let Some(secondary_index) = self.panel_index_for_persistent_name(secondary_name, cx) else {
            return false;
        };
        if primary_index == secondary_index {
            return false;
        }
        // A dock restored open shows what it was saved with, so the pairing
        // stays out of it; while it still waits for its saved panel, the
        // pairing waits too rather than deciding without it.
        if self.saved_open() {
            return self.active_panel_index.is_some();
        }

        if during_startup && !self.arranged_by_user {
            if self.saved_pair() {
                return true;
            }
            // The pair takes the saved slots, as if the dock had been saved
            // that way: every later startup event works the dock out again
            // from its saved state, so this is what keeps the pair. Placing
            // the panels that ask to start open that way also shows only
            // those, the pair in its configured order when both ask.
            let saved = self.serialized_dock.get_or_insert_with(DockData::default);
            saved.active_panel = Some(primary_name.to_string());
            saved.secondary_panel = Some(secondary_name.to_string());
            // Saved open on no panel at all, as `toggle_dock` leaves a dock
            // without one to show: the pair must not open it.
            saved.visible = false;
            self.apply_startup_arrangement(window, cx);
            return true;
        }
        // A dock already showing a pair was arranged by the user (or by an
        // earlier run of this): what it shows is theirs.
        if self.secondary_panel_index().is_some() {
            return true;
        }
        let mut target = self.arrangement();
        if target.open {
            // Opened by the user: what it shows stays, and a half of the pair
            // it does not show waits behind its button, which puts the pair
            // back together in its configured order.
            if target.active == Some(primary_index) {
                target.keep_off_screen(secondary_index, false);
            } else if target.active == Some(secondary_index) {
                target.keep_off_screen(primary_index, true);
            }
        } else {
            target.active = Some(primary_index);
            target.secondary = Some(secondary_index);
            target.hidden = None;
        }
        self.arrange(target, window, cx);
        true
    }

    /// Refuses every automatic open of this dock for the rest of its life. See
    /// [`Dock::starts_open_suppressed`].
    pub(crate) fn suppress_starts_open(&mut self) {
        self.starts_open_suppressed = true;
    }

    /// Brings the dock up to date with the panels that ask to start open, for
    /// an answer that changes after the panel is added, like the project
    /// panel's once the first worktree scan is in. While the window starts up
    /// this works the whole dock out again ([`Dock::startup_arrangement`]).
    /// Afterwards only a panel whose answer turned to yes since the last pass
    /// is placed, under the rules of [`Dock::arrangement_for_starts_open_panel`],
    /// so whatever else changed the dock in the meantime stays. Either way a
    /// dock the user rearranged is left alone, and so is one restored open.
    /// Returns whether a panel of this dock asks to start open.
    pub(crate) fn ensure_starts_open_panels(
        &mut self,
        during_startup: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let found = self.starts_open_panel_index(window, cx).is_some();
        if during_startup {
            self.apply_startup_arrangement(window, cx);
            return found;
        }
        if !self.arranged_by_user && !self.saved_open() {
            let turned_to_start_open = self
                .panel_entries
                .iter()
                .enumerate()
                .filter(|(index, entry)| {
                    !entry.starts_open_seen && self.panel_starts_open(*index, window, cx)
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            let mut target = self.arrangement();
            for index in turned_to_start_open {
                target = self.arrangement_for_starts_open_panel(target, index, window, cx);
            }
            self.arrange(target, window, cx);
        }
        self.reveal_panels_that_start_open(window, cx);
        self.record_starts_open_answers(window, cx);
        found
    }

    /// The first panel of this dock that asks to start open.
    fn starts_open_panel_index(&self, window: &Window, cx: &App) -> Option<usize> {
        (0..self.panel_entries.len()).find(|index| self.panel_starts_open(*index, window, cx))
    }

    /// Whether `index` asks to start open, in a dock that allows that.
    fn panel_starts_open(&self, index: usize, window: &Window, cx: &App) -> bool {
        !self.starts_open_suppressed
            && self
                .panel_at(index)
                .is_some_and(|panel| panel.starts_open(window, cx))
    }

    /// Whether the saved state restored into this dock had it open on a panel.
    fn saved_open(&self) -> bool {
        self.serialized_dock
            .as_ref()
            .is_some_and(|saved| saved.visible && saved.active_panel.is_some())
    }

    /// Whether the saved state stacks two panels in this dock.
    fn saved_pair(&self) -> bool {
        self.serialized_dock.as_ref().is_some_and(|saved| {
            saved.active_panel.is_some()
                && saved.secondary_panel.is_some()
                && saved.active_panel != saved.secondary_panel
        })
    }

    /// The panels the saved state has on top and stacked below it, as far as
    /// they have been added.
    fn saved_slots(&self, cx: &App) -> (Option<usize>, Option<usize>) {
        let Some(saved) = self.serialized_dock.as_ref() else {
            return (None, None);
        };
        let index = |name: &Option<String>| {
            name.as_deref()
                .and_then(|name| self.panel_index_for_persistent_name(name, cx))
        };
        (index(&saved.active_panel), index(&saved.secondary_panel))
    }

    /// Where `index`, a panel asking to start open, goes in `target`. Only
    /// panels that asked appear (ADR-F48). A closed dock opens for it: stacked
    /// below a panel that did not ask, it shows alone and the other panel's
    /// button puts that one back on top; anywhere else it takes the primary
    /// slot, and the panel stacked there stays only if it asked to start open
    /// too. Once an earlier panel that asked has opened the dock, the new one
    /// stacks beside it if that slot is free, and otherwise waits for its
    /// button. No dock open for another reason gets here: a dock restored
    /// open shows only its saved panels, and one the user opened is theirs.
    fn arrangement_for_starts_open_panel(
        &self,
        mut target: Arrangement,
        index: usize,
        window: &Window,
        cx: &App,
    ) -> Arrangement {
        if target.open {
            if target.active.is_none() {
                target.promote(index);
            } else if !target.shows(index) && target.secondary.is_none() {
                target.secondary = Some(index);
            }
            return target;
        }
        target.open = true;
        if let Some(primary_index) = target.active
            && target.secondary == Some(index)
        {
            if !self.panel_starts_open(primary_index, window, cx) {
                target.active = Some(index);
                target.secondary = None;
                target.keep_off_screen(primary_index, true);
            }
            return target;
        }
        target.promote(index);
        if let Some(secondary_index) = target.secondary
            && !self.panel_starts_open(secondary_index, window, cx)
        {
            target.secondary = None;
            target.keep_off_screen(secondary_index, false);
        }
        target
    }

    /// `target` with every panel of this dock that asks to start open placed,
    /// in the order the panels are in.
    fn arrangement_with_starts_open_panels(
        &self,
        mut target: Arrangement,
        window: &Window,
        cx: &App,
    ) -> Arrangement {
        for index in 0..self.panel_entries.len() {
            if self.panel_starts_open(index, window, cx) {
                target = self.arrangement_for_starts_open_panel(target, index, window, cx);
            }
        }
        target
    }

    /// What the dock shows while the window starts up, worked out from
    /// scratch from its saved state, the panels it has and whether each asks
    /// to start open, so that the order these arrive in does not matter
    /// (ADR-F48). A dock restored open shows exactly the panels it was saved
    /// with, waiting closed for the first of them until startup is over
    /// ([`Dock::stop_waiting_for_saved_panels`]), and a panel asking to start
    /// open waits for its button there. Any other dock keeps the saved panels
    /// in their slots for whenever it is opened, and opens for the panels
    /// that ask to start open. When a half of the fork's default pair is one
    /// of those, the pair takes the slots instead, whatever was saved: the
    /// hidden half of a pair is not saved, so the saved slots cannot say how
    /// the pair goes back together.
    fn startup_arrangement(&self, window: &Window, cx: &App) -> Arrangement {
        let (saved_top, saved_stacked) = self.saved_slots(cx);
        // The stacked panel stands in for a top one that has not arrived.
        let active = saved_top.or(saved_stacked);
        let mut slots = Arrangement {
            open: false,
            active,
            secondary: saved_top.and(saved_stacked),
            hidden: None,
        };
        if self.saved_open() {
            return Arrangement {
                open: active.is_some(),
                ..slots
            };
        }
        if let Some((primary_index, secondary_index)) = self.default_pair_indices(cx)
            && (self.panel_starts_open(primary_index, window, cx)
                || self.panel_starts_open(secondary_index, window, cx))
        {
            slots.active = Some(primary_index);
            slots.secondary = Some(secondary_index);
        }
        self.arrangement_with_starts_open_panels(slots, window, cx)
    }

    /// The panels of the fork's default pair, top one first, when the pair is
    /// configured for this dock and both of them are here.
    fn default_pair_indices(&self, cx: &App) -> Option<(usize, usize)> {
        let (position, primary_name, secondary_name) = Workspace::default_stacked_layout(cx)?;
        if position != self.position {
            return None;
        }
        let primary_index = self.panel_index_for_persistent_name(primary_name, cx)?;
        let secondary_index = self.panel_index_for_persistent_name(secondary_name, cx)?;
        (primary_index != secondary_index).then_some((primary_index, secondary_index))
    }

    /// Moves the dock to its [`Dock::startup_arrangement`], unless something
    /// else has rearranged it, and zooms the saved top panel back in.
    fn apply_startup_arrangement(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.arranged_by_user {
            return;
        }
        let target = self.startup_arrangement(window, cx);
        self.arrange(target, window, cx);
        self.reveal_panels_that_start_open(window, cx);
        self.record_starts_open_answers(window, cx);
        let (saved_top, _) = self.saved_slots(cx);
        if self
            .serialized_dock
            .as_ref()
            .is_some_and(|saved| saved.zoom)
            && saved_top.is_some()
            && self.active_panel_index == saved_top
            && let Some(panel) = self.active_panel().cloned()
            && !panel.is_zoomed(window, cx)
        {
            panel.set_zoomed(true, window, cx);
        }
    }

    /// Run once the workspace has started up. A dock restored open whose
    /// saved panels never arrived - removed from Zed, renamed, or failed to
    /// load - is taken as saved closed from then on, so the panels that ask
    /// to start open open it after all, now or once their answer changes.
    pub(crate) fn stop_waiting_for_saved_panels(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.saved_open() || self.active_panel_index.is_some() {
            return;
        }
        if let Some(saved) = self.serialized_dock.as_mut() {
            saved.visible = false;
        }
        self.apply_startup_arrangement(window, cx);
    }

    /// Tells the panels the open dock shows and that asked to start open that
    /// the user asked for them (see `Panel::revealed_by_user`). No click
    /// reaches the dock for those, but `browser.starts_open` and its kin are
    /// the user saying "open this every time" in their settings, which is as
    /// much of a request as pressing the button (ADR-F48). Panels that are
    /// only on screen because they share the dock with one of them are not
    /// told. Calling this again is harmless: a panel hears it once and the
    /// startup passes run many times over.
    fn reveal_panels_that_start_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_open {
            return;
        }
        for index in self.stacked_panel_indices().into_iter().flatten() {
            if self.panel_starts_open(index, window, cx) {
                self.reveal_panel_to_user(index, window, cx);
            }
        }
    }

    fn record_starts_open_answers(&mut self, window: &Window, cx: &App) {
        let answers = (0..self.panel_entries.len())
            .map(|index| self.panel_starts_open(index, window, cx))
            .collect::<Vec<_>>();
        for (entry, answer) in self.panel_entries.iter_mut().zip(answers) {
            entry.starts_open_seen = answer;
        }
    }

    fn arrangement(&self) -> Arrangement {
        Arrangement {
            open: self.is_open,
            active: self.active_panel_index,
            secondary: self.secondary_panel_index(),
            hidden: self.hidden_stacked_panel,
        }
    }

    /// Moves the dock to `target` in one step, telling only the panels whose
    /// visibility changes, each of them once: the ones leaving the screen
    /// first, then the ones entering it. Going through the individual steps
    /// one after another can show a panel and hide it again, or the reverse,
    /// within one update, and such a round trip is not free: the browser panel
    /// hides its WebView and loads its home page, the terminal panel spawns a
    /// shell nobody sees.
    fn arrange(&mut self, target: Arrangement, window: &mut Window, cx: &mut Context<Self>) {
        let panel_count = self.panel_entries.len();
        let active = target.active.filter(|index| *index < panel_count);
        let previously_visible = self.visible_panels().cloned().collect::<Vec<_>>();
        self.is_open = target.open;
        self.active_panel_index = active;
        self.secondary_panel_index = target
            .secondary
            .filter(|index| active.is_some() && *index < panel_count && Some(*index) != active);
        self.hidden_stacked_panel = target.hidden;
        let visible = self.visible_panels().cloned().collect::<Vec<_>>();
        for panel in &previously_visible {
            if !visible
                .iter()
                .any(|shown| shown.panel_id() == panel.panel_id())
            {
                panel.set_active(false, window, cx);
            }
        }
        for panel in &visible {
            if !previously_visible
                .iter()
                .any(|shown| shown.panel_id() == panel.panel_id())
            {
                panel.set_active(true, window, cx);
            }
        }
        cx.notify();
    }

    /// Opens the dock on `index`. The panel takes the primary slot, unless it
    /// is already stacked on screen and `promote_stacked` is false.
    pub(crate) fn show_panel(
        &mut self,
        index: usize,
        promote_stacked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if index >= self.panel_entries.len() {
            return;
        }
        self.arranged_by_user = true;
        let mut target = self.arrangement();
        target.open = true;
        if promote_stacked || target.secondary != Some(index) {
            target.promote(index);
        }
        self.arrange(target, window, cx);
        self.reveal_panel_to_user(index, window, cx);
    }

    pub fn set_open(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.arranged_by_user = true;
        if open != self.is_open {
            let mut target = self.arrangement();
            target.open = open;
            self.arrange(target, window, cx);
        }
    }

    pub fn set_panel_zoomed(
        &mut self,
        panel: &AnyView,
        zoomed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.arranged_by_user = true;
        // Zoom shows a single panel, so a stacked secondary has to become the
        // primary before it can take over the dock.
        if zoomed
            && let Some(index) = self
                .panel_entries
                .iter()
                .position(|entry| entry.panel.panel_id() == panel.entity_id())
            && self.secondary_panel_index() == Some(index)
        {
            self.activate_panel(index, window, cx);
        }

        for entry in &mut self.panel_entries {
            if entry.panel.panel_id() == panel.entity_id() {
                if zoomed != entry.panel.is_zoomed(window, cx) {
                    entry.panel.set_zoomed(zoomed, window, cx);
                }
            } else if entry.panel.is_zoomed(window, cx) {
                entry.panel.set_zoomed(false, window, cx);
            }
        }

        self.workspace
            .update(cx, |workspace, cx| {
                workspace.serialize_workspace(window, cx);
            })
            .ok();
        cx.notify();
    }

    pub fn zoom_out(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for entry in &mut self.panel_entries {
            if entry.panel.is_zoomed(window, cx) {
                entry.panel.set_zoomed(false, window, cx);
            }
        }
    }

    /// Adds `panel` to the dock. `during_startup` says whether the window is
    /// still starting up (`Workspace::is_starting_up`): only then do the saved
    /// state and `Panel::starts_open` give the panel a place on screen.
    pub(crate) fn add_panel<T: Panel>(
        &mut self,
        panel: Entity<T>,
        workspace: WeakEntity<Workspace>,
        during_startup: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let subscriptions = [
            cx.observe(&panel, |_, _, cx| cx.notify()),
            cx.observe_global_in::<SettingsStore>(window, {
                let workspace = workspace.clone();
                let panel = panel.clone();

                move |this, window, cx| {
                    let new_position = panel.read(cx).position(window, cx);
                    let panel_id = Entity::entity_id(&panel);
                    if new_position == this.position {
                        // A drop that asked to move this panel elsewhere and
                        // left it here - a project setting pins its dock, say -
                        // must not leave its drop state behind for a later
                        // move to pick up. The settings change that should
                        // have moved it is the only one expected meanwhile.
                        let position = this.position;
                        let stale_drop = workspace
                            .read_with(cx, |workspace, _| {
                                workspace.dropping_panel.is_some_and(|(id, target)| {
                                    id == panel_id && target != position
                                })
                            })
                            .unwrap_or(false);
                        if stale_drop {
                            workspace
                                .update(cx, |workspace, _| workspace.forget_panel_drop())
                                .ok();
                        }
                        return;
                    }

                    let Ok((new_dock, being_dropped, during_startup)) =
                        workspace.update(cx, |workspace, cx| {
                            if panel.is_zoomed(window, cx) {
                                workspace.zoomed_position = Some(new_position);
                            }
                            let new_dock = match new_position {
                                DockPosition::Left => &workspace.left_dock,
                                DockPosition::Bottom => &workspace.bottom_dock,
                                DockPosition::Right => &workspace.right_dock,
                            }
                            .clone();
                            let being_dropped = workspace
                                .dropping_panel
                                .is_some_and(|(id, _)| id == panel_id);
                            (new_dock, being_dropped, workspace.is_starting_up())
                        })
                    else {
                        return;
                    };

                    let was_visible = this
                        .visible_panels()
                        .any(|visible_panel| visible_panel.panel_id() == panel_id);
                    let size_state = this
                        .panel_entries
                        .iter()
                        .find(|entry| entry.panel.panel_id() == panel_id)
                        .map(|entry| entry.size_state)
                        .unwrap_or_default();

                    let previous_axis = this.position.axis();
                    let next_axis = new_position.axis();
                    let size_state = if previous_axis == next_axis {
                        size_state
                    } else {
                        PanelSizeState::default()
                    };

                    // A panel moved while the window starts up - by a project's
                    // own settings, say - out of a dock the startup rules
                    // arranged is placed by them in both docks, like any panel
                    // added then. One the user arranged, or is working in,
                    // keeps what they see, and a drop gives the panel its slot
                    // itself.
                    let focused =
                        PanelHandle::panel_focus_handle(&panel, cx).contains_focused(window, cx);
                    let placed_by_startup_rules =
                        during_startup && !being_dropped && !this.arranged_by_user && !focused;
                    if !this.remove_panel(&panel, window, cx) {
                        // Panel was already moved from this dock
                        return;
                    }
                    if placed_by_startup_rules {
                        this.apply_startup_arrangement(window, cx);
                    }

                    new_dock.update(cx, |new_dock, cx| {
                        let index = new_dock.add_panel(
                            panel.clone(),
                            workspace.clone(),
                            placed_by_startup_rules,
                            window,
                            cx,
                        );
                        if let Some(added_panel) = new_dock.panel_for_id(panel_id).cloned() {
                            new_dock.set_panel_size_state(added_panel.as_ref(), size_state, cx);
                        }
                        // A panel that was on screen stays on screen in its new
                        // dock, unless the startup rules or a drop place it.
                        if was_visible && !being_dropped && !placed_by_startup_rules {
                            new_dock.show_panel(index, true, window, cx);
                        } else if placed_by_startup_rules
                            && was_visible
                            && !new_dock
                                .visible_panels()
                                .any(|visible_panel| visible_panel.panel_id() == panel_id)
                        {
                            // Leaving the old dock took it off screen without
                            // telling it.
                            panel.set_active(false, window, cx);
                        }
                    });

                    workspace
                        .update(cx, |workspace, cx| {
                            workspace.serialize_workspace(window, cx);
                        })
                        .ok();
                }
            }),
            {
                let panel = panel.clone();
                let mut last_default_size = panel.read(cx).default_size(window, cx);

                cx.observe_global_in::<SettingsStore>(window, move |this, window, cx| {
                    let default_size = panel.read(cx).default_size(window, cx);
                    if default_size == last_default_size {
                        return;
                    }
                    last_default_size = default_size;

                    let panel_id = Entity::entity_id(&panel);
                    if let Some(entry) = this
                        .panel_entries
                        .iter_mut()
                        .find(|entry| entry.panel.panel_id() == panel_id)
                    {
                        entry.size_state.size = None;
                        entry.panel.size_state_changed(window, cx);
                        cx.notify();
                    }
                })
            },
            cx.subscribe_in(
                &panel,
                window,
                move |this, panel, event, window, cx| match event {
                    PanelEvent::ZoomIn => {
                        this.set_panel_zoomed(&panel.to_any(), true, window, cx);
                        if !PanelHandle::panel_focus_handle(panel, cx).contains_focused(window, cx)
                        {
                            window.focus(&panel.read(cx).activation_focus_handle(cx), cx);
                        }
                        workspace
                            .update(cx, |workspace, cx| {
                                workspace.zoomed = Some(panel.downgrade().into());
                                workspace.zoomed_position =
                                    Some(panel.read(cx).position(window, cx));
                                cx.emit(Event::ZoomChanged);
                            })
                            .ok();
                    }
                    PanelEvent::ZoomOut => {
                        this.set_panel_zoomed(&panel.to_any(), false, window, cx);
                        workspace
                            .update(cx, |workspace, cx| {
                                if workspace.zoomed_position == Some(this.position) {
                                    workspace.zoomed = None;
                                    workspace.zoomed_position = None;
                                    cx.emit(Event::ZoomChanged);
                                }
                                cx.notify();
                            })
                            .ok();
                    }
                    PanelEvent::Activate => {
                        if let Some(ix) = this
                            .panel_entries
                            .iter()
                            .position(|entry| entry.panel.panel_id() == Entity::entity_id(panel))
                        {
                            // A stacked panel is already on screen, so focus it
                            // instead of swapping it into the primary slot.
                            this.show_panel(ix, false, window, cx);
                            window.focus(&panel.read(cx).activation_focus_handle(cx), cx);
                        }
                    }
                    PanelEvent::Close => {
                        let panel_id = Entity::entity_id(panel);
                        if this
                            .secondary_panel()
                            .is_some_and(|p| p.panel_id() == panel_id)
                        {
                            this.set_secondary_panel(None, window, cx);
                        } else if this
                            .visible_panel()
                            .is_some_and(|p| p.panel_id() == panel_id)
                        {
                            this.set_open(false, window, cx);
                        }
                    }
                },
            ),
        ];

        let index = match self
            .panel_entries
            .binary_search_by_key(&panel.read(cx).activation_priority(), |entry| {
                entry.panel.activation_priority(cx)
            }) {
            Ok(ix) => {
                if cfg!(debug_assertions) {
                    panic!(
                        "Panels `{}` and `{}` have the same activation priority. Each panel must have a unique priority so the status bar order is deterministic.",
                        T::panel_key(),
                        self.panel_entries[ix].panel.panel_key()
                    );
                }
                ix
            }
            Err(ix) => ix,
        };
        if let Some(active_index) = self.active_panel_index.as_mut()
            && *active_index >= index
        {
            *active_index += 1;
        }
        if let Some(secondary_index) = self.secondary_panel_index.as_mut()
            && *secondary_index >= index
        {
            *secondary_index += 1;
        }
        if let Some(hidden) = self.hidden_stacked_panel.as_mut()
            && hidden.index >= index
        {
            hidden.index += 1;
        }
        let size_state = panel.read(cx).initial_size_state(window, cx);
        let starts_open_seen =
            !self.starts_open_suppressed && panel.read(cx).starts_open(window, cx);

        self.panel_entries.insert(
            index,
            PanelEntry {
                panel: Arc::new(panel.clone()),
                size_state,
                starts_open_seen,
                _subscriptions: subscriptions,
            },
        );

        // The saved state and `Panel::starts_open` only decide what the dock
        // shows while the window starts up. A panel added later - moved here,
        // or the agent panel coming back when AI is turned on again - leaves
        // the dock as it is.
        if during_startup {
            self.apply_startup_arrangement(window, cx);
        }

        cx.notify();
        index
    }

    /// Applies [`Dock::serialized_dock`], the state saved for this dock, when
    /// a workspace is restored, together with the panels that ask to start
    /// open, in one step ([`Dock::startup_arrangement`]). The saved state
    /// replaces whatever the dock had, a click made before it arrived
    /// included. A panel asking to start open that arrived before it may
    /// already have opened the dock, though, and a dock saved open without
    /// that panel takes it off screen again here: nothing tells the dock that
    /// a restore is coming until it comes.
    pub fn restore_state(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(saved) = self.serialized_dock.as_ref() else {
            return false;
        };
        if let Some(ratio) = saved.secondary_ratio {
            self.secondary_ratio = clamp_secondary_ratio(ratio);
        }
        self.arranged_by_user = false;
        self.apply_startup_arrangement(window, cx);
        true
    }

    pub fn remove_panel<T: Panel>(
        &mut self,
        panel: &Entity<T>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Some(panel_ix) = self
            .panel_entries
            .iter()
            .position(|entry| entry.panel.panel_id() == Entity::entity_id(panel))
        {
            let shift = |slot: Option<usize>| match slot {
                Some(index) => match panel_ix.cmp(&index) {
                    std::cmp::Ordering::Less => Some(index - 1),
                    std::cmp::Ordering::Equal => None,
                    std::cmp::Ordering::Greater => Some(index),
                },
                None => None,
            };
            let mut active_panel_index = shift(self.active_panel_index);
            let mut secondary_panel_index = shift(self.secondary_panel_index);
            // Losing the primary panel shouldn't hide a stacked panel the user can
            // still see, so promote it instead of closing the dock.
            if active_panel_index.is_none()
                && let Some(promoted) = secondary_panel_index.take()
            {
                active_panel_index = Some(promoted);
            }
            let close_dock = active_panel_index.is_none();
            self.active_panel_index = active_panel_index;
            self.secondary_panel_index = secondary_panel_index;
            self.hidden_stacked_panel = self.hidden_stacked_panel.and_then(|hidden| {
                shift(Some(hidden.index)).map(|index| HiddenStackedPanel { index, ..hidden })
            });

            self.panel_entries.remove(panel_ix);
            if close_dock {
                // Not `set_open`: losing a panel is not the user rearranging
                // the dock.
                let mut target = self.arrangement();
                target.open = false;
                self.arrange(target, window, cx);
            }
            cx.notify();

            true
        } else {
            false
        }
    }

    pub fn panels_len(&self) -> usize {
        self.panel_entries.len()
    }

    pub fn has_agent_panel(&self, cx: &App) -> bool {
        self.panel_entries
            .iter()
            .any(|entry| entry.panel.is_agent_panel(cx))
    }

    pub fn activate_panel(&mut self, panel_ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.arranged_by_user = true;
        if Some(panel_ix) != self.active_panel_index {
            // Activating the stacked panel promotes it rather than hiding it: both
            // panels stay on screen, they just swap slots. In a closed dock this
            // only picks the panel the dock will open on.
            let mut target = self.arrangement();
            target.promote(panel_ix);
            self.arrange(target, window, cx);
        }
        self.reveal_panel_to_user(panel_ix, window, cx);
    }

    /// Tells the panel at `index` that the user asked for it (see
    /// `Panel::revealed_by_user`). Callers that only open the dock, or that show
    /// a panel because it shares the dock with the one being asked for, must not
    /// call this. The panel still has to hold one of the two slots: a dock that
    /// is only picking which panel it would open on has revealed nothing.
    pub(crate) fn reveal_panel_to_user(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active_panel_index != Some(index) && self.secondary_panel_index() != Some(index) {
            return;
        }
        let Some(panel) = self
            .panel_entries
            .get(index)
            .map(|entry| entry.panel.clone())
        else {
            return;
        };
        panel.revealed_by_user(window, cx);
    }

    pub fn visible_panel(&self) -> Option<&Arc<dyn PanelHandle>> {
        let entry = self.visible_entry()?;
        Some(&entry.panel)
    }

    pub fn active_panel(&self) -> Option<&Arc<dyn PanelHandle>> {
        let panel_entry = self.active_panel_entry()?;
        Some(&panel_entry.panel)
    }

    fn visible_entry(&self) -> Option<&PanelEntry> {
        if self.is_open {
            self.active_panel_entry()
        } else {
            None
        }
    }

    pub fn zoomed_panel(&self, window: &Window, cx: &App) -> Option<Arc<dyn PanelHandle>> {
        let entry = self.visible_entry()?;
        if entry.panel.is_zoomed(window, cx) {
            Some(entry.panel.clone())
        } else {
            None
        }
    }

    pub fn active_panel_size(&self) -> Option<PanelSizeState> {
        if self.is_open {
            self.active_panel_entry().map(|entry| entry.size_state)
        } else {
            None
        }
    }

    pub fn stored_panel_size(
        &self,
        panel: &dyn PanelHandle,
        window: &Window,
        cx: &App,
    ) -> Option<Pixels> {
        self.panel_entries
            .iter()
            .find(|entry| entry.panel.panel_id() == panel.panel_id())
            .map(|entry| {
                entry
                    .size_state
                    .size
                    .unwrap_or_else(|| entry.panel.default_size(window, cx))
            })
    }

    pub fn stored_panel_size_state(&self, panel: &dyn PanelHandle) -> Option<PanelSizeState> {
        self.panel_entries
            .iter()
            .find(|entry| entry.panel.panel_id() == panel.panel_id())
            .map(|entry| entry.size_state)
    }

    pub fn stored_active_panel_size(&self, window: &Window, cx: &App) -> Option<Pixels> {
        if self.is_open {
            self.active_panel_entry().map(|entry| {
                entry
                    .size_state
                    .size
                    .unwrap_or_else(|| entry.panel.default_size(window, cx))
            })
        } else {
            None
        }
    }

    pub fn set_panel_size_state(
        &mut self,
        panel: &dyn PanelHandle,
        size_state: PanelSizeState,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Some(entry) = self
            .panel_entries
            .iter_mut()
            .find(|entry| entry.panel.panel_id() == panel.panel_id())
        {
            entry.size_state = size_state;
            cx.notify();
            true
        } else {
            false
        }
    }

    pub fn toggle_panel_flexible_size(
        &mut self,
        panel: &dyn PanelHandle,
        current_size: Option<Pixels>,
        current_flex: Option<f32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self
            .panel_entries
            .iter_mut()
            .find(|entry| entry.panel.panel_id() == panel.panel_id())
        else {
            return;
        };
        let currently_flexible = entry.panel.has_flexible_size(window, cx);
        if currently_flexible {
            entry.size_state.size = current_size;
        } else {
            entry.size_state.flex = current_flex;
        }
        let panel_key = entry.panel.panel_key();
        let size_state = entry.size_state;
        let workspace = self.workspace.clone();
        entry
            .panel
            .set_flexible_size(!currently_flexible, window, cx);
        entry.panel.size_state_changed(window, cx);
        cx.defer(move |cx| {
            if let Some(workspace) = workspace.upgrade() {
                workspace.update(cx, |workspace, cx| {
                    workspace.persist_panel_size_state(panel_key, size_state, cx);
                });
            }
        });
        cx.notify();
    }

    fn resize_active_panel(
        &mut self,
        size: Option<Pixels>,
        flex: Option<f32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let position = self.position;
        if let Some(entry) = self.active_panel_entry_mut() {
            let (panel_key, size_state) =
                resize_panel_entry(position, entry, size, flex, window, cx);

            let workspace = self.workspace.clone();
            cx.defer(move |cx| {
                if let Some(workspace) = workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        workspace.persist_panel_size_state(panel_key, size_state, cx);
                    });
                }
            });
            cx.notify();
        }
    }

    /// Resizes the active panel and, when this dock is included in
    /// `resize_all_panels_in_dock`, all panels using the same sizing mode.
    pub fn resize_panel_sizes(
        &mut self,
        size: Option<Pixels>,
        flex: Option<f32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.should_resize_all_panels(cx) {
            self.resize_all_panels(size, flex, window, cx);
        } else {
            self.resize_active_panel(size, flex, window, cx);
        }
    }

    /// Resets the active panel and, when this dock is included in
    /// `resize_all_panels_in_dock`, all panels using the same sizing mode.
    pub fn reset_panel_sizes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.should_resize_all_panels(cx) {
            self.reset_all_panel_sizes(window, cx);
        } else {
            self.reset_active_panel_size(window, cx);
        }
    }

    fn reset_active_panel_size(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = PanelSizeState::default();
        self.resize_active_panel(state.size, state.flex, window, cx);
    }

    fn reset_all_panel_sizes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active_entry) = self.active_panel_entry() else {
            return;
        };
        let size =
            (!panel_uses_flexible_width(self.position, active_entry.panel.as_ref(), window, cx))
                .then(|| active_entry.panel.default_size(window, cx));
        self.resize_all_panels(size, None, window, cx);
    }

    fn should_resize_all_panels(&self, cx: &App) -> bool {
        WorkspaceSettings::get_global(cx)
            .resize_all_panels_in_dock
            .contains(&self.position)
    }

    fn resize_all_panels(
        &mut self,
        size: Option<Pixels>,
        flex: Option<f32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(active_panel_index) = self.active_panel_index else {
            return;
        };

        let active_panel_uses_flexible_width = {
            let Some(active_entry) = self.panel_entries.get(active_panel_index) else {
                return;
            };
            panel_uses_flexible_width(self.position, active_entry.panel.as_ref(), window, cx)
        };
        let mut size_states_to_persist = Vec::new();
        for entry in &mut self.panel_entries {
            if panel_uses_flexible_width(self.position, entry.panel.as_ref(), window, cx)
                == active_panel_uses_flexible_width
            {
                size_states_to_persist.push(resize_panel_entry(
                    self.position,
                    entry,
                    size,
                    flex,
                    window,
                    cx,
                ));
            }
        }

        let workspace = self.workspace.clone();
        cx.defer(move |cx| {
            if let Some(workspace) = workspace.upgrade() {
                workspace.update(cx, |workspace, cx| {
                    for (panel_key, size_state) in size_states_to_persist {
                        workspace.persist_panel_size_state(panel_key, size_state, cx);
                    }
                });
            }
        });

        cx.notify();
    }

    /// Moves the divider between the stacked panels. `bounds` is the container
    /// holding both panels; the secondary one always sits at the far end of it.
    fn resize_secondary(
        &mut self,
        bounds: Bounds<Pixels>,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let ratio = match self.stack_axis() {
            Axis::Vertical => {
                if bounds.size.height <= px(0.) {
                    return;
                }
                (bounds.bottom() - position.y) / bounds.size.height
            }
            Axis::Horizontal => {
                if bounds.size.width <= px(0.) {
                    return;
                }
                (bounds.right() - position.x) / bounds.size.width
            }
        };
        self.set_secondary_ratio(ratio, cx);
    }

    pub fn toggle_action(&self) -> Box<dyn Action> {
        match self.position {
            DockPosition::Left => crate::ToggleLeftDock.boxed_clone(),
            DockPosition::Bottom => crate::ToggleBottomDock.boxed_clone(),
            DockPosition::Right => crate::ToggleRightDock.boxed_clone(),
        }
    }

    fn dispatch_context() -> KeyContext {
        let mut dispatch_context = KeyContext::new_with_defaults();
        dispatch_context.add("Dock");

        dispatch_context
    }

    pub fn clamp_panel_size(&mut self, max_size: Pixels, window: &Window, cx: &mut Context<Self>) {
        let max_size = (max_size - RESIZE_HANDLE_SIZE).abs();
        let mut clamped = false;
        for entry in &mut self.panel_entries {
            let uses_flexible_width =
                panel_uses_flexible_width(self.position, entry.panel.as_ref(), window, cx);
            if uses_flexible_width {
                continue;
            }

            let size = entry
                .size_state
                .size
                .unwrap_or_else(|| entry.panel.default_size(window, cx));
            if size > max_size {
                entry.size_state.size = Some(max_size.max(RESIZE_HANDLE_SIZE));
                clamped = true;
            }
        }
        if clamped {
            cx.notify();
        }
    }

    /// Renders the primary panel, plus the stacked secondary one and the divider
    /// between them when the dock is showing two panels.
    fn render_panels(&self, cx: &mut Context<Self>) -> AnyElement {
        let panel_element = |panel: &Arc<dyn PanelHandle>| {
            panel
                .to_any()
                .cached(StyleRefinement::default().v_flex().size_full())
        };
        let Some(primary) = self.visible_entry().map(|entry| entry.panel.clone()) else {
            return div().into_any_element();
        };
        let Some(secondary) = self.secondary_panel().cloned() else {
            return div()
                .size_full()
                .child(panel_element(&primary))
                .into_any_element();
        };

        let stack_axis = self.stack_axis();
        let position = self.position;
        let secondary_ratio = self.secondary_ratio;
        let sized = |mut element: Div, share: f32| {
            let style = element.style();
            style.flex_grow = Some(share.max(0.001));
            style.flex_shrink = Some(1.0);
            style.flex_basis = Some(relative(0.).into());
            element
        };

        let divider = div()
            .id("dock-split-handle")
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .map(|this| match stack_axis {
                Axis::Vertical => this.w_full().h(RESIZE_HANDLE_SIZE).cursor_row_resize(),
                Axis::Horizontal => this.h_full().w(RESIZE_HANDLE_SIZE).cursor_col_resize(),
            })
            .child(
                div()
                    .bg(cx.theme().colors().border)
                    .map(|this| match stack_axis {
                        Axis::Vertical => this.w_full().h(px(1.)),
                        Axis::Horizontal => this.h_full().w(px(1.)),
                    }),
            )
            // No `on_mouse_down` here: a listener that stops propagation runs
            // before the drag machinery's own mouse-down listener and would
            // keep the drag from ever starting (the handle is `occlude()`d, so
            // nothing underneath sees the click anyway).
            .on_drag(DraggedDockSplit(position), |split, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| split.clone())
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|dock, e: &MouseUpEvent, window, cx| {
                    if e.click_count == 2 {
                        dock.set_secondary_ratio(DEFAULT_SECONDARY_RATIO, cx);
                        dock.workspace
                            .update(cx, |workspace, cx| {
                                workspace.serialize_workspace(window, cx);
                            })
                            .ok();
                        cx.stop_propagation();
                    }
                }),
            );

        div()
            .id("dock-panel-stack")
            .size_full()
            .flex()
            .map(|this| match stack_axis {
                Axis::Vertical => this.flex_col(),
                Axis::Horizontal => this.flex_row(),
            })
            .on_drag_move(cx.listener(
                move |dock, e: &DragMoveEvent<DraggedDockSplit>, window, cx| {
                    if e.drag(cx).0 != dock.position {
                        return;
                    }
                    dock.resize_secondary(e.bounds, e.event.position, cx);
                    dock.workspace
                        .update(cx, |workspace, cx| {
                            workspace.serialize_workspace(window, cx);
                        })
                        .ok();
                },
            ))
            .child(sized(
                div().overflow_hidden().child(panel_element(&primary)),
                1.0 - secondary_ratio,
            ))
            .child(divider)
            .child(sized(
                div().overflow_hidden().child(panel_element(&secondary)),
                secondary_ratio,
            ))
            .into_any_element()
    }

    pub(crate) fn load_persisted_size_state(
        workspace: &Workspace,
        panel_key: &'static str,
        cx: &App,
    ) -> Option<PanelSizeState> {
        let workspace_id = workspace
            .database_id()
            .map(|id| i64::from(id).to_string())
            .or(workspace.session_id())?;
        let kvp = KeyValueStore::global(cx);
        let scope = kvp.scoped(PANEL_SIZE_STATE_KEY);
        scope
            .read(&format!("{workspace_id}:{panel_key}"))
            .log_err()
            .flatten()
            .and_then(|json| serde_json::from_str::<PanelSizeState>(&json).log_err())
    }
}

impl Render for Dock {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dispatch_context = Self::dispatch_context();
        if self.visible_entry().is_some() {
            let position = self.position;
            let panels = self.render_panels(cx);
            let create_resize_handle = || {
                let handle = div()
                    .id("resize-handle")
                    .on_drag(DraggedDock(position), |dock, _, _, cx| {
                        cx.stop_propagation();
                        cx.new(|_| dock.clone())
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                        }),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|dock, e: &MouseUpEvent, window, cx| {
                            if e.click_count == 2 {
                                dock.reset_panel_sizes(window, cx);
                                dock.workspace
                                    .update(cx, |workspace, cx| {
                                        workspace.serialize_workspace(window, cx);
                                    })
                                    .ok();
                                cx.stop_propagation();
                            }
                        }),
                    )
                    .occlude();
                match self.position() {
                    DockPosition::Left => deferred(
                        handle
                            .absolute()
                            .right(-RESIZE_HANDLE_SIZE / 2.)
                            .top(px(0.))
                            .h_full()
                            .w(RESIZE_HANDLE_SIZE)
                            .cursor_col_resize(),
                    ),
                    DockPosition::Bottom => deferred(
                        handle
                            .absolute()
                            .top(-RESIZE_HANDLE_SIZE / 2.)
                            .left(px(0.))
                            .w_full()
                            .h(RESIZE_HANDLE_SIZE)
                            .cursor_row_resize(),
                    ),
                    DockPosition::Right => deferred(
                        handle
                            .absolute()
                            .top(px(0.))
                            .left(-RESIZE_HANDLE_SIZE / 2.)
                            .h_full()
                            .w(RESIZE_HANDLE_SIZE)
                            .cursor_col_resize(),
                    ),
                }
            };

            div()
                .id("dock-panel")
                .key_context(dispatch_context)
                .track_focus(&self.focus_handle(cx))
                .focus_follows_mouse(self.focus_follows_mouse, cx)
                .flex()
                .bg(cx.theme().colors().panel_background)
                .border_color(cx.theme().colors().border)
                .overflow_hidden()
                .map(|this| match self.position().axis() {
                    // Width and height are always set on the workspace wrapper in
                    // render_dock, so fill whatever space the wrapper provides.
                    Axis::Horizontal => this.w_full().h_full().flex_row(),
                    Axis::Vertical => this.h_full().w_full().flex_col(),
                })
                .map(|this| match self.position() {
                    DockPosition::Left => this.border_r_1(),
                    DockPosition::Right => this.border_l_1(),
                    DockPosition::Bottom => this.border_t_1(),
                })
                .child(
                    div()
                        .map(|this| match self.position().axis() {
                            Axis::Horizontal => this.w_full().h_full(),
                            Axis::Vertical => this.h_full().w_full(),
                        })
                        .child(panels),
                )
                .when(self.resizable(cx), |this| {
                    this.child(create_resize_handle())
                })
        } else {
            div()
                .id("dock-panel")
                .key_context(dispatch_context)
                .track_focus(&self.focus_handle(cx))
        }
    }
}

impl PanelButtons {
    pub fn new(dock: Entity<Dock>, cx: &mut Context<Self>) -> Self {
        cx.observe(&dock, |_, _, cx| cx.notify()).detach();
        let settings_subscription = cx.observe_global::<SettingsStore>(|_, cx| cx.notify());
        Self {
            dock,
            _settings_subscription: settings_subscription,
        }
    }
}

impl Render for PanelButtons {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dock = self.dock.read(cx);
        let active_index = dock.active_panel_index;
        let secondary_index = dock.secondary_panel_index();
        let hidden_stacked_index = dock.hidden_stacked_panel_index();
        let stack_entry_label = match dock.stack_axis() {
            Axis::Vertical => "下に並べて表示",
            Axis::Horizontal => "横に並べて表示",
        };
        let stack_hint_meta: SharedString =
            format!("右クリック / Shift+クリックで{stack_entry_label}").into();
        let is_open = dock.is_open;
        let dock_position = dock.position;
        // Arranging by hand the dock the fork pairs panels in answers the
        // question that default exists to answer, so it must not be applied
        // later.
        let settles_default_layout = dock_position
            == WorkspaceSettings::get_global(cx)
                .default_stacked_dock
                .into();

        let (menu_anchor, menu_attach) = match dock.position {
            DockPosition::Left => (Anchor::BottomLeft, Anchor::TopLeft),
            DockPosition::Bottom | DockPosition::Right => (Anchor::BottomRight, Anchor::TopRight),
        };

        let dock_entity = self.dock.clone();
        let workspace = dock.workspace.clone();
        let mut buttons: Vec<_> = dock
            .panel_entries
            .iter()
            .enumerate()
            .filter_map(|(i, entry)| {
                let icon = entry.panel.icon(window, cx)?;
                let icon_tooltip = entry
                    .panel
                    .icon_tooltip(window, cx)
                    .ok_or_else(|| {
                        anyhow::anyhow!("can't render a panel button without an icon tooltip")
                    })
                    .log_err()?;
                let name = entry.panel.persistent_name();
                let panel = entry.panel.clone();
                let supports_flexible = panel.supports_flexible_size(cx);
                let currently_flexible = panel.has_flexible_size(window, cx);
                let dock_for_menu = dock_entity.clone();
                let workspace_for_menu = workspace.clone();

                let is_active_button = Some(i) == active_index && is_open;
                let is_stacked_button = Some(i) == secondary_index && is_open;
                let can_stack = !is_active_button && active_index.is_some();
                let shows_in_pair =
                    (is_active_button && secondary_index.is_some()) || is_stacked_button;
                let is_restorable = Some(i) == hidden_stacked_index;
                let (action, tooltip) = if is_active_button {
                    let action = dock.toggle_action();

                    let tooltip: SharedString =
                        format!("{}ドックを閉じる", dock.position.label()).into();

                    (action, tooltip)
                } else {
                    let action = entry.panel.toggle_action(window, cx);

                    (action, icon_tooltip.into())
                };

                // While the dock shows a pair, each button hides or restores its
                // own panel, so it no longer stands for the action (and the key
                // binding) the tooltip would otherwise advertise.
                let (tooltip_title, tooltip_action, tooltip_meta): (
                    SharedString,
                    Option<Box<dyn Action>>,
                    Option<SharedString>,
                ) = if shows_in_pair {
                    (format!("{icon_tooltip}を隠す").into(), None, None)
                } else if is_restorable {
                    (
                        format!("{icon_tooltip}を{stack_entry_label}").into(),
                        None,
                        None,
                    )
                } else if can_stack {
                    (
                        tooltip,
                        Some(action.boxed_clone()),
                        Some(stack_hint_meta.clone()),
                    )
                } else {
                    (
                        tooltip,
                        Some(action.boxed_clone()),
                        Some(SharedString::new_static("ドラッグで別のドックへ移動")),
                    )
                };

                let focus_handle = dock.focus_handle(cx);
                let dock_for_click = dock_entity.clone();
                let workspace_for_click = workspace.clone();
                let icon_label = entry.panel.icon_label(window, cx);
                let panel_for_drag = panel.clone();

                Some(
                    right_click_menu(name)
                        .menu(move |window, cx| {
                            const POSITIONS: [DockPosition; 3] = [
                                DockPosition::Left,
                                DockPosition::Right,
                                DockPosition::Bottom,
                            ];

                            let panel_hide = panel.hide_button_setting(cx);
                            ContextMenu::build(window, cx, |mut menu, _, cx| {
                                let mut has_position_entries = false;
                                for position in POSITIONS {
                                    if panel.position_is_valid(position, cx) {
                                        let is_current = position == dock_position;
                                        let panel = panel.clone();
                                        menu = menu.toggleable_entry(
                                            format!("{}にドッキング", position.label()),
                                            is_current,
                                            IconPosition::Start,
                                            None,
                                            move |window, cx| {
                                                if !is_current {
                                                    panel.set_position(position, window, cx);
                                                }
                                            },
                                        );
                                        has_position_entries = true;
                                    }
                                }
                                if supports_flexible {
                                    if has_position_entries {
                                        menu = menu.separator();
                                    }
                                    let panel_for_flex = panel.clone();
                                    let dock_for_flex = dock_for_menu.clone();
                                    let workspace_for_flex = workspace_for_menu.clone();
                                    menu = menu.toggleable_entry(
                                        "幅を可変にする",
                                        currently_flexible,
                                        IconPosition::Start,
                                        None,
                                        move |window, cx| {
                                            if !currently_flexible {
                                                if let Some(ws) = workspace_for_flex.upgrade() {
                                                    ws.update(cx, |workspace, cx| {
                                                        workspace.toggle_dock_panel_flexible_size(
                                                            &dock_for_flex,
                                                            panel_for_flex.as_ref(),
                                                            window,
                                                            cx,
                                                        );
                                                    });
                                                }
                                            }
                                        },
                                    );
                                    let panel_for_fixed = panel.clone();
                                    let dock_for_fixed = dock_for_menu.clone();
                                    let workspace_for_fixed = workspace_for_menu.clone();
                                    menu = menu.toggleable_entry(
                                        "幅を固定する",
                                        !currently_flexible,
                                        IconPosition::Start,
                                        None,
                                        move |window, cx| {
                                            if currently_flexible {
                                                if let Some(ws) = workspace_for_fixed.upgrade() {
                                                    ws.update(cx, |workspace, cx| {
                                                        workspace.toggle_dock_panel_flexible_size(
                                                            &dock_for_fixed,
                                                            panel_for_fixed.as_ref(),
                                                            window,
                                                            cx,
                                                        );
                                                    });
                                                }
                                            }
                                        },
                                    );
                                }
                                if is_stacked_button {
                                    let dock_for_unstack = dock_for_menu.clone();
                                    let workspace_for_unstack = workspace_for_menu.clone();
                                    menu = menu.separator().entry(
                                        "並べ表示をやめる",
                                        None,
                                        move |window, cx| {
                                            dock_for_unstack.update(cx, |dock, cx| {
                                                dock.set_secondary_panel(None, window, cx);
                                            });
                                            workspace_for_unstack
                                                .update(cx, |workspace, cx| {
                                                    if settles_default_layout {
                                                        workspace
                                                            .settle_default_dock_layout(window, cx);
                                                    }
                                                    workspace.serialize_workspace(window, cx);
                                                })
                                                .ok();
                                        },
                                    );
                                } else if can_stack {
                                    let dock_for_stack = dock_for_menu.clone();
                                    let workspace_for_stack = workspace_for_menu.clone();
                                    menu = menu.separator().entry(
                                        stack_entry_label,
                                        None,
                                        move |window, cx| {
                                            dock_for_stack.update(cx, |dock, cx| {
                                                dock.set_secondary_panel(Some(i), window, cx);
                                                dock.set_open(true, window, cx);
                                            });
                                            workspace_for_stack
                                                .update(cx, |workspace, cx| {
                                                    if settles_default_layout {
                                                        workspace
                                                            .settle_default_dock_layout(window, cx);
                                                    }
                                                    workspace.serialize_workspace(window, cx);
                                                })
                                                .ok();
                                        },
                                    );
                                }
                                {
                                    let panel_for_window = panel.clone();
                                    let workspace_for_window = workspace_for_menu.clone();
                                    menu = menu.separator().entry(
                                        "新しいウィンドウへ切り出す",
                                        None,
                                        move |window, cx| {
                                            let panel = panel_for_window.clone();
                                            workspace_for_window
                                                .update(cx, |workspace, cx| {
                                                    workspace.move_panel_to_new_window(
                                                        panel, None, window, cx,
                                                    );
                                                })
                                                .ok();
                                        },
                                    );
                                }
                                if let Some(hide) = panel_hide {
                                    menu = crate::status_bar::add_hide_button_entry(
                                        menu.separator(),
                                        hide,
                                    );
                                }
                                menu
                            })
                        })
                        .anchor(menu_anchor)
                        .attach(menu_attach)
                        .trigger(move |is_active, _window, _cx| {
                            // Include active state in element ID to invalidate the cached
                            // tooltip when panel state changes (e.g., via keyboard shortcut).
                            // The stacking states get their own bits because they change
                            // what the tooltip says without changing `shown`.
                            let shown = is_active_button || is_stacked_button;
                            let button_state = (shown as u64)
                                | ((shows_in_pair as u64) << 1)
                                | ((is_restorable as u64) << 2);
                            let button = IconButton::new((name, button_state), icon)
                                .icon_size(IconSize::Small)
                                .toggle_state(shown)
                                .tab_index(0isize)
                                .aria_label(icon_tooltip)
                                .on_click({
                                    let action = action.boxed_clone();
                                    move |event: &ClickEvent, window, cx| {
                                        window.focus(&focus_handle, cx);
                                        // Shift stacks the panel next to the one
                                        // the dock shows; a plain click on either
                                        // half of a stacked pair hides or restores
                                        // just that half. Anything else keeps the
                                        // panel's own toggle action.
                                        let handled = dock_for_click.update(cx, |dock, cx| {
                                            if event.modifiers().shift {
                                                dock.stack_panel_beside_primary(i, window, cx)
                                            } else {
                                                dock.hide_stacked_panel(i, window, cx)
                                                    || dock.restack_hidden_panel(i, window, cx)
                                            }
                                        });
                                        if handled {
                                            workspace_for_click
                                                .update(cx, |workspace, cx| {
                                                    if settles_default_layout {
                                                        workspace
                                                            .settle_default_dock_layout(window, cx);
                                                    }
                                                    workspace.serialize_workspace(window, cx);
                                                })
                                                .ok();
                                        } else {
                                            window.dispatch_action(action.boxed_clone(), cx)
                                        }
                                    }
                                })
                                .when(!is_active, |this| {
                                    this.tooltip(move |_window, cx| {
                                        match (&tooltip_meta, &tooltip_action) {
                                            (Some(meta), action) => Tooltip::with_meta(
                                                tooltip_title.clone(),
                                                action.as_deref(),
                                                meta.clone(),
                                                cx,
                                            ),
                                            (None, Some(action)) => Tooltip::for_action(
                                                tooltip_title.clone(),
                                                action.as_ref(),
                                                cx,
                                            ),
                                            (None, None) => {
                                                Tooltip::simple(tooltip_title.clone(), cx)
                                            }
                                        }
                                    })
                                });

                            div()
                                .id(SharedString::from(format!("{name}-drag")))
                                .relative()
                                // Dragging the button moves the whole panel: the
                                // workspace draws the docks it can land on and
                                // tears it off into its own window when the drag
                                // ends outside this one (ADR-F40).
                                .on_drag(
                                    DraggedPanel {
                                        panel: panel_for_drag.clone(),
                                        icon,
                                        label: icon_tooltip.into(),
                                    },
                                    |dragged, _, _, cx| cx.new(|_| dragged.clone()),
                                )
                                .child(button)
                                .when_some(
                                    icon_label
                                        .clone()
                                        .filter(|_| !is_active_button)
                                        .and_then(|label| label.parse::<usize>().ok()),
                                    |this, count| this.child(CountBadge::new(count)),
                                )
                        }),
                )
            })
            .collect();

        if dock_position == DockPosition::Right {
            buttons.reverse();
        }

        let has_buttons = !buttons.is_empty();

        h_flex()
            .gap_1()
            .when(
                has_buttons
                    && (dock.position == DockPosition::Bottom
                        || dock.position == DockPosition::Right),
                |this| this.child(Divider::vertical().color(DividerColor::Border)),
            )
            .children(buttons)
            .when(has_buttons && dock.position == DockPosition::Left, |this| {
                this.child(Divider::vertical().color(DividerColor::Border))
            })
    }
}

impl StatusItemView for PanelButtons {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn crate::ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        // Nothing to do, panel buttons don't depend on the active center item
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        // Panel buttons are hidden on a per-panel basis through each panel
        // button's own context menu.
        None
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod test {
    use super::*;
    use gpui::{App, Context, Window, actions, div};

    pub struct TestPanel {
        pub position: DockPosition,
        pub zoomed: bool,
        pub active: bool,
        pub focus_handle: FocusHandle,
        pub activation_focus_handle: Option<FocusHandle>,
        pub default_size: Pixels,
        pub flexible: bool,
        pub starts_open: bool,
        pub activation_priority: u32,
        /// Every value `set_active` received, in order.
        pub set_active_calls: Vec<bool>,
        /// How often the dock said the user asked for this panel by name.
        pub reveal_calls: usize,
    }
    actions!(test_only, [ToggleTestPanel]);

    impl EventEmitter<PanelEvent> for TestPanel {}

    impl TestPanel {
        pub fn new(position: DockPosition, activation_priority: u32, cx: &mut App) -> Self {
            Self {
                position,
                zoomed: false,
                active: false,
                focus_handle: cx.focus_handle(),
                activation_focus_handle: None,
                default_size: px(300.),
                flexible: false,
                starts_open: false,
                activation_priority,
                set_active_calls: Vec::new(),
                reveal_calls: 0,
            }
        }

        pub fn new_starts_open(
            position: DockPosition,
            activation_priority: u32,
            cx: &mut App,
        ) -> Self {
            Self {
                starts_open: true,
                ..Self::new(position, activation_priority, cx)
            }
        }

        pub fn new_flexible(
            position: DockPosition,
            activation_priority: u32,
            cx: &mut App,
        ) -> Self {
            Self {
                flexible: true,
                ..Self::new(position, activation_priority, cx)
            }
        }

        pub fn new_with_activation_child(
            position: DockPosition,
            activation_priority: u32,
            cx: &mut App,
        ) -> Self {
            Self {
                activation_focus_handle: Some(cx.focus_handle()),
                ..Self::new(position, activation_priority, cx)
            }
        }
    }

    impl Render for TestPanel {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .id("test")
                .track_focus(&self.focus_handle(cx))
                .children(self.activation_focus_handle.iter().map(|focus_handle| {
                    div().id("test-activation-child").track_focus(focus_handle)
                }))
        }
    }

    impl Panel for TestPanel {
        fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
            self.activation_focus_handle
                .clone()
                .unwrap_or_else(|| self.focus_handle(cx))
        }

        fn persistent_name() -> &'static str {
            "TestPanel"
        }

        fn panel_key() -> &'static str {
            "TestPanel"
        }

        fn position(&self, _window: &Window, _: &App) -> super::DockPosition {
            self.position
        }

        fn position_is_valid(&self, _: super::DockPosition) -> bool {
            true
        }

        fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
            self.position = position;
            cx.update_global::<SettingsStore, _>(|_, _| {});
        }

        fn default_size(&self, _window: &Window, _: &App) -> Pixels {
            self.default_size
        }

        fn initial_size_state(&self, _window: &Window, _: &App) -> PanelSizeState {
            PanelSizeState {
                size: None,
                flex: None,
            }
        }

        fn supports_flexible_size(&self) -> bool {
            self.flexible
        }

        fn has_flexible_size(&self, _window: &Window, _: &App) -> bool {
            self.flexible
        }

        fn set_flexible_size(
            &mut self,
            flexible: bool,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) {
            self.flexible = flexible;
        }

        fn icon(&self, _window: &Window, _: &App) -> Option<ui::IconName> {
            None
        }

        fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
            None
        }

        fn toggle_action(&self) -> Box<dyn Action> {
            ToggleTestPanel.boxed_clone()
        }

        fn is_zoomed(&self, _window: &Window, _: &App) -> bool {
            self.zoomed
        }

        fn starts_open(&self, _window: &Window, _: &App) -> bool {
            self.starts_open
        }

        fn set_zoomed(&mut self, zoomed: bool, _window: &mut Window, _cx: &mut Context<Self>) {
            self.zoomed = zoomed;
        }

        fn set_active(&mut self, active: bool, _window: &mut Window, _cx: &mut Context<Self>) {
            self.active = active;
            self.set_active_calls.push(active);
        }

        fn revealed_by_user(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
            self.reveal_calls += 1;
        }

        fn activation_priority(&self) -> u32 {
            self.activation_priority
        }
    }

    impl Focusable for TestPanel {
        fn focus_handle(&self, _cx: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }

    /// Stand-ins for real panels. The saved dock state and the fork's default
    /// right dock layout resolve panels through [`Panel::persistent_name`], so
    /// only a panel carrying the real name exercises them.
    macro_rules! named_test_panel {
        ($type_name:ident, $panel_name:literal) => {
            pub struct $type_name {
                pub panel: TestPanel,
            }

            impl $type_name {
                pub fn new(position: DockPosition, activation_priority: u32, cx: &mut App) -> Self {
                    Self {
                        panel: TestPanel::new(position, activation_priority, cx),
                    }
                }

                pub fn new_starts_open(
                    position: DockPosition,
                    activation_priority: u32,
                    cx: &mut App,
                ) -> Self {
                    Self {
                        panel: TestPanel::new_starts_open(position, activation_priority, cx),
                    }
                }
            }

            impl EventEmitter<PanelEvent> for $type_name {}

            impl Focusable for $type_name {
                fn focus_handle(&self, cx: &App) -> FocusHandle {
                    self.panel.focus_handle(cx)
                }
            }

            impl Render for $type_name {
                fn render(
                    &mut self,
                    _window: &mut Window,
                    cx: &mut Context<Self>,
                ) -> impl IntoElement {
                    div().id($panel_name).track_focus(&self.focus_handle(cx))
                }
            }

            impl Panel for $type_name {
                fn persistent_name() -> &'static str {
                    $panel_name
                }

                fn panel_key() -> &'static str {
                    $panel_name
                }

                fn position(&self, _window: &Window, _: &App) -> DockPosition {
                    self.panel.position
                }

                fn position_is_valid(&self, _: DockPosition) -> bool {
                    true
                }

                fn set_position(
                    &mut self,
                    position: DockPosition,
                    _: &mut Window,
                    cx: &mut Context<Self>,
                ) {
                    self.panel.position = position;
                    cx.update_global::<SettingsStore, _>(|_, _| {});
                }

                fn default_size(&self, _window: &Window, _: &App) -> Pixels {
                    self.panel.default_size
                }

                fn icon(&self, _window: &Window, _: &App) -> Option<ui::IconName> {
                    None
                }

                fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
                    None
                }

                fn toggle_action(&self) -> Box<dyn Action> {
                    ToggleTestPanel.boxed_clone()
                }

                fn is_zoomed(&self, _window: &Window, _: &App) -> bool {
                    self.panel.zoomed
                }

                fn set_zoomed(
                    &mut self,
                    zoomed: bool,
                    _window: &mut Window,
                    _cx: &mut Context<Self>,
                ) {
                    self.panel.zoomed = zoomed;
                }

                fn starts_open(&self, _window: &Window, _: &App) -> bool {
                    self.panel.starts_open
                }

                fn set_active(
                    &mut self,
                    active: bool,
                    _window: &mut Window,
                    _cx: &mut Context<Self>,
                ) {
                    self.panel.active = active;
                    self.panel.set_active_calls.push(active);
                }

                fn revealed_by_user(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
                    self.panel.reveal_calls += 1;
                }

                fn activation_priority(&self) -> u32 {
                    self.panel.activation_priority
                }
            }
        };
    }

    named_test_panel!(TestAgentPanel, "AgentPanel");
    named_test_panel!(TestBrowserPanel, "BrowserPanel");
    named_test_panel!(TestTerminalPanel, "TerminalPanel");
    named_test_panel!(TestDebugPanel, "DebugPanel");
    named_test_panel!(TestGitPanel, "GitPanel");
    named_test_panel!(TestProjectPanel, "Project Panel");
}
