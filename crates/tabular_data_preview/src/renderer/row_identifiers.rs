use editor::markdown_actions::display_width;
use gpui::{AbsoluteLength, MouseButton, MouseDownEvent, MouseMoveEvent};
use ui::{
    ActiveTheme as _, AnyElement, Context, ElementId, InteractiveElement as _, IntoElement as _,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, StyledTypography as _,
    Tooltip, div, px,
};

use crate::{
    TabularDataPreviewPane,
    grid::GridRow,
    settings::RowIdentifiers,
    types::{AnyColumn, DataRow, DisplayRow, LineNumber},
};

pub enum RowIdentDisplayMode {
    /// E.g
    /// ```text
    /// 1
    /// ...
    /// 5
    /// ```
    Vertical,
    /// E.g.
    /// ```text
    /// 1-5
    /// ```
    Horizontal,
}

impl LineNumber {
    pub fn display_string(&self, mode: RowIdentDisplayMode) -> String {
        match *self {
            LineNumber::Line(line) => line.to_string(),
            LineNumber::LineRange(start, end) => match mode {
                RowIdentDisplayMode::Vertical => {
                    if start + 1 == end {
                        format!("{start}\n{end}")
                    } else {
                        format!("{start}\n-\n{end}")
                    }
                }
                RowIdentDisplayMode::Horizontal => {
                    format!("{start}-{end}")
                }
            },
        }
    }
}

/// Rows sampled to size a column. Enough to catch the shape of the data without
/// walking a file of millions of rows on every reparse.
const COLUMN_WIDTH_SAMPLE_ROWS: usize = 200;
/// Rough advance of one character in the buffer font. Column widths are only a
/// starting point that the user can drag, so an estimate is enough.
const COLUMN_WIDTH_PER_CHARACTER: f32 = 8.0;
const COLUMN_WIDTH_PADDING: f32 = 16.0;
const MINIMUM_COLUMN_WIDTH: f32 = 64.0;
const MAXIMUM_COLUMN_WIDTH: f32 = 480.0;

impl TabularDataPreviewPane {
    /// Calculate the optimal width for the row identifier column (line numbers or row numbers).
    ///
    /// This ensures the column is wide enough to display the largest identifier comfortably,
    /// but not wastefully wide for small files.
    pub(crate) fn calculate_row_identifier_column_width(&self) -> f32 {
        match self.settings.numbering_type {
            RowIdentifiers::SrcLines => self.calculate_line_number_width(),
            RowIdentifiers::RowNum => self.calculate_row_number_width(),
        }
    }

    /// Starting widths for the data columns, taken from the widest value each
    /// one actually holds.
    pub(crate) fn calculate_data_column_widths(&self) -> Vec<AbsoluteLength> {
        (0..self.engine.contents.number_of_cols)
            .map(|col| {
                let col = AnyColumn(col);
                let widest = std::iter::once(&self.engine.contents.headers)
                    .chain(
                        self.engine
                            .contents
                            .rows
                            .iter()
                            .take(COLUMN_WIDTH_SAMPLE_ROWS),
                    )
                    .filter_map(|row| row.get(col))
                    .filter_map(|cell| cell.display_value())
                    .map(|value| value.lines().map(display_width).max().unwrap_or(0))
                    .max()
                    .unwrap_or(0);
                let width = (widest as f32 * COLUMN_WIDTH_PER_CHARACTER + COLUMN_WIDTH_PADDING)
                    .clamp(MINIMUM_COLUMN_WIDTH, MAXIMUM_COLUMN_WIDTH);
                AbsoluteLength::Pixels(px(width))
            })
            .collect()
    }

    /// Calculate width needed for line numbers (can be multi-line)
    fn calculate_line_number_width(&self) -> f32 {
        // Find the maximum line number that could be displayed
        let max_line_number = self
            .engine
            .contents
            .line_numbers
            .iter()
            .map(|ln| match ln {
                LineNumber::Line(n) => *n,
                LineNumber::LineRange(_, end) => *end,
            })
            .max()
            .unwrap_or_default();

        let digit_count = if max_line_number == 0 {
            1
        } else {
            (max_line_number as f32).log10().floor() as usize + 1
        };

        let char_width_px = 9.0; // TODO: get real width of the characters
        let base_width = (digit_count as f32) * char_width_px;
        let padding = 20.0;
        let min_width = 60.0;
        (base_width + padding).max(min_width)
    }

    /// Calculate width needed for sequential row numbers
    fn calculate_row_number_width(&self) -> f32 {
        let max_row_number = self.engine.contents.rows.len();

        let digit_count = if max_row_number == 0 {
            1
        } else {
            (max_row_number as f32).log10().floor() as usize + 1
        };

        let char_width_px = 9.0; // TODO: get real width of the characters
        let base_width = (digit_count as f32) * char_width_px;
        let padding = 20.0;
        let min_width = 60.0;
        (base_width + padding).max(min_width)
    }

    /// The corner above the row identifiers, which selects the whole sheet.
    pub(crate) fn create_row_identifier_header(
        &self,
        cx: &mut Context<'_, TabularDataPreviewPane>,
    ) -> AnyElement {
        div()
            .id(ElementId::Name("tabular-grid-select-all".into()))
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .text_color(cx.theme().colors().text_muted)
            .font_buffer(cx)
            .child("◤")
            .tooltip(Tooltip::text("すべてのセルを選択"))
            .on_click(cx.listener(|this, _event, _window, cx| {
                this.select_all_cells(cx);
            }))
            .into_any_element()
    }

    pub(crate) fn create_row_identifier_cell(
        &self,
        display_row: DisplayRow,
        data_row: DataRow,
        cx: &Context<'_, TabularDataPreviewPane>,
    ) -> Option<AnyElement> {
        let row_identifier: ui::SharedString = match self.settings.numbering_type {
            RowIdentifiers::SrcLines => self
                .engine
                .contents
                .line_numbers
                .get(*data_row)?
                .display_string(if self.settings.multiline_cells_effectively_enabled() {
                    RowIdentDisplayMode::Vertical
                } else {
                    RowIdentDisplayMode::Horizontal
                })
                .into(),
            RowIdentifiers::RowNum => (*display_row + 1).to_string().into(),
        };

        let grid_row = GridRow::Body(display_row);
        let selected = self.selection_rect().contains_row(grid_row);
        let colors = cx.theme().colors();

        let value = div()
            .id(ElementId::NamedInteger(
                "tabular-grid-row-header".into(),
                *display_row as u64,
            ))
            .flex()
            .px_1()
            .border_color(colors.border_variant)
            .bg(if selected {
                colors.element_selected
            } else {
                colors.panel_background
            })
            .h_full()
            .text_color(if selected {
                colors.text_accent
            } else {
                colors.text_muted
            })
            .justify_center()
            .items_center()
            .font_buffer(cx)
            .text_ui(cx)
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.on_row_mouse_down(
                        grid_row,
                        MouseButton::Left,
                        event.modifiers.shift,
                        event.position,
                        window,
                        cx,
                    );
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.on_row_mouse_down(
                        grid_row,
                        MouseButton::Right,
                        event.modifiers.shift,
                        event.position,
                        window,
                        cx,
                    );
                }),
            )
            .on_mouse_move(
                cx.listener(move |this, event: &MouseMoveEvent, _window, cx| {
                    if event.pressed_button == Some(MouseButton::Left) {
                        this.on_row_drag(grid_row, cx);
                    }
                }),
            )
            .child(row_identifier)
            .into_any_element();
        Some(value)
    }
}
