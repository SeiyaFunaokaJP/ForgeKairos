//! Table Cell Rendering

use gpui::{AnyElement, ElementId, MouseButton, MouseDownEvent, MouseMoveEvent, transparent_black};
use ui::{SharedString, Tooltip, div, prelude::*};

use crate::{TabularDataPreviewPane, grid::GridCell, settings::VerticalAlignment};

/// Columns the cell id packs a row index against. Ids are built on every frame
/// for every visible cell, so they are computed rather than formatted.
const CELL_ID_COLUMN_STRIDE: u64 = 1 << 20;

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
        let is_selected = self.selection_rect().contains(cell.row, cell.col);
        let colors = cx.theme().colors();
        // Only a cell that is clipped has anything left to show on hover; a
        // spreadsheet does not put a tooltip on every value.
        let is_clipped =
            !self.settings.multiline_cells_effectively_enabled() && cell_content.contains('\n');

        div()
            .id(ElementId::NamedInteger(
                SharedString::new_static("tabular-grid-cell"),
                cell.row.index() as u64 * CELL_ID_COLUMN_STRIDE + cell.col.get() as u64,
            ))
            .relative()
            .flex()
            .h_full()
            .px_1()
            // The border is always two pixels wide so that highlighting the
            // caret's cell does not nudge the text of every neighbour.
            .border_2()
            .border_color(if is_active {
                colors.border_focused
            } else {
                transparent_black()
            })
            .when(is_selected && !is_active, |this| {
                this.bg(colors.element_selected.opacity(0.5))
            })
            .map(|this| match vertical_alignment {
                VerticalAlignment::Top => this.items_start(),
                VerticalAlignment::Center => this.items_center(),
            })
            .font_buffer(cx)
            .when(is_clipped, |this| {
                this.tooltip(Tooltip::text(cell_content.clone()))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.on_cell_mouse_down(
                        cell,
                        MouseButton::Left,
                        event.modifiers.shift,
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
                        event.modifiers.shift,
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
            .child(div().child(cell_content))
            .when(needs_probe, |this| this.child(self.active_cell_probe(cx)))
            .into_any_element()
    }
}
