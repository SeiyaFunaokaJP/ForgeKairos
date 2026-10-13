use editor::markdown_actions::display_width;
use gpui::{AbsoluteLength, App, MouseButton, MouseDownEvent, MouseMoveEvent};
use settings::Settings as _;
use theme_settings::ThemeSettings;
use ui::{
    ActiveTheme as _, AnyElement, ButtonCommon as _, ButtonSize, Clickable, Context, ElementId,
    FluentBuilder as _, IconButton, IconButtonShape, IconName, IconSize, InteractiveElement as _,
    IntoElement as _, ParentElement as _, SharedString, StatefulInteractiveElement as _,
    Styled as _, StyledTypography as _, Tooltip, div, h_flex, px, rems,
    utils::replace_control_characters,
};

use crate::{
    TabularDataPreviewPane,
    grid::{GridRow, column_label},
    renderer::table_header::HEADING_FADE_WIDTH,
    settings::RowIdentifiers,
    types::{AnyColumn, DataRow, DisplayRow, LineNumber, TableCell},
};

pub enum RowIdentDisplayMode {
    /// E.g
    /// ```text
    /// 1
    /// -
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
/// Advance of one character when the buffer font cannot be measured. Column
/// widths are only a starting point that the user can drag.
const FALLBACK_CHARACTER_WIDTH_PER_FONT_SIZE: f32 = 0.6;
const COLUMN_WIDTH_PADDING: f32 = 16.0;
/// The heading's `border_2` on both sides.
const HEADING_BORDER_WIDTH: f32 = 4.0;
/// The heading's `gap_1` between its column letter and its text, in rems.
const HEADING_GAP_REMS: f32 = 0.25;
/// The `px_1` on both sides of the table's header cell around the heading, in
/// rems.
const HEADER_CELL_PADDING_REMS: f32 = 0.5;
const MINIMUM_COLUMN_WIDTH: f32 = 64.0;
const MAXIMUM_COLUMN_WIDTH: f32 = 480.0;

/// How wide a heading has to be to show its column letter and all of its text
/// before the fade under the filter button begins.
fn heading_width(letter_width: f32, text_width: f32, gap: f32, cell_padding: f32) -> f32 {
    let text = if text_width > 0.0 {
        gap + text_width
    } else {
        0.0
    };
    cell_padding + HEADING_BORDER_WIDTH + letter_width + text + f32::from(HEADING_FADE_WIDTH)
}

/// How wide `text` is drawn, from each character's `advance` in the buffer
/// font or, where that cannot be trusted, the width it is `laid_out` at.
///
/// Outside ASCII the buffer font may have no glyph, and on Windows the font
/// then hands back the advance of its missing-glyph box rather than an error,
/// although the character is drawn in a fallback font that is often twice as
/// wide. Laying the character out goes through the same fallbacks as drawing
/// it. Measuring stops past the widest a column starts at, so that a heading
/// of any length costs the same.
fn heading_text_width(
    text: &str,
    advance: impl Fn(char) -> Option<f32>,
    laid_out: impl Fn(char) -> f32,
) -> f32 {
    let mut width = 0.0;
    for character in text.chars() {
        if width > MAXIMUM_COLUMN_WIDTH {
            break;
        }
        width += if character.is_ascii() {
            advance(character).unwrap_or_else(|| laid_out(character))
        } else {
            laid_out(character)
        };
    }
    width
}

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
    /// one actually holds. The heading also has to show its column letter and
    /// keep clear of the filter button its text fades out under, or a column
    /// of short values cuts its own name off.
    pub(crate) fn calculate_data_column_widths(&self, cx: &App) -> Vec<AbsoluteLength> {
        let settings = ThemeSettings::get_global(cx);
        let font_size = settings.buffer_font_size(cx);
        let text_system = cx.text_system();
        let font_id = text_system.resolve_font(&settings.buffer_font);
        let character_width = match text_system.em_advance(font_id, font_size) {
            Ok(width) => f32::from(width),
            Err(error) => {
                log::error!(
                    "failed to measure a character of {:?} for the grid's columns: {error}",
                    settings.buffer_font.family
                );
                f32::from(font_size) * FALLBACK_CHARACTER_WIDTH_PER_FONT_SIZE
            }
        };
        let rem_size = settings.ui_font_size(cx);
        let heading_gap = f32::from(rems(HEADING_GAP_REMS).to_pixels(rem_size));
        let header_cell_padding = f32::from(rems(HEADER_CELL_PADDING_REMS).to_pixels(rem_size));
        let has_header_row = self.settings.first_row_is_header;
        // A heading is a single line whose text has to be seen whole, so it is
        // measured glyph by glyph rather than estimated: a proportional buffer
        // font draws most letters wider than its `m` is wide.
        let measure = |text: &str| -> f32 {
            heading_text_width(
                text,
                |character| {
                    text_system
                        .advance(font_id, font_size, character)
                        .ok()
                        .map(|advance| f32::from(advance.width))
                },
                |character| f32::from(text_system.layout_width(font_id, font_size, character)),
            )
        };
        (0..self.engine.contents.number_of_cols)
            .map(|col| {
                let column = AnyColumn(col);
                let widest_line = |cell: &TableCell| {
                    cell.display_value()
                        .map(|value| value.lines().map(display_width).max().unwrap_or(0))
                };
                let widest_value = self
                    .engine
                    .contents
                    .rows
                    .iter()
                    .take(COLUMN_WIDTH_SAMPLE_ROWS)
                    .filter_map(|row| row.get(column))
                    .filter_map(widest_line)
                    .max()
                    .unwrap_or(0);
                let header_text = if has_header_row {
                    self.engine
                        .contents
                        .headers
                        .get(column)
                        .and_then(TableCell::display_value)
                        .map(|text| replace_control_characters(text).into_owned())
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                let values = widest_value as f32 * character_width + COLUMN_WIDTH_PADDING;
                let heading = heading_width(
                    measure(&column_label(col)),
                    measure(&header_text),
                    heading_gap,
                    header_cell_padding,
                );
                let width = values
                    .max(heading)
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
        let has_line_numbers = !self.engine.contents.line_numbers.is_empty();

        h_flex()
            .size_full()
            .items_center()
            .child(
                div()
                    .id(ElementId::Name("tabular-grid-select-all".into()))
                    .flex_1()
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .text_color(cx.theme().colors().text_muted)
                    .font_buffer(cx)
                    .child("◤")
                    .tooltip(Tooltip::text(i18n::t!("Select All Cells")))
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.select_all_cells(cx);
                    })),
            )
            .when(has_line_numbers, |row| {
                // A dedicated toggle (rather than only exposing this in the settings popover) so
                // toggling shows its effect on the row identifier column immediately, instead of
                // being hidden behind the open popover until it's dismissed.
                let (icon, tooltip_text) = match self.settings.numbering_type {
                    RowIdentifiers::SrcLines => (
                        IconName::Code,
                        i18n::t!(
                            "Showing file line numbers.\nClick to show sequential row numbers."
                        ),
                    ),
                    RowIdentifiers::RowNum => (
                        IconName::Hash,
                        i18n::t!(
                            "Showing sequential row numbers.\nClick to show file line numbers."
                        ),
                    ),
                };

                let view = cx.entity();
                row.child(
                    IconButton::new("row-identifier-toggle", icon)
                        .shape(IconButtonShape::Square)
                        .icon_size(IconSize::Small)
                        .size(ButtonSize::Compact)
                        .tooltip(Tooltip::text(tooltip_text))
                        .on_click(move |_event, _window, cx| {
                            view.update(cx, |this, cx| {
                                this.settings.numbering_type = match this.settings.numbering_type {
                                    RowIdentifiers::SrcLines => RowIdentifiers::RowNum,
                                    RowIdentifiers::RowNum => RowIdentifiers::SrcLines,
                                };
                                this.sync_column_widths(cx);
                                cx.notify();
                            });
                        }),
                )
            })
            .into_any_element()
    }

    pub(crate) fn create_row_identifier_cell(
        &self,
        display_row: DisplayRow,
        data_row: DataRow,
        cx: &Context<'_, TabularDataPreviewPane>,
    ) -> Option<AnyElement> {
        let row_identifier: ui::SharedString = match self.settings.numbering_type {
            // Fall back to a sequential row number when there is no source line for this row (e.g.
            // content that did not originate from a text buffer, such as a database result set).
            RowIdentifiers::SrcLines => match self.engine.contents.line_numbers.get(*data_row) {
                Some(line) => line
                    .display_string(if self.settings.multiline_cells_effectively_enabled() {
                        RowIdentDisplayMode::Vertical
                    } else {
                        RowIdentDisplayMode::Horizontal
                    })
                    .into(),
                None => (*display_row + 1).to_string().into(),
            },
            RowIdentifiers::RowNum => (*display_row + 1).to_string().into(),
        };

        let grid_row = GridRow::Body(display_row);
        let selected = self.selection_contains_row(grid_row);
        // Excel paints the row numbers blue while a filter is hiding rows, which
        // is the only standing sign that what the grid shows is not the whole
        // file. Sorting alone leaves them as they are: nothing is missing then.
        let filtered = self.engine.has_any_filters();
        let colors = cx.theme().colors();
        // The "+" on the heading's top edge only shows while the pointer is on
        // the heading, so that the column of numbers stays a column of numbers.
        let group_name = SharedString::from(format!("tabular-grid-row-header-{}", *display_row));

        let value = div()
            .id(ElementId::NamedInteger(
                "tabular-grid-row-header".into(),
                *display_row as u64,
            ))
            .group(group_name.clone())
            .relative()
            .flex()
            .px_1()
            .border_color(colors.border_variant)
            .bg(if selected {
                colors.element_selected
            } else {
                colors.panel_background
            })
            .h_full()
            .text_color(if selected || filtered {
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
                        event.modifiers,
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
                        event.modifiers,
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
            .child(self.insert_row_handle(grid_row, group_name, cx))
            .into_any_element();
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn a_heading_outside_ascii_is_measured_as_it_is_drawn() {
        // The advance the buffer font reports for its missing-glyph box, and
        // the width a fallback font actually draws the character at.
        let missing_glyph_box = 9.0;
        let drawn = 15.0;
        let advance = |_| Some(missing_glyph_box);
        let laid_out = |_| drawn;

        assert_eq!(heading_text_width("ab", advance, laid_out), 18.0);
        assert_eq!(heading_text_width("担当者", advance, laid_out), 45.0);
        assert_eq!(heading_text_width("id担当", advance, laid_out), 48.0);
        assert_eq!(heading_text_width("", advance, laid_out), 0.0);
        assert_eq!(
            heading_text_width("ab", |_| None, laid_out),
            30.0,
            "an ASCII character the font cannot measure is laid out instead"
        );
    }

    #[test]
    fn a_very_long_heading_is_only_measured_up_to_the_widest_column() {
        let measured = Cell::new(0usize);
        let width = heading_text_width(
            &"name".repeat(100_000),
            |_| {
                measured.set(measured.get() + 1);
                Some(8.0)
            },
            |_| 8.0,
        );
        assert!(width > MAXIMUM_COLUMN_WIDTH);
        assert!(
            measured.get() <= (MAXIMUM_COLUMN_WIDTH / 8.0) as usize + 1,
            "measured {} characters",
            measured.get()
        );
    }
}
