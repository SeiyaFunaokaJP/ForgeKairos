use std::time::Instant;

use editor::EditorMode;
use gpui::{AnyElement, MouseButton, Refineable as _, anchored, deferred, point, size};
use ui::{SpinnerLabel, div, prelude::*};
use util::ResultExt as _;

use crate::{
    TabularDataPreviewPane,
    grid::{cell_line_height, cell_text_style, shape_cell_text},
    renderer::table_cell::{
        EDITOR_FRAME_DEBUG_SELECTOR, EDITOR_TEXT_DEBUG_SELECTOR, cell_text_inset, style_as_cell,
    },
};

/// How wide the overlay editor is while it sits idle over the caret's cell: big
/// enough for an input method's candidate window to be anchored there, small
/// enough to show nothing.
const IDLE_EDITOR_SIZE: Pixels = px(1.);
/// A cell narrower than this is still given room to type in.
const MINIMUM_EDITOR_WIDTH: Pixels = px(48.);
/// How wide the editor opens over a file that has no columns yet, where there is
/// no cell to take a width from. Matches the width a new column is given.
const FIRST_CELL_EDITOR_WIDTH: Pixels = px(150.);

impl Render for TabularDataPreviewPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editor_background = cx.theme().colors().editor_background;
        let row_height = cell_line_height(cx);
        if row_height != self.row_height {
            self.row_height = row_height;
            // Font size (rem size, buffer font override, ...) changed since the list was last
            // measured: existing rows and unmeasured-item height hints are now the wrong size.
            // Unlike `reset_with_uniform_height`, this preserves scroll position and keeps each
            // item's prior size as a hint rather than dropping straight to a fresh guess.
            self.list_state.remeasure();
        }
        let i18n_generation = i18n::generation();
        if self.column_widths.append_column_i18n_generation != i18n_generation {
            self.column_widths.append_column_i18n_generation = i18n_generation;
            self.sync_column_widths(cx);
        }
        self.grid.render_generation = self.grid.render_generation.wrapping_add(1);
        self.clamp_cursor();
        self.sync_cell_editor_style(cx);

        let render_prep_start = Instant::now();
        let table_with_settings = v_flex()
            .size_full()
            .bg(editor_background)
            .child(self.render_settings_panel(window, cx))
            .child(
                div()
                    .relative()
                    .flex_grow_1()
                    .w_full()
                    .overflow_hidden()
                    // Clicking anywhere in the grid puts the caret back in the
                    // cell editor, which the table's own focus tracking would
                    // otherwise take away.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _event, window, cx| {
                            this.focus_cell_editor(window, cx);
                        }),
                    )
                    .child(self.render_grid_body(cx))
                    .child(self.bounds_probe(&self.grid.body_bounds.clone(), cx))
                    .child(self.drag_pointer_tracker(cx)),
            );

        self.performance_metrics.timings.insert(
            "render_prep",
            (render_prep_start.elapsed(), std::time::Instant::now()),
        );

        let root = div()
            .key_context("TabularDataGrid")
            .track_focus(&self.focus_handle)
            .relative()
            .w_full()
            .h_full();
        let root = self.register_grid_actions(root, cx);
        let root = self.register_append_actions(root, cx);
        let root = self.register_filter_actions(root, cx);
        let root = self.capture_editor_actions(root, cx);

        let div = root
            // The overlay editor is positioned against this wrapper, and clipped
            // by it so that a cell scrolled sideways out of view cannot paint
            // over the rest of the window. The context menu is deliberately a
            // sibling of it, so that it can still open past the pane's edge.
            .child(
                div()
                    .relative()
                    .size_full()
                    .overflow_hidden()
                    .child(self.bounds_probe(&self.grid.root_bounds.clone(), cx))
                    .child(table_with_settings)
                    .child(self.render_cell_editor_overlay(window, cx)),
            )
            .children(self.grid.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .anchor(gpui::Anchor::TopLeft)
                        .child(menu.clone()),
                )
                .with_priority(3)
            }));

        #[cfg(feature = "dev-tools")]
        let show_perf_metrics_overlay = self.settings.show_perf_metrics_overlay;

        #[cfg(feature = "dev-tools")]
        let div = div.when(show_perf_metrics_overlay, |div| {
            div.child(self.render_performance_metrics_overlay(cx))
        });

        #[cfg(feature = "dev-tools")]
        if !show_perf_metrics_overlay {
            self.performance_metrics.rendered_indices.clear();
        }

        #[cfg(not(feature = "dev-tools"))]
        self.performance_metrics.rendered_indices.clear();

        div
    }
}

impl TabularDataPreviewPane {
    /// Re-applies the buffer font to the overlay editor, so that changing the
    /// font size keeps what is typed the same size as what the cells show.
    ///
    /// The color is the cell's too: left to itself the editor draws in the
    /// editor foreground, which in most themes is dimmer than cell text, and the
    /// value would fade the moment the cell is opened.
    fn sync_cell_editor_style(&self, cx: &mut Context<Self>) {
        let mut style = cell_text_style(self.row_height, cx);
        style.color = Some(
            self.cell_paint(self.overlay_cell(), false, true, cx)
                .text
                .unwrap_or(cx.theme().colors().text),
        );
        self.grid.cell_editor.update(cx, |editor, _cx| {
            editor.set_text_style_refinement(style);
        });
    }

    /// The table, or the message that stands in for it. A reparse in flight
    /// keeps the table it already has rather than blanking the grid.
    fn render_grid_body(&self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(error) = &self.parse_error {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p_4()
                .text_color(cx.theme().status().error)
                .child(error.clone())
                .into_any_element();
        }
        if self.engine.contents.number_of_cols > 0 {
            return self.create_table(&self.column_widths.widths, cx);
        }

        let message = div()
            .flex()
            .items_center()
            .justify_center()
            .h_32()
            .text_ui(cx)
            .font_buffer(cx)
            .text_color(cx.theme().colors().text_muted);
        if self.is_parsing {
            message
                .child(
                    h_flex()
                        .gap_2()
                        .child(SpinnerLabel::new())
                        .child(i18n::t!("Loading…")),
                )
                .into_any_element()
        } else {
            // A file with no columns has no heading to hang the grid's own "+"
            // off, so the way into a table has to be here instead.
            message
                .child(
                    v_flex()
                        .gap_1()
                        .items_center()
                        .child(i18n::t!(
                            "This file is empty. Typing in a cell creates the first row"
                        ))
                        .child(self.render_start_table_button(cx)),
                )
                .into_any_element()
        }
    }

    /// The single editor every cell is typed into, moved over the caret's cell.
    ///
    /// It is mounted here rather than inside a row so that the virtualized list
    /// can never drop the focused element out of the tree, which would leave
    /// the grid unable to receive keys at all.
    fn render_cell_editor_overlay(&self, window: &mut Window, cx: &Context<Self>) -> AnyElement {
        let generation = self.grid.render_generation;
        let root = self.grid.root_bounds.measured(generation);
        let cell = self.grid.active_cell_bounds.measured(generation);
        let editing = self.grid.editing.is_some();
        let colors = cx.theme().colors();

        let placement = root
            .zip(cell)
            .map(|(root, cell)| (cell.origin - root.origin, cell.size))
            // A file with no columns yet draws a message instead of a table, so
            // there is no cell for the editor to sit over. Without a placement
            // it would stay the one pixel it idles at, and typing the first
            // value of a new file would show nothing at all, so it opens where
            // that first cell is about to appear.
            .or_else(|| {
                if self.grid_column_count() > 0 {
                    return None;
                }
                let body = self.grid.body_bounds.measured(generation)?;
                let root = root?;
                Some((
                    body.origin - root.origin,
                    size(FIRST_CELL_EDITOR_WIDTH, self.row_height),
                ))
            });
        let (origin, cell_size) = placement.unwrap_or_else(|| {
            (
                point(px(0.), px(0.)),
                size(IDLE_EDITOR_SIZE, IDLE_EDITOR_SIZE),
            )
        });
        let visible = editing && placement.is_some();
        #[cfg(test)]
        self.grid.cell_editor_was_drawn.set(visible);

        let width = if visible {
            let available = root.map(|root| root.size.width - origin.x);
            self.cell_editor_width(cell_size.width, available, window, cx)
        } else {
            IDLE_EDITOR_SIZE
        };

        let editor = self.grid.cell_editor.clone();
        let placed = div().absolute().left(origin.x).top(origin.y);
        if !visible {
            return placed
                .w(IDLE_EDITOR_SIZE)
                .h(IDLE_EDITOR_SIZE)
                .overflow_hidden()
                .child(editor)
                .into_any_element();
        }

        // The open cell wears the cell's own frame, down to the font, so that
        // the text the user was looking at does not move the moment the cell is
        // opened. `active_cell_bounds` measures the cell's outside, borders
        // included, which is what this rectangle is placed and sized against.
        placed
            .child(
                style_as_cell(div(), self.settings.vertical_alignment, self.row_height, cx)
                    .w(width)
                    .min_h(cell_size.height)
                    .bg(colors.editor_background)
                    .border_color(colors.border_focused)
                    .debug_selector(|| EDITOR_FRAME_DEBUG_SELECTOR.to_owned())
                    // The editor asks for the whole width of what holds it, so
                    // this stand-in for the cell's text element has to take the
                    // frame's width rather than shrink to its contents.
                    .child(
                        div()
                            .flex_1()
                            .debug_selector(|| EDITOR_TEXT_DEBUG_SELECTOR.to_owned())
                            .child(editor),
                    ),
            )
            .into_any_element()
    }

    /// How wide the open cell is drawn.
    ///
    /// An editor keeps room for its caret past the end of the text: it lays its
    /// lines out two ems short of its width, folding them there when it is an
    /// auto height one, and a single line one scrolls so as to leave a further
    /// em free on the right of the caret. Held to the cell's width, a value
    /// that fills its column would slide left (or fold one word early) the
    /// moment the cell opens. So the editor grows to the right over its
    /// neighbours instead, as a spreadsheet's does, by as much as the text
    /// needs and no further than the pane's edge.
    fn cell_editor_width(
        &self,
        cell_width: Pixels,
        available: Option<Pixels>,
        window: &mut Window,
        cx: &Context<Self>,
    ) -> Pixels {
        let cell_width = cell_width.max(MINIMUM_EDITOR_WIDTH);
        let editor = self.grid.cell_editor.read(cx);
        let text = editor.text(cx);
        let inset = cell_text_inset(window);
        let lines = shape_cell_text(&text, self.row_height, window, cx);
        let Some(first_line) = lines.first() else {
            return cell_width;
        };
        let text_width = lines
            .iter()
            .map(|line| line.width())
            .fold(px(0.), Pixels::max);

        let mut style = window.text_style();
        style.refine(&cell_text_style(self.row_height, cx));
        let text_system = window.text_system();
        let font_id = text_system.resolve_font(&style.font());
        let font_size = first_line.font_size();
        let em_width = text_system
            .em_width(font_id, font_size)
            .log_err()
            .unwrap_or(font_size);
        let slack = if matches!(editor.mode(), EditorMode::AutoHeight { .. }) {
            em_width * 2.
        } else {
            let em_advance = text_system
                .em_advance(font_id, font_size)
                .log_err()
                .unwrap_or(font_size);
            em_width * 2. + em_advance
        };
        // A pixel over, so that rounding cannot fold or scroll the last glyph.
        let needed = text_width + slack + inset * 2. + px(1.);
        let needed = available.map_or(needed, |available| needed.min(available));
        cell_width.max(needed)
    }
}
