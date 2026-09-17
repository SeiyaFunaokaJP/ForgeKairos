//! Table Cell Rendering

use gpui::{
    AnyElement, App, ElementId, MouseButton, MouseDownEvent, MouseMoveEvent, Refineable as _,
    transparent_black,
};
use ui::{SharedString, Tooltip, div, prelude::*};

use crate::{
    TabularDataPreviewPane,
    grid::{GridCell, cell_text_style},
    settings::VerticalAlignment,
};

/// Columns the cell id packs a row index against. Ids are built on every frame
/// for every visible cell, so they are computed rather than formatted.
const CELL_ID_COLUMN_STRIDE: u64 = 1 << 20;

/// How much taller a cell is than the text in it: the border it draws above and
/// below, which [`style_as_cell`] puts on the cell and on the overlay editor
/// alike. A row clamped to one line has to leave room for it, or the cell ends
/// up shorter than the editor that opens over it.
pub(crate) const CELL_FRAME_HEIGHT: Pixels = px(4.);

/// The frame a cell draws around its text: the padding that keeps the text off
/// the grid lines, the border that highlights the caret's cell, the vertical
/// alignment, and the font.
///
/// The overlay editor applies the very same frame to itself, which is what
/// makes opening a cell for editing leave its text exactly where it was, so
/// this must stay the only place any of it is written down.
pub(crate) fn style_as_cell<E: Styled + IntoElement>(
    element: E,
    vertical_alignment: VerticalAlignment,
    line_height: Pixels,
    cx: &App,
) -> E {
    let mut element = element
        .flex()
        .px_1()
        // The border is always two pixels wide so that highlighting the
        // caret's cell does not nudge the text of every neighbour.
        .border_2()
        .map(|this| match vertical_alignment {
            VerticalAlignment::Top => this.items_start(),
            VerticalAlignment::Center => this.items_center(),
        });
    element
        .text_style()
        .refine(&cell_text_style(line_height, cx));
    element
}

impl TabularDataPreviewPane {
    /// Measures where the caret's cell ended up, which is how the overlay
    /// editor knows the rectangle to cover.
    pub(crate) fn active_cell_probe(&self, cx: &Context<Self>) -> AnyElement {
        self.bounds_probe(&self.grid.active_cell_bounds.clone(), cx)
    }

    /// The cell the overlay editor has to cover: the one being edited while an
    /// edit is open (the caret cannot move away from it), the caret's otherwise.
    pub(crate) fn overlay_cell(&self) -> GridCell {
        self.grid.editing_cell.unwrap_or(self.grid.cursor)
    }

    /// Create a table cell that takes part in the grid's selection.
    pub(crate) fn create_selectable_cell(
        &self,
        cell: GridCell,
        cell_content: SharedString,
        vertical_alignment: VerticalAlignment,
        cx: &Context<TabularDataPreviewPane>,
    ) -> AnyElement {
        let is_active = self.grid.cursor == cell && self.grid.context_menu.is_none();
        let needs_probe = self.overlay_cell() == cell;
        let is_selected = self.selection_contains(cell.row, cell.col);
        let colors = cx.theme().colors();
        let paint = self.cell_paint(cell, is_selected, is_active, cx);
        // Only a cell that is clipped has anything left to show on hover; a
        // spreadsheet does not put a tooltip on every value.
        let is_clipped =
            !self.settings.multiline_cells_effectively_enabled() && cell_content.contains('\n');

        let framed_cell = style_as_cell(
            div().id(ElementId::NamedInteger(
                SharedString::new_static("tabular-grid-cell"),
                cell.row.index() as u64 * CELL_ID_COLUMN_STRIDE + cell.col.get() as u64,
            )),
            vertical_alignment,
            self.row_height,
            cx,
        )
        .size_full()
        .when(needs_probe, |this| {
            this.debug_selector(|| CELL_FRAME_DEBUG_SELECTOR.to_owned())
        })
        .border_color(if is_active {
            colors.border_focused
        } else {
            self.search_range_border(cell, cx)
                .unwrap_or_else(transparent_black)
        })
        .when_some(paint.background, |this, background| this.bg(background))
        .when_some(paint.text, |this, color| this.text_color(color))
        .when(is_clipped, |this| {
            this.tooltip(Tooltip::text(cell_content.clone()))
        })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                this.on_cell_mouse_down(
                    cell,
                    MouseButton::Left,
                    event.modifiers,
                    event.click_count,
                    event.position,
                    window,
                    cx,
                );
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                this.on_cell_mouse_down(
                    cell,
                    MouseButton::Right,
                    event.modifiers,
                    event.click_count,
                    event.position,
                    window,
                    cx,
                );
            }),
        )
        .on_mouse_move(
            cx.listener(move |this, event: &MouseMoveEvent, _window, cx| {
                if event.pressed_button == Some(MouseButton::Left) {
                    this.on_cell_drag(cell, cx);
                }
            }),
        )
        .child(
            div()
                .when(needs_probe, |this| {
                    this.debug_selector(|| CELL_TEXT_DEBUG_SELECTOR.to_owned())
                })
                .child(cell_content),
        );

        // The probe hangs off an undecorated wrapper rather than off the cell
        // itself: an absolutely positioned child is laid out against its
        // parent's padding box, so a probe inside the cell would report a
        // rectangle two pixels smaller on every side than the cell it stands
        // for, and the overlay editor built from it would sit that far off.
        // The wrapper has to be a flex container: an absolutely positioned
        // child of a block container is laid out in flow, which would put the
        // probe below the cell instead of over it.
        //
        // The fill handle and the bands that grab the selection's border share
        // that wrapper, and come after the probe so that their hitboxes are the
        // ones the pointer finds at the cell's edge.
        div()
            .relative()
            .flex()
            .size_full()
            .child(framed_cell)
            .when(needs_probe, |this| this.child(self.active_cell_probe(cx)))
            .children(self.grid_drag_overlays(cell, cx))
            .into_any_element()
    }
}

/// Marks the cell the overlay editor covers, so that a test can check that the
/// editor lands on it exactly. `debug_selector` compiles away outside of tests.
pub(crate) const CELL_FRAME_DEBUG_SELECTOR: &str = "tabular-grid-active-cell";

/// Marks that cell's text, which is the part the user watches for movement.
pub(crate) const CELL_TEXT_DEBUG_SELECTOR: &str = "tabular-grid-active-cell-text";

/// The overlay editor's counterpart of [`CELL_FRAME_DEBUG_SELECTOR`].
pub(crate) const EDITOR_FRAME_DEBUG_SELECTOR: &str = "tabular-grid-cell-editor";

/// The overlay editor's counterpart of [`CELL_TEXT_DEBUG_SELECTOR`]: the
/// element the editor's own text is laid out in.
pub(crate) const EDITOR_TEXT_DEBUG_SELECTOR: &str = "tabular-grid-cell-editor-text";

#[cfg(test)]
mod tests {
    use gpui::{Bounds, Modifiers, MouseButton, Pixels, TestAppContext, VisualTestContext, px};
    use settings::SettingsStore;

    use super::*;
    use crate::{
        grid::GridRow,
        grid_editing::tests::open_grid,
        types::{AnyColumn, DisplayRow},
    };

    /// A file whose columns are wide enough that the editor's own minimum width
    /// never comes into play.
    const FIXTURE: &str = "name,value\nalphabetical,1\nbetabetical,2";

    /// The editor that opens over a cell has to start on it exactly and be as
    /// wide as it is. A rectangle off by the cell's border, or a font a pixel
    /// off the cell's, moves the text the user was looking at the moment the
    /// cell opens, which reads as editing something other than that cell.
    ///
    /// The editor is allowed to be the taller of the two: it keeps an em of
    /// slack at its right edge, so text that fits the cell on one line can wrap
    /// onto a second inside the editor.
    #[track_caller]
    fn assert_the_editor_starts_on_the_active_cell(
        cx: &mut VisualTestContext,
        opened_by: &str,
    ) -> (Bounds<Pixels>, Bounds<Pixels>) {
        let cell = cx
            .debug_bounds(CELL_FRAME_DEBUG_SELECTOR)
            .expect("the caret's cell should have been drawn");
        let editor = cx
            .debug_bounds(EDITOR_FRAME_DEBUG_SELECTOR)
            .expect("the editor should have been drawn over the caret's cell");
        let cell_text = cx
            .debug_bounds(CELL_TEXT_DEBUG_SELECTOR)
            .expect("the caret's cell should have drawn its text");
        let editor_text = cx
            .debug_bounds(EDITOR_TEXT_DEBUG_SELECTOR)
            .expect("the editor should have drawn its text");

        // `debug_bounds` answers for elements of no size too, which would make
        // every comparison below hold for a grid that drew nothing at all.
        assert!(
            cell.size.width > px(0.) && cell.size.height > px(0.),
            "{opened_by}: the cell was drawn with no size ({cell:?})"
        );
        assert!(
            editor_text.size.width > px(0.),
            "{opened_by}: the editor was drawn with no width to type in ({editor_text:?})"
        );
        assert_eq!(
            editor.origin, cell.origin,
            "{opened_by}: the editor should start exactly where the cell does"
        );
        assert_eq!(
            editor.size.width, cell.size.width,
            "{opened_by}: the editor should be exactly as wide as the cell"
        );
        assert!(
            editor.size.height >= cell.size.height,
            "{opened_by}: the editor should not leave part of the cell showing \
             ({editor:?} over {cell:?})"
        );
        assert_eq!(
            editor_text.origin, cell_text.origin,
            "{opened_by}: the first character should not move when the cell opens"
        );
        (editor, cell)
    }

    /// The same, for the cells of one line that the editor covers exactly.
    #[track_caller]
    fn assert_the_editor_covers_the_active_cell(cx: &mut VisualTestContext, opened_by: &str) {
        let (editor, cell) = assert_the_editor_starts_on_the_active_cell(cx, opened_by);
        assert_eq!(
            editor.size.height, cell.size.height,
            "{opened_by}: the editor should be exactly as tall as the cell"
        );
    }

    async fn open_the_first_body_cell(
        cx: &mut TestAppContext,
    ) -> (
        gpui::Entity<TabularDataPreviewPane>,
        gpui::Entity<editor::Editor>,
        VisualTestContext,
    ) {
        let (pane, editor, mut cx) = open_grid(FIXTURE, cx).await;
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        (pane, editor, cx)
    }

    #[gpui::test]
    async fn typing_over_a_cell_opens_the_editor_on_it(cx: &mut TestAppContext) {
        let (_pane, _editor, mut cx) = open_the_first_body_cell(cx).await;

        cx.simulate_input("x");
        cx.run_until_parked();

        assert_the_editor_covers_the_active_cell(&mut cx, "typing a character");
    }

    #[gpui::test]
    async fn f2_opens_the_editor_on_the_cell(cx: &mut TestAppContext) {
        let (_pane, _editor, mut cx) = open_the_first_body_cell(cx).await;

        cx.simulate_keystrokes("f2");
        cx.run_until_parked();

        assert_the_editor_covers_the_active_cell(&mut cx, "F2");
    }

    #[gpui::test]
    async fn a_double_click_opens_the_editor_on_the_cell(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_the_first_body_cell(cx).await;
        let cell = cx
            .debug_bounds(CELL_FRAME_DEBUG_SELECTOR)
            .expect("the caret's cell should have been drawn");

        let clicked = GridCell::new(GridRow::Body(DisplayRow(0)), AnyColumn(0));
        pane.update_in(&mut cx, |pane, window, cx| {
            pane.on_cell_mouse_down(
                clicked,
                MouseButton::Left,
                Modifiers::none(),
                2,
                cell.center(),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        assert_the_editor_covers_the_active_cell(&mut cx, "a double click");
    }

    /// The rows, the cell text and the editor are all sized from the buffer
    /// font, so enlarging it has to grow the rows and leave the editor on the
    /// cell rather than pull the two apart.
    #[gpui::test]
    async fn a_larger_buffer_font_keeps_the_editor_on_the_cell(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_the_first_body_cell(cx).await;
        cx.simulate_input("x");
        cx.run_until_parked();
        assert_the_editor_covers_the_active_cell(&mut cx, "the default buffer font");
        let default_row_height = pane.read_with(&cx, |pane, _| pane.row_height);

        cx.update(|_window, cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |content| {
                    content.theme.buffer_font_size = Some(20.0.into());
                });
            });
        });
        cx.run_until_parked();

        let enlarged_row_height: Pixels = pane.read_with(&cx, |pane, _| pane.row_height);
        assert!(
            enlarged_row_height > default_row_height,
            "a 20 pixel buffer font should make the rows taller than the default \
             ({default_row_height:?} -> {enlarged_row_height:?})"
        );
        assert_the_editor_covers_the_active_cell(&mut cx, "a 20 pixel buffer font");
    }

    /// A quoted line break makes the cell several lines tall and turns the
    /// overlay into an auto height editor, so the two have to agree on a height
    /// as well as on an origin.
    #[gpui::test]
    async fn the_editor_covers_a_cell_of_several_lines(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("name,value\n\"first\nsecond\nthird\",1", cx).await;
        cx.simulate_keystrokes("down");
        cx.run_until_parked();

        cx.simulate_keystrokes("f2");
        cx.run_until_parked();

        let line_height: Pixels = pane.read_with(&cx, |pane, _| pane.row_height);
        let cell = cx
            .debug_bounds(CELL_FRAME_DEBUG_SELECTOR)
            .expect("the caret's cell should have been drawn");
        // Without this the test would pass just as well on a grid that showed
        // only the first line of the cell.
        assert!(
            cell.size.height > line_height * 2.,
            "a cell of three lines should be drawn taller than two lines \
             ({cell:?} against {line_height:?} per line)"
        );
        assert_the_editor_starts_on_the_active_cell(&mut cx, "a cell of three lines");
    }

    /// With multiline cells turned off every row is clamped to one line, and the
    /// editor has to be clamped with it rather than stand taller than the cell
    /// it covers.
    #[gpui::test]
    async fn the_editor_covers_a_cell_clamped_to_one_line(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_the_first_body_cell(cx).await;
        pane.update(&mut cx, |pane, cx| {
            pane.settings.multiline_cells_enabled = false;
            cx.notify();
        });
        cx.run_until_parked();

        cx.simulate_keystrokes("f2");
        cx.run_until_parked();

        assert_the_editor_covers_the_active_cell(&mut cx, "a cell clamped to one line");
    }

    /// The fill handle has to sit in the bottom right corner of the cell the
    /// selection ends on, and be a small square there: one that filled the cell
    /// would swallow every click on it, and one placed against the wrapper's
    /// padding box rather than its edge would float inside the cell.
    #[gpui::test]
    async fn the_fill_handle_sits_in_the_corner_of_the_caret_cell(cx: &mut TestAppContext) {
        let (_pane, _editor, mut cx) = open_the_first_body_cell(cx).await;

        let cell = cx
            .debug_bounds(CELL_FRAME_DEBUG_SELECTOR)
            .expect("the caret's cell should have been drawn");
        let handle = cx
            .debug_bounds(crate::grid_fill::FILL_HANDLE_DEBUG_SELECTOR)
            .expect("the caret's cell should carry the fill handle");
        assert!(
            cell.size.width > px(0.) && cell.size.height > px(0.),
            "the cell was drawn with no size ({cell:?})"
        );
        assert_eq!(
            handle.bottom_right(),
            cell.bottom_right(),
            "the handle should end where the cell does ({handle:?} against {cell:?})"
        );
        assert!(
            handle.size.width < cell.size.width && handle.size.height < cell.size.height,
            "the handle should be a small square in the corner rather than the \
             whole cell ({handle:?} against {cell:?})"
        );

        // Opening the cell for input hides it: the overlay editor covers the
        // corner it would sit in.
        cx.simulate_keystrokes("f2");
        cx.run_until_parked();
        assert_eq!(
            cx.debug_bounds(crate::grid_fill::FILL_HANDLE_DEBUG_SELECTOR),
            None
        );
    }

    /// The fill handle and the bands that grab the selection's border hang off
    /// the same wrapper as the cell and take presses of their own. Away from the
    /// corner and the edges the cell has to keep them, or the selected cell -
    /// the one the pointer is most often on - would stop selecting and opening.
    #[gpui::test]
    async fn a_press_in_the_middle_of_the_caret_cell_still_reaches_it(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_the_first_body_cell(cx).await;
        let cell = cx
            .debug_bounds(CELL_FRAME_DEBUG_SELECTOR)
            .expect("the caret's cell should have been drawn");

        cx.simulate_mouse_down(cell.center(), MouseButton::Left, Modifiers::none());
        cx.run_until_parked();

        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.grid.drag,
                Some(crate::grid::GridDrag::Cells),
                "the middle of the cell still starts an ordinary drag selection"
            );
        });
    }

    /// The heading is a cell too: the caret starts on it, and the editor that
    /// opens over it is placed from the same measurement. Only the rectangle is
    /// shared - the heading packs the column's letter in beside the value, so it
    /// keeps a height of its own.
    #[gpui::test]
    async fn the_editor_covers_a_heading_cell(cx: &mut TestAppContext) {
        let (_pane, _editor, mut cx) = open_grid(FIXTURE, cx).await;
        cx.run_until_parked();

        cx.simulate_keystrokes("f2");
        cx.run_until_parked();

        let heading = cx
            .debug_bounds(CELL_FRAME_DEBUG_SELECTOR)
            .expect("the caret's heading cell should have been drawn");
        let editor: Bounds<Pixels> = cx
            .debug_bounds(EDITOR_FRAME_DEBUG_SELECTOR)
            .expect("the editor should have been drawn over the heading");
        assert!(
            heading.size.width > px(0.) && heading.size.height > px(0.),
            "the heading was drawn with no size ({heading:?})"
        );
        assert_eq!(
            editor.origin, heading.origin,
            "the editor should start exactly where the heading cell does"
        );
        assert_eq!(
            editor.size.width, heading.size.width,
            "the editor should be as wide as the heading cell"
        );
    }
}
