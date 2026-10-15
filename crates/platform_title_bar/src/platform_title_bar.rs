pub mod platforms;
mod system_window_tabs;

use gpui::{
    Action, AnyElement, App, Context, Decorations, Div, Entity, Hsla, InteractiveElement,
    IntoElement, MouseButton, ParentElement, Stateful, StatefulInteractiveElement, Styled,
    WeakEntity, Window, WindowButtonLayout, WindowControlArea, div, px,
};
use project::DisableAiSettings;
use settings::Settings;
use smallvec::SmallVec;
use std::mem;
use ui::{
    prelude::*,
    utils::{TRAFFIC_LIGHT_PADDING, platform_title_bar_height},
};
use workspace::{MultiWorkspace, SidebarRenderState, SidebarSide};

use crate::{
    platforms::{platform_linux, platform_windows},
    system_window_tabs::SystemWindowTabs,
};

const DRAG_AREA_GROUP: &str = "platform-title-bar-drag-area";

pub use system_window_tabs::{
    DraggedWindowTab, MergeAllWindows, MoveTabToNewWindow, ShowNextWindowTab, ShowPreviousWindowTab,
};

/// With client-side decorations, the title bar is pulled over the window frame's
/// border to avoid a transparent gap in the rounded corners, and a border in the
/// title bar's own color insets its content back by the same amount. Headers that
/// render window controls in place of the title bar use this to apply the same
/// insets as padding, so the controls stay on the same pixel.
///
/// `pad_bottom` should be false when the header already has a bottom border of
/// its own occupying that space.
pub fn apply_title_bar_insets<E: Styled + FluentBuilder>(
    header: E,
    left_window_controls: bool,
    right_window_controls: bool,
    pad_bottom: bool,
) -> E {
    let inset = theme::CLIENT_SIDE_DECORATION_BORDER;
    header
        .mt(-inset)
        .pt(inset)
        .when(pad_bottom, |header| header.pb(inset))
        .when(left_window_controls, |header| header.pl(inset))
        .when(right_window_controls, |header| header.pr(inset))
}

pub struct PlatformTitleBar {
    id: ElementId,
    platform_style: PlatformStyle,
    children: SmallVec<[AnyElement; 2]>,
    should_move: bool,
    system_window_tabs: Entity<SystemWindowTabs>,
    button_layout: Option<WindowButtonLayout>,
    multi_workspace: Option<WeakEntity<MultiWorkspace>>,
}

impl PlatformTitleBar {
    pub fn new(id: impl Into<ElementId>, cx: &mut Context<Self>) -> Self {
        let platform_style = PlatformStyle::platform();
        let system_window_tabs = cx.new(|_cx| SystemWindowTabs::new());

        Self {
            id: id.into(),
            platform_style,
            children: SmallVec::new(),
            should_move: false,
            system_window_tabs,
            button_layout: None,
            multi_workspace: None,
        }
    }

    pub fn with_multi_workspace(mut self, multi_workspace: WeakEntity<MultiWorkspace>) -> Self {
        self.multi_workspace = Some(multi_workspace);
        self
    }

    pub fn set_multi_workspace(&mut self, multi_workspace: WeakEntity<MultiWorkspace>) {
        self.multi_workspace = Some(multi_workspace);
    }

    pub fn title_bar_color(&self, window: &mut Window, cx: &mut Context<Self>) -> Hsla {
        if cfg!(any(target_os = "linux", target_os = "freebsd")) {
            if window.is_window_active() && !self.should_move {
                cx.theme().colors().title_bar_background
            } else {
                cx.theme().colors().title_bar_inactive_background
            }
        } else {
            cx.theme().colors().title_bar_background
        }
    }

    pub fn set_children<T>(&mut self, children: T)
    where
        T: IntoIterator<Item = AnyElement>,
    {
        self.children = children.into_iter().collect();
    }

    pub fn set_button_layout(&mut self, button_layout: Option<WindowButtonLayout>) {
        self.button_layout = button_layout;
    }

    fn effective_button_layout(
        &self,
        decorations: &Decorations,
        cx: &App,
    ) -> Option<WindowButtonLayout> {
        if self.platform_style == PlatformStyle::Linux
            && matches!(decorations, Decorations::Client { .. })
        {
            self.button_layout.or_else(|| cx.button_layout())
        } else {
            None
        }
    }

    pub fn init(cx: &mut App) {
        SystemWindowTabs::init(cx);
    }

    fn sidebar_render_state(&self, cx: &App) -> SidebarRenderState {
        self.multi_workspace
            .as_ref()
            .and_then(|mw| mw.upgrade())
            .map(|mw| mw.read(cx).sidebar_render_state(cx))
            .unwrap_or_default()
    }

    pub fn is_multi_workspace_enabled(cx: &App) -> bool {
        !DisableAiSettings::get_global(cx).disable_ai
    }
}

/// Renders the platform-appropriate left-side window controls (e.g. Ubuntu/GNOME close button).
///
/// Only relevant on Linux with client-side decorations when the window manager
/// places controls on the left.
pub fn render_left_window_controls(
    button_layout: Option<WindowButtonLayout>,
    close_action: Box<dyn Action>,
    window: &Window,
) -> Option<AnyElement> {
    if PlatformStyle::platform() != PlatformStyle::Linux {
        return None;
    }
    if !matches!(window.window_decorations(), Decorations::Client { .. }) {
        return None;
    }
    let button_layout = button_layout?;
    if button_layout.left[0].is_none() {
        return None;
    }
    Some(
        platform_linux::LinuxWindowControls::new(
            "left-window-controls",
            button_layout.left,
            close_action,
        )
        .into_any_element(),
    )
}

/// Renders the platform-appropriate right-side window controls (close, minimize, maximize).
///
/// Returns `None` on Mac or when the platform doesn't need custom controls
/// (e.g. Linux with server-side decorations).
pub fn render_right_window_controls(
    button_layout: Option<WindowButtonLayout>,
    close_action: Box<dyn Action>,
    window: &Window,
) -> Option<AnyElement> {
    let decorations = window.window_decorations();
    let height = platform_title_bar_height(window);

    match PlatformStyle::platform() {
        PlatformStyle::Linux => {
            if !matches!(decorations, Decorations::Client { .. }) {
                return None;
            }
            let button_layout = button_layout?;
            if button_layout.right[0].is_none() {
                return None;
            }
            Some(
                platform_linux::LinuxWindowControls::new(
                    "right-window-controls",
                    button_layout.right,
                    close_action,
                )
                .into_any_element(),
            )
        }
        PlatformStyle::Windows => {
            Some(platform_windows::WindowsWindowControls::new(height).into_any_element())
        }
        PlatformStyle::Mac => None,
    }
}

impl Render for PlatformTitleBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let supported_controls = window.window_controls();
        let decorations = window.window_decorations();
        let height = platform_title_bar_height(window);
        let titlebar_color = self.title_bar_color(window, cx);
        let close_action = Box::new(workspace::CloseWindow);
        let children = mem::take(&mut self.children);
        let is_fullscreen = window.is_fullscreen() || window.is_simple_fullscreen();
        let can_zoom = self.platform_style == PlatformStyle::Linux
            && supported_controls.maximize
            && window.is_resizable();

        let button_layout = self.effective_button_layout(&decorations, cx);
        let sidebar = self.sidebar_render_state(cx);

        let (round_top_left, round_top_right) = match decorations {
            Decorations::Server => (false, false),
            Decorations::Client { tiling, .. } => (
                !(tiling.top || tiling.left)
                    && !(sidebar.open && sidebar.side == SidebarSide::Left),
                !(tiling.top || tiling.right)
                    && !(sidebar.open && sidebar.side == SidebarSide::Right),
            ),
        };

        let show_left_controls = !(sidebar.open && sidebar.side == SidebarSide::Left);
        let left_controls =
            if is_fullscreen || (self.platform_style == PlatformStyle::Mac && show_left_controls) {
                None
            } else {
                show_left_controls
                    .then(|| {
                        render_left_window_controls(
                            button_layout,
                            close_action.as_ref().boxed_clone(),
                            window,
                        )
                    })
                    .flatten()
            };
        let show_right_controls = !(sidebar.open && sidebar.side == SidebarSide::Right);
        let right_controls = if is_fullscreen {
            None
        } else {
            show_right_controls
                .then(|| {
                    render_right_window_controls(
                        button_layout,
                        close_action.as_ref().boxed_clone(),
                        window,
                    )
                })
                .flatten()
        };
        let has_right_controls = right_controls.is_some();
        let last_child_index = children.len().checked_sub(1);
        let inner_rounding =
            theme::CLIENT_SIDE_DECORATION_ROUNDING - theme::CLIENT_SIDE_DECORATION_BORDER;

        let title_bar = h_flex()
            .id(self.id.clone())
            .window_control_area(WindowControlArea::Drag)
            .w_full()
            .h(height)
            .map(|this| Self::with_window_drag_listeners(this, self.platform_style, can_zoom, cx))
            .map(|this| {
                if is_fullscreen {
                    this.pl_2()
                } else if self.platform_style == PlatformStyle::Mac && show_left_controls {
                    // The traffic lights sit on top of this strip. It stays draggable, but it
                    // blocks the bar's hover so the drag area highlight never shows while the
                    // pointer is on a traffic light.
                    this.child(Self::with_window_drag_listeners(
                        div()
                            .id("traffic-light-spacer")
                            .flex_none()
                            .w(px(TRAFFIC_LIGHT_PADDING))
                            .h_full()
                            .occlude(),
                        self.platform_style,
                        can_zoom,
                        cx,
                    ))
                } else if let Some(controls) = left_controls {
                    this.child(
                        non_draggable_area(controls, titlebar_color)
                            .when(round_top_left, |area| area.rounded_tl(inner_rounding)),
                    )
                } else {
                    this.pl_2()
                }
            })
            .map(|el| match decorations {
                Decorations::Server => el,
                Decorations::Client { .. } => el
                    .when(round_top_right, |el| {
                        el.rounded_tr(theme::CLIENT_SIDE_DECORATION_ROUNDING)
                    })
                    .when(round_top_left, |el| {
                        el.rounded_tl(theme::CLIENT_SIDE_DECORATION_ROUNDING)
                    })
                    // Keep in sync with `apply_title_bar_insets`.
                    .mt(-theme::CLIENT_SIDE_DECORATION_BORDER)
                    .mb(-theme::CLIENT_SIDE_DECORATION_BORDER)
                    .border(theme::CLIENT_SIDE_DECORATION_BORDER)
                    .border_color(titlebar_color),
            })
            .bg(titlebar_color)
            // A fullscreen window can't be moved, so its title bar has no drag area to show.
            .when(!is_fullscreen, |this| {
                let hover_color = drag_area_hover_color(titlebar_color, cx);
                this.group(DRAG_AREA_GROUP)
                    .hover(move |style| style.bg(hover_color).border_color(hover_color))
            })
            .content_stretch()
            .child(
                div()
                    .id(self.id.clone())
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .overflow_x_hidden()
                    .w_full()
                    .children(children.into_iter().enumerate().map(|(index, child)| {
                        non_draggable_area(child, titlebar_color).when(
                            round_top_right
                                && !has_right_controls
                                && Some(index) == last_child_index,
                            |area| area.rounded_tr(inner_rounding),
                        )
                    })),
            )
            .children(right_controls.map(|controls| {
                non_draggable_area(controls, titlebar_color)
                    .when(round_top_right, |area| area.rounded_tr(inner_rounding))
            }))
            .when(
                !is_fullscreen
                    && self.platform_style == PlatformStyle::Linux
                    && matches!(decorations, Decorations::Client { .. })
                    && supported_controls.window_menu,
                |titlebar| {
                    titlebar.on_mouse_down(MouseButton::Right, move |ev, window, _| {
                        window.show_window_menu(ev.position)
                    })
                },
            );

        v_flex()
            .w_full()
            .child(title_bar)
            .child(self.system_window_tabs.clone().into_any_element())
    }
}

impl PlatformTitleBar {
    fn with_window_drag_listeners(
        element: Stateful<Div>,
        platform_style: PlatformStyle,
        can_zoom: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        element
            .on_mouse_down_out(cx.listener(move |this, _ev, _window, _cx| {
                this.should_move = false;
            }))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(move |this, _ev, _window, _cx| {
                    this.should_move = false;
                }),
            )
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _ev, _window, _cx| {
                    this.should_move = true;
                }),
            )
            .on_mouse_move(cx.listener(move |this, _ev, window, _| {
                if this.should_move {
                    this.should_move = false;
                    window.start_window_move();
                }
            }))
            // Note: On Windows the title bar behavior is handled by the platform implementation.
            .when(platform_style == PlatformStyle::Mac, |this| {
                this.on_click(|event, window, _| {
                    if event.click_count() == 2 {
                        window.titlebar_double_click();
                    }
                })
            })
            .when(can_zoom, |this| {
                this.on_click(|event, window, _| {
                    if event.click_count() == 2 {
                        window.zoom_window();
                    }
                })
            })
    }
}

/// Wraps an element of the title bar so it is excluded from the drag area. It occludes the
/// bar's hitbox, so neither the bar's drag listeners, the Windows `HTCAPTION` hit test, nor
/// the bar's hover highlight apply over it, and it masks the highlight with the plain title
/// bar color while the drag area is hovered.
fn non_draggable_area(element: impl IntoElement, titlebar_color: Hsla) -> Div {
    h_flex()
        .h_full()
        .occlude()
        .group_hover(DRAG_AREA_GROUP, move |style| style.bg(titlebar_color))
        .child(element)
}

fn drag_area_hover_color(titlebar_color: Hsla, cx: &App) -> Hsla {
    // Themes often make the ghost hover nearly the title bar color (One Light differs by 3 of
    // 255), which leaves the drag area unmarked, so tint toward the text color instead: it
    // contrasts with the bar in light and dark themes alike.
    titlebar_color.blend(cx.theme().colors().text.opacity(0.1))
}

impl ParentElement for PlatformTitleBar {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements)
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use gpui::{
        AnyWindowHandle, Modifiers, MouseButton, Pixels, Point, TestAppContext, canvas, point,
    };
    use settings::SettingsStore;

    use super::*;

    const CHILD_WIDTH: Pixels = px(40.);
    const PROBE_WIDTH: Pixels = px(10.);
    const HIGHLIGHTED_PROBE_WIDTH: Pixels = px(20.);

    struct TitleBarHost {
        title_bar: Entity<PlatformTitleBar>,
        probe_width: Rc<Cell<Pixels>>,
    }

    impl Render for TitleBarHost {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let probe_width = self.probe_width.clone();
            let child = div()
                .id("interactive-child")
                .w(CHILD_WIDTH)
                .h_full()
                .child(
                    // Only stateful elements apply group hover styles during layout.
                    div()
                        .id("drag-area-highlight-probe")
                        .w(PROBE_WIDTH)
                        .h(px(4.))
                        .group_hover(DRAG_AREA_GROUP, |style| style.w(HIGHLIGHTED_PROBE_WIDTH))
                        .child(canvas(
                            move |bounds, _, _| probe_width.set(bounds.size.width),
                            |_, _, _, _| {},
                        )),
                )
                .into_any_element();
            self.title_bar
                .update(cx, |title_bar, _| title_bar.set_children([child]));
            div().size_full().child(self.title_bar.clone())
        }
    }

    struct Fixture {
        window: AnyWindowHandle,
        title_bar: Entity<PlatformTitleBar>,
        probe_width: Rc<Cell<Pixels>>,
        child_point: Point<Pixels>,
        empty_point: Point<Pixels>,
    }

    fn init_test(cx: &mut TestAppContext) -> Fixture {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let probe_width = Rc::new(Cell::new(px(0.)));
        let window = cx.add_window({
            let probe_width = probe_width.clone();
            move |_, cx| TitleBarHost {
                title_bar: cx.new(|cx| PlatformTitleBar::new("test-title-bar", cx)),
                probe_width,
            }
        });
        let title_bar = window
            .read_with(cx, |host, _| host.title_bar.clone())
            .expect("window should be open");
        let window = AnyWindowHandle::from(window);
        let (child_point, empty_point) = cx
            .update_window(window, |_, window, cx| {
                window.draw(cx).clear(cx);
                let height = platform_title_bar_height(window);
                let middle = height / 2.;
                // The bar has an 8px left padding (or the traffic light strip on macOS)
                // before the first child.
                let child_left = if PlatformStyle::platform() == PlatformStyle::Mac {
                    px(TRAFFIC_LIGHT_PADDING)
                } else {
                    px(8.)
                };
                (
                    point(child_left + CHILD_WIDTH / 2., middle),
                    point(window.viewport_size().width / 2., middle),
                )
            })
            .expect("window should be open");
        Fixture {
            window,
            title_bar,
            probe_width,
            child_point,
            empty_point,
        }
    }

    fn move_mouse(cx: &mut TestAppContext, window: AnyWindowHandle, position: Point<Pixels>) {
        cx.update_window(window, |_, window, cx| {
            window.simulate_mouse_move(position, cx)
        })
        .expect("window should be open");
    }

    fn press_mouse(cx: &mut TestAppContext, window: AnyWindowHandle, position: Point<Pixels>) {
        cx.update_window(window, |_, window, cx| {
            window.dispatch_event(
                gpui::PlatformInput::MouseDown(gpui::MouseDownEvent {
                    button: MouseButton::Left,
                    position,
                    modifiers: Modifiers::default(),
                    click_count: 1,
                    first_mouse: false,
                }),
                cx,
            );
        })
        .expect("window should be open");
    }

    fn release_mouse(cx: &mut TestAppContext, window: AnyWindowHandle, position: Point<Pixels>) {
        cx.update_window(window, |_, window, cx| {
            window.dispatch_event(
                gpui::PlatformInput::MouseUp(gpui::MouseUpEvent {
                    button: MouseButton::Left,
                    position,
                    modifiers: Modifiers::default(),
                    click_count: 1,
                }),
                cx,
            );
        })
        .expect("window should be open");
    }

    #[gpui::test]
    fn drag_area_highlight_only_over_empty_space(cx: &mut TestAppContext) {
        let fixture = init_test(cx);
        assert_eq!(fixture.probe_width.get(), PROBE_WIDTH);

        move_mouse(cx, fixture.window, fixture.empty_point);
        assert_eq!(
            fixture.probe_width.get(),
            HIGHLIGHTED_PROBE_WIDTH,
            "hovering the empty part of the title bar should highlight the drag area"
        );

        move_mouse(cx, fixture.window, fixture.child_point);
        assert_eq!(
            fixture.probe_width.get(),
            PROBE_WIDTH,
            "hovering an interactive child should not highlight the drag area"
        );

        move_mouse(cx, fixture.window, point(px(1.), px(500.)));
        assert_eq!(
            fixture.probe_width.get(),
            PROBE_WIDTH,
            "leaving the title bar should clear the highlight"
        );
    }

    fn painted_background_at(
        cx: &mut TestAppContext,
        window: AnyWindowHandle,
        position: Point<Pixels>,
    ) -> Hsla {
        cx.update_window(window, |_, window, _| {
            let position = position.scale(window.scale_factor());
            window
                .painted_quads()
                .into_iter()
                .filter(|quad| {
                    !quad.background.is_transparent()
                        && quad
                            .bounds
                            .intersect(&quad.content_mask.bounds)
                            .contains(&position)
                })
                .max_by_key(|quad| quad.order)
                .and_then(|quad| quad.background.as_solid())
                .expect("a solid background should be painted at the position")
        })
        .expect("window should be open")
    }

    #[gpui::test]
    fn drag_area_highlight_does_not_paint_over_children(cx: &mut TestAppContext) {
        let fixture = init_test(cx);
        let (titlebar_color, hover_color) = cx
            .update_window(fixture.window, |_, window, cx| {
                let titlebar_color = fixture
                    .title_bar
                    .update(cx, |title_bar, cx| title_bar.title_bar_color(window, cx));
                (titlebar_color, drag_area_hover_color(titlebar_color, cx))
            })
            .expect("window should be open");
        assert_ne!(titlebar_color, hover_color);

        move_mouse(cx, fixture.window, fixture.empty_point);
        assert_eq!(
            painted_background_at(cx, fixture.window, fixture.empty_point),
            hover_color,
            "the hovered drag area should be highlighted"
        );
        assert_eq!(
            painted_background_at(cx, fixture.window, fixture.child_point),
            titlebar_color,
            "the highlight should not cover the bar's children"
        );
    }

    #[gpui::test]
    fn drag_starts_only_where_the_highlight_shows(cx: &mut TestAppContext) {
        let fixture = init_test(cx);
        let should_move = |cx: &mut TestAppContext| {
            fixture
                .title_bar
                .read_with(cx, |title_bar, _| title_bar.should_move)
        };

        move_mouse(cx, fixture.window, fixture.empty_point);
        press_mouse(cx, fixture.window, fixture.empty_point);
        assert!(
            should_move(cx),
            "pressing the empty part of the title bar should arm a window move"
        );
        release_mouse(cx, fixture.window, fixture.empty_point);
        assert!(!should_move(cx));

        move_mouse(cx, fixture.window, fixture.child_point);
        press_mouse(cx, fixture.window, fixture.child_point);
        assert!(
            !should_move(cx),
            "pressing an interactive child should not arm a window move"
        );
    }

    #[gpui::test]
    fn no_drag_area_highlight_in_fullscreen(cx: &mut TestAppContext) {
        let fixture = init_test(cx);
        cx.update_window(fixture.window, |_, window, _| window.toggle_fullscreen())
            .expect("window should be open");
        cx.update_window(fixture.window, |_, window, cx| window.draw(cx).clear(cx))
            .expect("window should be open");

        move_mouse(cx, fixture.window, fixture.empty_point);
        assert_eq!(
            fixture.probe_width.get(),
            PROBE_WIDTH,
            "a fullscreen window can't be dragged, so nothing should be highlighted"
        );
    }
}
