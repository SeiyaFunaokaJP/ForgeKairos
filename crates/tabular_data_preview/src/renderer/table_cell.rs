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

/// How far a cell's text sits from the cell's outside on the left and right:
/// the border and the padding [`style_as_cell`] gives it.
pub(crate) fn cell_text_inset(window: &Window) -> Pixels {
    CELL_FRAME_HEIGHT / 2. + window.rem_size() * 0.25
}

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
        let is_active = self.draws_active_cell_border(cell);
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
    use gpui::{
        Bounds, Modifiers, MouseButton, Pixels, TestAppContext, VisualTestContext, point, px,
    };
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

    /// The editor that opens over a cell has to start on it exactly and be at
    /// least as wide as it is. A rectangle off by the cell's border, or a font a
    /// pixel off the cell's, moves the text the user was looking at the moment
    /// the cell opens, which reads as editing something other than that cell.
    ///
    /// The editor is allowed to be the wider of the two, because it grows to
    /// the right over a value that fills its column (see
    /// `a_value_that_fills_its_column_does_not_slide_when_opened`), and the
    /// taller, because a cell's value can need more lines than its row shows.
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
        assert!(
            editor.size.width >= cell.size.width,
            "{opened_by}: the editor should not leave part of the cell showing \
             ({editor:?} over {cell:?})"
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

    /// Where the caret of the cell editor is, as a byte offset into its text.
    fn caret_offset(
        pane: &gpui::Entity<TabularDataPreviewPane>,
        cx: &mut VisualTestContext,
    ) -> usize {
        pane.update(cx, |pane, cx| {
            pane.grid.cell_editor.update(cx, |editor, cx| {
                let snapshot = editor.display_snapshot(cx);
                editor
                    .selections
                    .newest::<editor::MultiBufferOffset>(&snapshot)
                    .head()
                    .0
            })
        })
    }

    fn double_click(
        pane: &gpui::Entity<TabularDataPreviewPane>,
        cell: GridCell,
        position: gpui::Point<Pixels>,
        cx: &mut VisualTestContext,
    ) {
        pane.update_in(cx, |pane, window, cx| {
            pane.on_cell_mouse_down(
                cell,
                MouseButton::Left,
                Modifiers::none(),
                2,
                position,
                window,
                cx,
            );
        });
        cx.run_until_parked();
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

    /// The column is sized to its longest value, so that value leaves less than
    /// the editor's em of caret room at the right. An editor held to the cell's
    /// width scrolled the value left as it opened, cutting off its first
    /// letter; it has to grow to the right instead and keep the text in place.
    #[gpui::test]
    async fn a_value_that_fills_its_column_does_not_slide_when_opened(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_the_first_body_cell(cx).await;
        for multiline in [false, true] {
            pane.update(&mut cx, |pane, cx| {
                pane.settings.multiline_cells_enabled = multiline;
                cx.notify();
            });
            cx.run_until_parked();

            cx.simulate_keystrokes("f2");
            cx.run_until_parked();
            let opened_by = if multiline {
                "F2 with multiline cells"
            } else {
                "F2 with cells clamped to one line"
            };
            let (editor, cell) = assert_the_editor_starts_on_the_active_cell(&mut cx, opened_by);
            assert!(
                editor.size.width > cell.size.width,
                "{opened_by}: the editor should have grown past a cell its value fills \
                 ({editor:?} over {cell:?})"
            );
            let (scroll, lines) = pane.update(&mut cx, |pane, cx| {
                pane.grid.cell_editor.update(cx, |editor, cx| {
                    let scroll = editor.scroll_position(cx);
                    let lines = editor.display_snapshot(cx).max_point().row().0 + 1;
                    (scroll, lines)
                })
            });
            assert_eq!(
                scroll.x, 0.,
                "{opened_by}: the value should not have scrolled"
            );
            assert_eq!(lines, 1, "{opened_by}: the value should not have folded");

            cx.simulate_keystrokes("escape");
            cx.run_until_parked();
        }
    }

    /// A double click opens the cell with the caret under the pointer, as a
    /// spreadsheet does, rather than always at the end of the value.
    #[gpui::test]
    async fn a_double_click_puts_the_caret_under_the_pointer(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_the_first_body_cell(cx).await;
        let clicked = GridCell::new(GridRow::Body(DisplayRow(0)), AnyColumn(0));
        let cell = cx
            .debug_bounds(CELL_FRAME_DEBUG_SELECTOR)
            .expect("the caret's cell should have been drawn");
        let text = cx
            .debug_bounds(CELL_TEXT_DEBUG_SELECTOR)
            .expect("the caret's cell should have drawn its text");

        double_click(
            &pane,
            clicked,
            point(text.left() + px(1.), cell.center().y),
            &mut cx,
        );
        assert_eq!(
            caret_offset(&pane, &mut cx),
            0,
            "a double click on the first letter should put the caret before it"
        );
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        double_click(&pane, clicked, cell.center(), &mut cx);
        let middle = caret_offset(&pane, &mut cx);
        assert!(
            middle > 0 && middle < "alphabetical".len(),
            "a double click in the middle of the value should put the caret inside it, \
             not at {middle}"
        );
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

/// The line drawn around each selected block, which is measured against the
/// cells it is supposed to trace.
#[cfg(test)]
mod band_7a {
    use gpui::{Bounds, Pixels, TestAppContext, VisualTestContext, px};

    use super::{CELL_FRAME_DEBUG_SELECTOR, TabularDataPreviewPane};
    use crate::{
        grid::{GridCell, GridRow, SelectionChange},
        grid_editing::tests::open_grid,
        grid_fill::{SELECTION_OUTLINE_DEBUG_SELECTORS, SELECTION_OUTLINE_WIDTH},
        types::{AnyColumn, DisplayRow},
    };

    const FIXTURE: &str = "name,value,note\nalphabetical,1,x\nbetabetical,2,y";

    fn body_cell(row: usize, col: usize) -> GridCell {
        GridCell::new(GridRow::Body(DisplayRow(row)), AnyColumn(col))
    }

    /// The four sides of one block, in [`SELECTION_OUTLINE_DEBUG_SELECTORS`]'s
    /// own order: top, bottom, left, right.
    fn outline(cx: &mut VisualTestContext, block: usize) -> [Option<Bounds<Pixels>>; 4] {
        let names = SELECTION_OUTLINE_DEBUG_SELECTORS
            .get(block)
            .expect("the test only names blocks the grid names too");
        [
            cx.debug_bounds(names[0]),
            cx.debug_bounds(names[1]),
            cx.debug_bounds(names[2]),
            cx.debug_bounds(names[3]),
        ]
    }

    fn drawn_sides(cx: &mut VisualTestContext, block: usize) -> usize {
        outline(cx, block)
            .iter()
            .filter(|side| side.is_some())
            .count()
    }

    #[track_caller]
    fn side(sides: [Option<Bounds<Pixels>>; 4], index: usize, name: &str) -> Bounds<Pixels> {
        sides
            .get(index)
            .copied()
            .flatten()
            .unwrap_or_else(|| panic!("the block's {name} should have been drawn"))
    }

    fn select(
        pane: &gpui::Entity<TabularDataPreviewPane>,
        from: GridCell,
        to: GridCell,
        cx: &mut VisualTestContext,
    ) {
        pane.update(cx, |pane, cx| {
            pane.change_cell_selection(from, SelectionChange::Replace, cx);
            pane.change_cell_selection(to, SelectionChange::Extend, cx);
        });
        cx.run_until_parked();
    }

    /// The four sides have to land on the block's own outer edges. A line drawn
    /// a cell short, or one cell's border repeated on every cell of the block,
    /// says something different about where the selection ends - which is the
    /// whole reason the line is there.
    ///
    /// The caret's cell is the only one a test can measure, so the block is
    /// selected once from each end: its top and left are read off its first
    /// cell, its bottom and right off its last.
    #[gpui::test]
    async fn the_outline_traces_the_edges_of_the_selected_block(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid(FIXTURE, cx).await;

        select(&pane, body_cell(1, 2), body_cell(0, 1), &mut cx);
        assert_eq!(
            drawn_sides(&mut cx, 0),
            4,
            "a block has four sides, each named once"
        );
        let first_cell = cx
            .debug_bounds(CELL_FRAME_DEBUG_SELECTOR)
            .expect("the caret should be on the block's first cell");
        assert!(
            first_cell.size.width > px(0.) && first_cell.size.height > px(0.),
            "the cell was drawn with no size ({first_cell:?})"
        );
        let sides = outline(&mut cx, 0);
        let top = side(sides, 0, "top");
        let left = side(sides, 2, "left");
        assert_eq!(
            top.origin, first_cell.origin,
            "the top should start at the block's top left corner ({top:?})"
        );
        assert_eq!(top.size.width, first_cell.size.width);
        assert_eq!(top.size.height, SELECTION_OUTLINE_WIDTH);
        assert_eq!(
            left.origin, first_cell.origin,
            "the left should start at the block's top left corner ({left:?})"
        );
        assert_eq!(left.size.height, first_cell.size.height);
        assert_eq!(left.size.width, SELECTION_OUTLINE_WIDTH);

        // The same block with the caret at the other end of it.
        select(&pane, body_cell(0, 1), body_cell(1, 2), &mut cx);
        assert_eq!(drawn_sides(&mut cx, 0), 4);
        let last_cell = cx
            .debug_bounds(CELL_FRAME_DEBUG_SELECTOR)
            .expect("the caret should be on the block's last cell");
        let sides = outline(&mut cx, 0);
        let bottom = side(sides, 1, "bottom");
        let right = side(sides, 3, "right");
        assert_eq!(
            bottom.bottom_right(),
            last_cell.bottom_right(),
            "the bottom should end at the block's bottom right corner ({bottom:?})"
        );
        assert_eq!(bottom.size.width, last_cell.size.width);
        assert_eq!(bottom.size.height, SELECTION_OUTLINE_WIDTH);
        assert_eq!(
            right.bottom_right(),
            last_cell.bottom_right(),
            "the right should end at the block's bottom right corner ({right:?})"
        );
        assert_eq!(right.size.height, last_cell.size.height);
        assert_eq!(right.size.width, SELECTION_OUTLINE_WIDTH);

        // The block reaches over two cells, so its outline is wider and taller
        // than the one cell each side was measured against.
        assert!(
            right.origin.x > top.origin.x + top.size.width
                && bottom.origin.y > left.origin.y + left.size.height,
            "the four sides should stand around the block rather than around one \
             cell of it ({top:?} {left:?} {bottom:?} {right:?})"
        );
    }

    /// One selected cell under the caret already has a border of its own, and a
    /// second line on top of it would make that one cell look thicker than the
    /// caret's border is anywhere else.
    #[gpui::test]
    async fn a_single_cell_under_the_caret_is_left_to_its_own_border(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid(FIXTURE, cx).await;

        select(&pane, body_cell(0, 0), body_cell(0, 0), &mut cx);
        assert_eq!(
            drawn_sides(&mut cx, 0),
            0,
            "the caret's own border is the outline of a block of one cell"
        );

        // Reaching one cell further makes it a block, which is outlined.
        select(&pane, body_cell(0, 0), body_cell(1, 0), &mut cx);
        assert_eq!(drawn_sides(&mut cx, 0), 4);
    }

    /// Every block of a multiple selection is outlined, which is what tells one
    /// block from the next: the faint background behind the cells cannot.
    #[gpui::test]
    async fn every_selected_block_gets_its_own_outline(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid(FIXTURE, cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.change_cell_selection(body_cell(0, 0), SelectionChange::Replace, cx);
            pane.change_cell_selection(body_cell(1, 1), SelectionChange::Add, cx);
            pane.change_cell_selection(body_cell(1, 2), SelectionChange::Extend, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            drawn_sides(&mut cx, 0),
            4,
            "the block the caret left behind is outlined too"
        );
        assert_eq!(drawn_sides(&mut cx, 1), 4, "and so is the current one");

        // The first block is one cell, so its four sides close around that one
        // cell rather than reaching to the other block.
        let first = outline(&mut cx, 0);
        let (top, bottom) = (side(first, 0, "top"), side(first, 1, "bottom"));
        let (left, right) = (side(first, 2, "left"), side(first, 3, "right"));
        assert_eq!(top.origin, left.origin);
        assert_eq!(bottom.bottom_right(), right.bottom_right());
        assert_eq!(top.size.width, bottom.size.width);
        assert_eq!(left.size.height, right.size.height);
        assert!(top.size.width > px(0.) && left.size.height > px(0.));

        let second = outline(&mut cx, 1);
        assert_ne!(
            side(second, 0, "top").origin,
            top.origin,
            "the two blocks should be outlined where they each are"
        );
    }

    /// A selection is counted in the rows the user can see, so under a filter
    /// the outline has to close around the rows on screen rather than reach
    /// down to where the block's last row sits in the file - the hidden row in
    /// between is not drawn at all, and a line measured against the file would
    /// have nothing to land on.
    #[gpui::test]
    async fn the_outline_closes_around_the_rows_a_filter_leaves_on_screen(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("k,v\na,1\nb,2\na,3", cx).await;
        pane.update(&mut cx, |pane, cx| {
            pane.toggle_filter(AnyColumn(0), Some("a".into()), cx);
        });
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| assert_eq!(pane.body_row_count(), 2));

        // The two rows still showing are the file's second and fourth lines.
        select(&pane, body_cell(0, 0), body_cell(1, 1), &mut cx);
        assert_eq!(drawn_sides(&mut cx, 0), 4);

        let last_cell = cx
            .debug_bounds(CELL_FRAME_DEBUG_SELECTOR)
            .expect("the caret should be on the block's last visible cell");
        let sides = outline(&mut cx, 0);
        let top = side(sides, 0, "top");
        let bottom = side(sides, 1, "bottom");
        assert_eq!(bottom.bottom_right().y, last_cell.bottom_right().y);
        let height = bottom.bottom_right().y - top.origin.y;
        assert!(
            height > last_cell.size.height && height < last_cell.size.height * 2.5,
            "the block is the two rows on screen, not the three lines of the \
             file ({height:?} against a row of {:?})",
            last_cell.size.height
        );
    }
}
