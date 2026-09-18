use crate::types::TableCell;
use gpui::{AnyElement, Div, Entity, MouseButton, MouseDownEvent, actions};
use std::ops::Range;
use ui::{
    ColumnWidthConfig, ResizableColumnsState, Table, Tooltip, UncheckedTableRow, div, prelude::*,
};

use crate::{
    TabularDataPreviewPane,
    edit_engine::{RowInsertPosition, RowRef},
    grid::{GridCell, GridRow},
    grid_styles::StyleShift,
    renderer::table_cell::CELL_FRAME_HEIGHT,
    settings::RowRenderMechanism,
    table_data_engine::ColumnShift,
    types::{AnyColumn, DataRow, DisplayRow},
};

actions!(
    tabular_data,
    [
        /// 表の末尾に空の行を 1 行足します。
        AppendRow,
        /// 表の右端に空の列を 1 列足します。
        AppendColumn,
    ]
);

/// The row identifier column, which the table draws before every data column.
pub(crate) const ROW_IDENTIFIER_COLUMNS: usize = 1;

/// The trailing "+" column, which the table draws after every data column.
///
/// It stands for a column that does not exist yet, so everything that maps a
/// table column onto a data column has to leave it out.
pub(crate) const APPEND_COLUMNS: usize = 1;

/// The trailing "+" row, which the list draws after the last body row.
///
/// It is part of the list rather than of the table, so
/// [`TabularDataPreviewPane::body_row_count`] deliberately does not count it:
/// the selection clamps to that count and the search scopes to it, and neither
/// may reach a row the file does not have.
pub(crate) const APPEND_ROWS: usize = 1;

/// How wide the "+" column is. Only as wide as its glyph needs, because it
/// holds no value that could want more and cannot be dragged wider.
pub(crate) const APPEND_COLUMN_WIDTH: f32 = 34.0;

/// How thick the "+" that appears between two headings is. Enough for the
/// glyph, and little enough to leave the heading itself clickable.
const INSERT_HANDLE_THICKNESS: Pixels = px(10.);

/// Marks the trailing "+" row, so that a test can hold the table to offering it.
pub(crate) const APPEND_ROW_DEBUG_SELECTOR: &str = "tabular-grid-append-row";

/// Marks the trailing "+" column's heading.
pub(crate) const APPEND_COLUMN_DEBUG_SELECTOR: &str = "tabular-grid-append-column";

/// Marks the "+" on the top edge of a row heading, which inserts a row there.
pub(crate) const INSERT_ROW_HANDLE_DEBUG_SELECTOR: &str = "tabular-grid-insert-row-handle";

/// Marks the "+" on the left edge of a column heading.
pub(crate) const INSERT_COLUMN_HANDLE_DEBUG_SELECTOR: &str = "tabular-grid-insert-column-handle";

impl TabularDataPreviewPane {
    /// Creates a new table.
    /// Column number is derived from the `ResizableColumnsState` entity.
    pub(crate) fn create_table(
        &self,
        current_widths: &Entity<ResizableColumnsState>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.create_table_inner(self.body_row_count() + APPEND_ROWS, current_widths, cx)
    }

    fn create_table_inner(
        &self,
        row_count: usize,
        current_widths: &Entity<ResizableColumnsState>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cols = current_widths.read(cx).cols();
        // Create headers array with interactive elements
        let mut headers = Vec::with_capacity(cols);

        headers.push(self.create_row_identifier_header(cx));

        // Add the actual data headers with sort buttons
        for i in 0..cols.saturating_sub(ROW_IDENTIFIER_COLUMNS + APPEND_COLUMNS) {
            headers.push(self.create_header_element_with_sort_button(cx, AnyColumn::from(i)));
        }

        // `TableRow` asserts the row's width, and the width config is resynced
        // from the contents a frame after the contents themselves, so the
        // headings are brought to the table's column count either way rather
        // than taken as a panic.
        if headers.len() < cols {
            headers.push(self.create_append_column_header(cx));
        }
        while headers.len() < cols {
            headers.push(div().size_full().into_any_element());
        }
        headers.truncate(cols);

        Table::new(cols)
            .interactable(&self.table_interaction_state)
            .width_config(ColumnWidthConfig::Resizable(current_widths.clone()))
            .header(headers)
            .disable_base_style()
            .pin_cols(1)
            .map(|table| {
                let row_identifier_text_color = cx.theme().colors().editor_line_number;
                match self.settings.rendering_with {
                    RowRenderMechanism::VariableList => {
                        table.variable_row_height_list(row_count, self.list_state.clone(), {
                            cx.processor(move |this, display_row: usize, _window, cx| {
                                this.performance_metrics.rendered_indices.push(display_row);

                                let display_row = DisplayRow(display_row);
                                Self::render_single_table_row(
                                    this,
                                    cols,
                                    display_row,
                                    row_identifier_text_color,
                                    this.row_height,
                                    cx,
                                )
                                .unwrap_or_else(|| Self::render_blank_table_row(cols))
                            })
                        })
                    }
                    RowRenderMechanism::UniformList => {
                        table.uniform_list("tabular-data-table", row_count, {
                            cx.processor(move |this, range: Range<usize>, _window, cx| {
                                // Record all display indices in the range for performance metrics
                                this.performance_metrics
                                    .rendered_indices
                                    .extend(range.clone());

                                let row_height = this.row_height;
                                range
                                    .filter_map(|display_index| {
                                        Self::render_single_table_row(
                                            this,
                                            cols,
                                            DisplayRow(display_index),
                                            row_identifier_text_color,
                                            row_height,
                                            cx,
                                        )
                                    })
                                    .collect()
                            })
                        })
                    }
                }
            })
            .into_any_element()
    }

    /// An empty stand-in for a row the table cannot render.
    ///
    /// The display mapping is recomputed a moment after the contents on a large
    /// file, so a list sized by the old mapping can ask for a row the table no
    /// longer has. The next frame draws the real thing; blanking the row keeps
    /// that from taking the window down with it.
    fn render_blank_table_row(cols: usize) -> UncheckedTableRow<AnyElement> {
        (0..cols)
            .map(|_| div().size_full().into_any_element())
            .collect()
    }

    /// Render a single table row
    ///
    /// Used both by UniformList and VariableRowHeightList
    fn render_single_table_row(
        this: &TabularDataPreviewPane,
        cols: usize,
        display_row: DisplayRow,
        row_identifier_text_color: gpui::Hsla,
        row_height: Pixels,
        cx: &Context<TabularDataPreviewPane>,
    ) -> Option<UncheckedTableRow<AnyElement>> {
        if display_row.get() == this.append_row_index() {
            return Some(this.render_append_row(cols, cx));
        }

        // Get the actual row index from our sorted indices
        let data_row = this.engine.d2d_mapping().get_data_row(display_row)?;
        let row = this.engine.contents.get_row(data_row)?;

        let mut elements = Vec::with_capacity(cols);
        elements.push(this.create_row_identifier_cell(display_row, data_row, cx)?);

        // Remaining columns: actual table data. The column widths are resynced
        // from the contents on every parse, but a frame can still land between
        // the two, so the row is built from what the contents hold and trimmed
        // to the width the table was created with.
        for col in (0..this
            .engine
            .contents
            .number_of_cols
            .min(cols.saturating_sub(ROW_IDENTIFIER_COLUMNS + APPEND_COLUMNS)))
            .map(AnyColumn)
        {
            let Some(table_cell) = row.get(col) else {
                continue;
            };

            // TODO: Introduce `<null>` cell type
            let cell_content = table_cell.display_value().cloned().unwrap_or_default();

            let cell = div()
                .size_full()
                .when(
                    !this.settings.multiline_cells_effectively_enabled(),
                    |div| {
                        // The frame is added on top of the line so that the
                        // clamped cell is exactly as tall as the same cell is
                        // with multiline on, and as the editor that opens over
                        // it: clamping to the bare line height would cut two
                        // pixels off the text at each end.
                        div.whitespace_nowrap()
                            .text_ellipsis()
                            .h(row_height + CELL_FRAME_HEIGHT)
                            .overflow_hidden()
                    },
                )
                .child(this.create_selectable_cell(
                    GridCell::new(GridRow::Body(display_row), col),
                    cell_content,
                    this.settings.vertical_alignment,
                    cx,
                ));

            elements.push(
                div()
                    .size_full()
                    .when(this.settings.show_debug_info, |parent| {
                        parent.child(div().text_color(row_identifier_text_color).child(
                            match table_cell {
                                TableCell::Real { position: pos, .. } => {
                                    let slv = pos.start.timestamp().value;
                                    let so = pos.start.offset;
                                    let elv = pos.end.timestamp().value;
                                    let eo = pos.end.offset;
                                    format!("Pos {so}(L{slv})-{eo}(L{elv})")
                                }
                                TableCell::Virtual => "Virtual cell".into(),
                            },
                        ))
                    })
                    .child(cell)
                    .into_any_element(),
            );
        }

        // `TableRow` asserts the row's width, so a row that came out short of
        // the table's column count is padded rather than taken as a panic.
        while elements.len() < cols {
            elements.push(div().size_full().into_any_element());
        }
        Some(elements)
    }

    ///// The trailing "+" row and column /////

    /// The table's column count: the row identifier column, one column per data
    /// column, and the trailing "+" column.
    pub(crate) fn table_column_count(&self) -> usize {
        ROW_IDENTIFIER_COLUMNS + self.grid_column_count() + APPEND_COLUMNS
    }

    /// The display row the "+ 行を追加" strip is drawn on, which is one past the
    /// last row of the file.
    pub(crate) fn append_row_index(&self) -> usize {
        self.body_row_count()
    }

    /// The strip drawn under the last row, which adds one more row where the
    /// user is already looking instead of sending them to the toolbar.
    fn render_append_row(&self, cols: usize, cx: &Context<Self>) -> UncheckedTableRow<AnyElement> {
        let colors = cx.theme().colors();
        // The same height a body row of one line has, so that the strip reads as
        // the next row of the table rather than as a band under it.
        let height = self.row_height + CELL_FRAME_HEIGHT;
        (0..cols)
            .map(|index| {
                // The cell under the "+" column is where the two strips meet:
                // clicking it would have to add a row and a column at once.
                if cols > ROW_IDENTIFIER_COLUMNS && index + APPEND_COLUMNS == cols {
                    return div().w_full().h(height).into_any_element();
                }
                div()
                    .id(ElementId::NamedInteger(
                        "tabular-grid-append-row".into(),
                        index as u64,
                    ))
                    .w_full()
                    .h(height)
                    .flex()
                    .items_center()
                    .px_1()
                    .gap_1()
                    .cursor_pointer()
                    .text_ui_sm(cx)
                    .text_color(colors.text_muted)
                    .hover(|style| style.bg(colors.element_hover))
                    .when(index == 0, |this| {
                        this.justify_center()
                            .debug_selector(|| APPEND_ROW_DEBUG_SELECTOR.to_owned())
                            .child(
                                Icon::new(IconName::Plus)
                                    .size(IconSize::Indicator)
                                    .color(Color::Accent),
                            )
                    })
                    .when(index == ROW_IDENTIFIER_COLUMNS, |this| {
                        this.child("行を追加")
                    })
                    .tooltip(Tooltip::text("末尾に 1 行追加"))
                    .on_click(cx.listener(|this, _event, window, cx| {
                        this.append_row(window, cx);
                    }))
                    .into_any_element()
            })
            .collect()
    }

    /// The heading of the trailing "+" column.
    fn create_append_column_header(&self, cx: &Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        div()
            .id(ElementId::Name("tabular-grid-append-column".into()))
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .hover(|style| style.bg(colors.element_hover))
            .debug_selector(|| APPEND_COLUMN_DEBUG_SELECTOR.to_owned())
            .child(
                Icon::new(IconName::Plus)
                    .size(IconSize::Indicator)
                    .color(Color::Accent),
            )
            .tooltip(Tooltip::text("末尾に 1 列追加"))
            .on_click(cx.listener(|this, _event, window, cx| {
                this.append_column(window, cx);
            }))
            .into_any_element()
    }

    /// How thick the handles between two headings are drawn.
    ///
    /// Never more than half of a row, because the rest of the heading is what
    /// selects the row, and a small buffer font makes the rows small enough for
    /// a fixed band to swallow the whole of it.
    fn insert_handle_thickness(&self) -> Pixels {
        INSERT_HANDLE_THICKNESS.min((self.row_height + CELL_FRAME_HEIGHT) / 2.)
    }

    /// The "+" that appears on the top edge of a row heading while the pointer
    /// is over it, which is the boundary a row inserted there lands on.
    ///
    /// Every boundary of the body is reachable this way: the one above row *n*
    /// through row *n*'s own handle, and the one after the last row through the
    /// trailing "+" row.
    pub(crate) fn insert_row_handle(
        &self,
        row: GridRow,
        group_name: SharedString,
        cx: &Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        div()
            .id(ElementId::NamedInteger(
                "tabular-grid-insert-row".into(),
                row.index() as u64,
            ))
            .visible_on_hover(group_name)
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .h(self.insert_handle_thickness())
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .bg(colors.element_selected)
            .debug_selector(|| INSERT_ROW_HANDLE_DEBUG_SELECTOR.to_owned())
            .child(
                Icon::new(IconName::Plus)
                    .size(IconSize::Indicator)
                    .color(Color::Accent),
            )
            .tooltip(Tooltip::text("ここに 1 行挿入"))
            // On the press rather than on the click, and stopping there: the
            // handle lies over the heading, which selects its row on a press,
            // and a "+" that selected the row as well as inserting above it
            // would leave the selection somewhere the user did not put it.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.insert_row_above(row, window, cx);
                    // Stopping the press also took it from the handler that
                    // puts the caret back into the cell editor.
                    this.focus_cell_editor(window, cx);
                }),
            )
            .into_any_element()
    }

    /// The same for the left edge of a column heading.
    pub(crate) fn insert_column_handle(
        &self,
        col: AnyColumn,
        group_name: SharedString,
        cx: &Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        div()
            .id(ElementId::NamedInteger(
                "tabular-grid-insert-column".into(),
                col.get() as u64,
            ))
            .visible_on_hover(group_name)
            .absolute()
            .top_0()
            .bottom_0()
            .left_0()
            .w(INSERT_HANDLE_THICKNESS)
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .bg(colors.element_selected)
            .debug_selector(|| INSERT_COLUMN_HANDLE_DEBUG_SELECTOR.to_owned())
            .child(
                Icon::new(IconName::Plus)
                    .size(IconSize::Indicator)
                    .color(Color::Accent),
            )
            .tooltip(Tooltip::text("ここに 1 列挿入"))
            // As for the row handle: the column heading selects its column on a
            // press, and the handle lies over the heading's own text.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.insert_column_left_of(col, window, cx);
                    this.focus_cell_editor(window, cx);
                }),
            )
            .into_any_element()
    }

    /// A button for a file that has no table at all yet, where there is no
    /// heading to hang a "+" off.
    pub(crate) fn render_start_table_button(&self, cx: &Context<Self>) -> AnyElement {
        Button::new(
            ElementId::Name("tabular-grid-start-table".into()),
            "行を追加",
        )
        .style(ButtonStyle::Subtle)
        .size(ButtonSize::Compact)
        .start_icon(Icon::new(IconName::Plus).size(IconSize::Small))
        .tooltip(Tooltip::text("1 行目を作ります"))
        .on_click(cx.listener(|this, _event, window, cx| {
            this.append_row(window, cx);
        }))
        .into_any_element()
    }

    pub(crate) fn register_append_actions(&self, element: Div, cx: &mut Context<Self>) -> Div {
        element
            .on_action(cx.listener(|this, _: &AppendRow, window, cx| {
                this.append_row(window, cx);
            }))
            .on_action(cx.listener(|this, _: &AppendColumn, window, cx| {
                this.append_column(window, cx);
            }))
    }

    ///// Growing the table /////

    /// Appends one row after the last row of the file and puts the caret in it.
    ///
    /// The row goes to the end of the data even while a sort or a filter is
    /// applied, which is where a spreadsheet puts it; the caret then follows it
    /// to wherever the display order has it drawn.
    pub(crate) fn append_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A cell left open for input is written out first, exactly as a click on
        // a row heading does it. The caret is about to move into the new row, and
        // an edit still addressed at the old cell would otherwise be committed
        // long after the user stopped looking at it.
        self.commit_editing(window, cx);
        let position = RowInsertPosition::AtEnd;
        let edits = self
            .with_edit_engine(cx, |engine| engine.insert_rows(position, 1))
            .unwrap_or_default();
        if edits.is_empty() {
            return;
        }
        // Resolved before the edit, while the physical rows still mean what the
        // stored colors were addressed against.
        let at = self.physical_insert_position(position);
        let transaction_id = self.apply_buffer_edits(edits, cx);
        self.record_style_shift(transaction_id, StyleShift::InsertRows { at, count: 1 }, cx);

        // A file that had nothing in it gains its first line rather than a data
        // row, and that line is the header row.
        //
        // The contents have already been reparsed, except on a file too large
        // to parse on the foreground: there the caret lands on what is still the
        // last row, one short of the new one, until the background parse lands.
        // Moving it afterwards would fight whatever the user did in between.
        let inserted = match self.engine.contents.rows.len().checked_sub(1) {
            Some(last_row) => self
                .display_row_for_data_row(DataRow(last_row))
                .map(GridRow::Body),
            None => Some(GridRow::Header),
        };
        self.reveal_inserted_row(inserted, window, cx);
    }

    /// Appends one column after the last column and puts the caret in it.
    pub(crate) fn append_column(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A file with no columns has no row for a column to be appended to, so
        // the first "+" of either kind is the one that creates that row.
        if self.grid_column_count() == 0 {
            self.append_row(window, cx);
            return;
        }
        self.insert_column_at(self.grid_column_count(), window, cx);
    }

    /// Inserts one row above `row`, which is what the "+" between two row
    /// headings does.
    pub(crate) fn insert_row_above(
        &mut self,
        row: GridRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Before the insert, and for a stronger reason than in `append_row`:
        // inserting above a row shifts the data index of every row below it, so
        // an edit still open on one of those rows would be committed into the
        // blank row that took its index.
        self.commit_editing(window, cx);
        let Some(row_ref) = self.row_ref(row) else {
            return;
        };
        // A blank line above the header row would become the file's first line
        // and therefore the new header, demoting the real one to a data row.
        if row_ref == RowRef::Header {
            self.set_status_message("見出し行の上には行を挿入できません", cx);
            return;
        }
        let position = RowInsertPosition::Before(row_ref);
        let edits = self
            .with_edit_engine(cx, |engine| engine.insert_rows(position, 1))
            .unwrap_or_default();
        if edits.is_empty() {
            return;
        }
        let at = self.physical_insert_position(position);
        let transaction_id = self.apply_buffer_edits(edits, cx);
        self.record_style_shift(transaction_id, StyleShift::InsertRows { at, count: 1 }, cx);

        // The blank row takes over the data index of the row it was inserted
        // above, which is where the caret has to end up.
        let inserted = match row_ref {
            RowRef::Data(data_row) => self.display_row_for_data_row(data_row).map(GridRow::Body),
            RowRef::Header => None,
        };
        self.reveal_inserted_row(inserted, window, cx);
    }

    /// Inserts one column to the left of `col`, which is what the "+" between
    /// two column headings does.
    pub(crate) fn insert_column_left_of(
        &mut self,
        col: AnyColumn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.insert_column_at(col.get(), window, cx);
    }

    /// Inserts one column at `index`, which `0..=grid_column_count()` addresses.
    fn insert_column_at(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        // As for a row: a column inserted at or before the open cell's column
        // shifts it, and the edit would land in the blank column instead.
        self.commit_editing(window, cx);
        let edits = self
            .with_edit_engine(cx, |engine| engine.insert_columns(index, 1))
            .unwrap_or_default();
        if edits.is_empty() {
            return;
        }
        // Before the edit, so that the reparse it triggers sees the filters on
        // the columns they belong to. See [`TabularDataPreviewPane::insert_columns`].
        let mut shift = ColumnShift::Insert {
            at: index,
            count: 1,
        };
        self.engine.apply_column_shift(&mut shift);
        let transaction_id = self.apply_buffer_edits(edits, cx);
        self.engine.record_column_shift(transaction_id, shift);
        self.record_style_shift(
            transaction_id,
            StyleShift::InsertColumns {
                at: index,
                count: 1,
            },
            cx,
        );

        // The topmost cell of the new column, which is the header row when the
        // file has one. A column is not filtered away, so it is always there.
        let row = GridRow::from_index(self.first_row_index());
        self.select_cell(GridCell::new(row, AnyColumn(index)), false, cx);
        self.scroll_cursor_into_view(window, cx);
    }

    /// Puts the caret in the first cell of a row that was just inserted.
    ///
    /// A filter can leave the new row with nowhere to draw it, and a "+" that
    /// appeared to do nothing at all would read as broken, so that case says so
    /// instead of moving the caret somewhere unrelated.
    fn reveal_inserted_row(
        &mut self,
        row: Option<GridRow>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match row {
            Some(row) => {
                self.select_cell(GridCell::new(row, AnyColumn(0)), false, cx);
                self.scroll_cursor_into_view(window, cx);
            }
            None => self.set_status_message("追加した行は絞り込みで表示されていません", cx),
        }
    }

    /// Where `data_row` is currently drawn. A sort moves it and a filter can
    /// take it off the screen entirely, which is `None`.
    ///
    /// The untransformed table answers without a lookup, which is the case that
    /// a file of a million rows is in; the walk is over the rows a sort or
    /// filter left visible, and only ever runs when the user asked for a row.
    fn display_row_for_data_row(&self, data_row: DataRow) -> Option<DisplayRow> {
        let visible_rows = self.body_row_count();
        let mapping = self.engine.d2d_mapping();
        if mapping.is_identity() {
            return (*data_row < visible_rows).then_some(DisplayRow(*data_row));
        }
        (0..visible_rows)
            .map(DisplayRow)
            .find(|&display_row| mapping.get_data_row(display_row) == Some(data_row))
    }
}

#[cfg(test)]
mod tests {
    use editor::Editor;
    use gpui::{Modifiers, TestAppContext, VisualTestContext};

    use super::*;
    use crate::{
        grid::GridSelectionKind,
        grid_editing::{
            GridMenuEntry, GridMenuTarget, grid_context_menu_entries, tests::open_grid,
        },
        table_data_engine::sorting_by_column::{AppliedSorting, SortDirection},
    };

    fn buffer_text(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> String {
        editor.read_with(cx, |editor, cx| editor.text(cx))
    }

    /// The text of every line of the menu, which is what is worth holding it to.
    fn menu_labels(target: GridMenuTarget) -> Vec<&'static str> {
        grid_context_menu_entries(target)
            .iter()
            .filter_map(|entry| match entry {
                GridMenuEntry::Separator => None,
                GridMenuEntry::Action { label, .. }
                | GridMenuEntry::Colors { label, .. }
                | GridMenuEntry::Sort { label, .. }
                | GridMenuEntry::Filter { label, .. } => Some(*label),
            })
            .collect()
    }

    /// The columns the menu's sorting entries would act on.
    fn menu_sort_columns(target: GridMenuTarget) -> Vec<AnyColumn> {
        grid_context_menu_entries(target)
            .iter()
            .filter_map(|entry| match entry {
                GridMenuEntry::Sort { col, .. } => Some(*col),
                _ => None,
            })
            .collect()
    }

    /// A row heading's menu is about rows and a column heading's about columns;
    /// offering both everywhere is what made the grid's one menu unreadable.
    #[test]
    fn test_the_context_menu_is_about_what_was_clicked() {
        let cell = menu_labels(GridMenuTarget::Cell);
        assert!(cell.contains(&"上に行を挿入"), "{cell:?}");
        assert!(cell.contains(&"左に列を挿入"), "{cell:?}");

        let rows = menu_labels(GridMenuTarget::RowHeader);
        assert_eq!(rows.first(), Some(&"上に行を挿入"), "{rows:?}");
        assert!(rows.contains(&"行を削除"), "{rows:?}");
        assert!(
            !rows.iter().any(|label| label.contains('列')),
            "a row heading's menu should not offer column operations: {rows:?}"
        );

        let columns = menu_labels(GridMenuTarget::ColumnHeader(AnyColumn(0)));
        assert_eq!(columns.first(), Some(&"左に列を挿入"), "{columns:?}");
        assert!(columns.contains(&"列を削除"), "{columns:?}");
        assert!(columns.contains(&"昇順で並べ替え"), "{columns:?}");
        assert!(
            !columns.iter().any(|label| label.contains('行')),
            "a column heading's menu should not offer row operations: {columns:?}"
        );

        // Every menu still copies and paints, which is what the cell menu was
        // always used for.
        for (target, labels) in [
            (GridMenuTarget::Cell, &cell),
            (GridMenuTarget::RowHeader, &rows),
            (GridMenuTarget::ColumnHeader(AnyColumn(0)), &columns),
        ] {
            assert!(labels.contains(&"コピー"), "{target:?}: {labels:?}");
            assert!(labels.contains(&"内容を消去"), "{target:?}: {labels:?}");
            assert!(labels.contains(&"塗りつぶしの色"), "{target:?}: {labels:?}");
        }
    }

    /// Right clicking a heading that is already inside the selection keeps that
    /// selection, so the caret stays wherever it was: the sorting entries have to
    /// carry the column that was clicked, or they sort the wrong one silently.
    #[test]
    fn test_the_sort_entries_act_on_the_clicked_column() {
        assert_eq!(
            menu_sort_columns(GridMenuTarget::ColumnHeader(AnyColumn(2))),
            vec![AnyColumn(2), AnyColumn(2), AnyColumn(2)]
        );
        assert!(menu_sort_columns(GridMenuTarget::Cell).is_empty());
        assert!(menu_sort_columns(GridMenuTarget::RowHeader).is_empty());
    }

    /// A one column file's blank row is nothing but its own line terminator, and
    /// at the end of a file that has none that terminator reads as the file's
    /// trailing newline, which is not a row. The first press has to add a row
    /// like any other.
    #[gpui::test]
    async fn test_the_plus_row_appends_to_a_single_column_file(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a\nb", cx).await;
        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.grid_column_count(), 1);
            assert_eq!(pane.body_row_count(), 1);
        });

        cx.dispatch_action(AppendRow);
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "a\nb\n\n");
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.body_row_count(),
                2,
                "the first press has to add a row, not only a newline"
            );
            assert_eq!(pane.grid.cursor.row, GridRow::Body(DisplayRow(1)));
        });

        cx.dispatch_action(editor::actions::Undo);
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &mut cx), "a\nb");
    }

    /// A cell left open for input is addressed at the row it was opened on, by
    /// data index. Inserting a row above that row hands its index to the blank
    /// row, so the edit has to be written out before the insert rather than
    /// after it.
    #[gpui::test]
    async fn test_inserting_a_row_writes_out_the_open_cell_first(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("h,n\na,1\nb,2", cx).await;

        cx.simulate_keystrokes("down down");
        cx.simulate_input("zzz");
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.grid.cursor.row, GridRow::Body(DisplayRow(1)));
            assert!(pane.grid.editing.is_some());
        });

        pane.update_in(&mut cx, |pane, window, cx| {
            pane.insert_row_above(GridRow::Body(DisplayRow(1)), window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            buffer_text(&editor, &mut cx),
            "h,n\na,1\n,\nzzz,2",
            "the typed value belongs to the row it was typed into, not to the \
             blank row that took that row's index"
        );
        pane.read_with(&cx, |pane, _| {
            assert!(
                pane.grid.editing.is_none(),
                "the cell should no longer be open for input"
            );
        });
    }

    #[gpui::test]
    async fn test_the_plus_row_appends_one_row_in_one_undo(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\nc,d", cx).await;

        cx.dispatch_action(AppendRow);
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "a,b\nc,d\n,");
        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.body_row_count(), 2);
            // The caret follows into the new row's first cell.
            assert_eq!(pane.grid.cursor.row, GridRow::Body(DisplayRow(1)));
            assert_eq!(pane.grid.cursor.col, AnyColumn(0));
        });

        cx.dispatch_action(editor::actions::Undo);
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &mut cx), "a,b\nc,d");
    }

    #[gpui::test]
    async fn test_the_plus_column_appends_one_column_in_one_undo(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\nc,d", cx).await;

        cx.dispatch_action(AppendColumn);
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "a,b,\nc,d,");
        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.grid_column_count(), 3);
            assert_eq!(pane.grid.cursor.col, AnyColumn(2));
            // The topmost cell of the new column, which is the heading here.
            assert_eq!(pane.grid.cursor.row, GridRow::Header);
        });

        cx.dispatch_action(editor::actions::Undo);
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &mut cx), "a,b\nc,d");
    }

    /// The "+" row is drawn by the list, but it is not a row of the file: the
    /// selection clamps to the rows that exist and the caret must not be able to
    /// walk onto the strip.
    #[gpui::test]
    async fn test_the_plus_row_is_not_a_row_of_the_table(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("a,b\nc,d\ne,f", cx).await;

        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.body_row_count(), 2);
            assert_eq!(pane.append_row_index(), 2);
            assert_eq!(
                pane.list_state.item_count(),
                3,
                "the list has to hold one item more than the table has rows"
            );
        });

        cx.simulate_keystrokes("down down down down");
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.grid.cursor.row, GridRow::Body(DisplayRow(1)));
        });
    }

    /// Both strips have to be on screen without being hovered, or the user has
    /// no way of finding out they are there.
    #[gpui::test]
    async fn test_both_plus_strips_are_drawn(cx: &mut TestAppContext) {
        let (_pane, _editor, mut cx) = open_grid("a,b\nc,d", cx).await;
        cx.run_until_parked();

        let append_row = cx
            .debug_bounds(APPEND_ROW_DEBUG_SELECTOR)
            .expect("the table should draw the trailing \"+\" row");
        let append_column = cx
            .debug_bounds(APPEND_COLUMN_DEBUG_SELECTOR)
            .expect("the table should draw the trailing \"+\" column");
        // `debug_bounds` answers for elements of no size too, which would make
        // the assertions above hold for a table that drew nothing at all.
        assert!(
            append_row.size.width > px(0.) && append_row.size.height > px(0.),
            "the \"+\" row was drawn with no size ({append_row:?})"
        );
        assert!(
            append_column.size.width > px(0.) && append_column.size.height > px(0.),
            "the \"+\" column was drawn with no size ({append_column:?})"
        );
        assert!(
            append_column.origin.x > append_row.origin.x,
            "the \"+\" column belongs to the right of the table ({append_column:?})"
        );
    }

    /// The "+" between two headings only shows while the heading is hovered,
    /// but it is laid out either way, and it has to be laid out *on* the
    /// boundary it inserts at: a band over the whole heading would take the
    /// heading's own click, and one of no width could never be reached.
    #[gpui::test]
    async fn test_the_insert_handles_lie_along_the_boundaries(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("a,b\nc,d", cx).await;
        cx.run_until_parked();
        let row_extent = pane.read_with(&cx, |pane, _| pane.row_height + CELL_FRAME_HEIGHT);

        let row_handle = cx
            .debug_bounds(INSERT_ROW_HANDLE_DEBUG_SELECTOR)
            .expect("a row heading should carry the \"+\" that inserts above it");
        assert!(
            row_handle.size.height > px(0.) && row_handle.size.height * 2. <= row_extent,
            "the row handle should be a band along the heading's top edge, and \
             leave the rest of the heading to select the row with \
             ({row_handle:?} of a {row_extent:?} row)"
        );
        assert!(
            row_handle.size.width > INSERT_HANDLE_THICKNESS,
            "the row handle should span the heading's width ({row_handle:?})"
        );

        let column_handle = cx
            .debug_bounds(INSERT_COLUMN_HANDLE_DEBUG_SELECTOR)
            .expect("a column heading should carry the \"+\" that inserts to its left");
        assert_eq!(
            column_handle.size.width, INSERT_HANDLE_THICKNESS,
            "the column handle should be a band along the heading's left edge \
             ({column_handle:?})"
        );
        assert!(
            column_handle.size.height > INSERT_HANDLE_THICKNESS,
            "the column handle should span the heading's height ({column_handle:?})"
        );
    }

    /// The whole of the boundary "+": hovering the heading reveals it, pressing
    /// it inserts one row, and the press does not reach the heading underneath,
    /// which would otherwise select the row the user only pointed at.
    #[gpui::test]
    async fn test_hovering_a_row_heading_reveals_a_plus_that_inserts(cx: &mut TestAppContext) {
        // One body row, so that the single row handle on screen is its own.
        let (pane, editor, mut cx) = open_grid("h,n\na,1", cx).await;
        cx.run_until_parked();
        let handle = cx
            .debug_bounds(INSERT_ROW_HANDLE_DEBUG_SELECTOR)
            .expect("the row heading should carry a \"+\"");

        // Until the heading is hovered the handle is not painted at all, so
        // this is also what makes it clickable.
        cx.simulate_mouse_move(handle.center(), None, Modifiers::default());
        cx.run_until_parked();
        cx.simulate_click(handle.center(), Modifiers::default());
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "h,n\n,\na,1");
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.grid.selection_kind,
                GridSelectionKind::Cells,
                "pressing the \"+\" should not select the row it belongs to"
            );
            assert_eq!(pane.grid.cursor.row, GridRow::Body(DisplayRow(0)));
        });
    }

    /// An empty file has no heading to hang a "+" off, so the table has to be
    /// startable from the message that stands in for it.
    #[gpui::test]
    async fn test_a_table_can_be_started_from_an_empty_file(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("", cx).await;
        pane.read_with(&cx, |pane, _| assert_eq!(pane.grid_column_count(), 0));

        cx.dispatch_action(AppendRow);
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "\n");
        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.grid_column_count(), 1);
            assert!(pane.has_header_row());
            assert_eq!(pane.grid.cursor.row, GridRow::Header);
        });

        // With a row to append to, the "+" column works from there on.
        cx.dispatch_action(AppendColumn);
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| assert_eq!(pane.grid_column_count(), 2));
    }

    /// The "+" of the empty file is the row one: a file with no rows has nothing
    /// for a column to be added to.
    #[gpui::test]
    async fn test_the_plus_column_starts_an_empty_file_too(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("", cx).await;

        cx.dispatch_action(AppendColumn);
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "\n");
        pane.read_with(&cx, |pane, _| assert_eq!(pane.grid_column_count(), 1));
    }

    #[gpui::test]
    async fn test_the_row_handle_inserts_above_the_row_it_belongs_to(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("h,n\na,1\nb,2", cx).await;

        pane.update_in(&mut cx, |pane, window, cx| {
            pane.insert_row_above(GridRow::Body(DisplayRow(1)), window, cx);
        });
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "h,n\na,1\n,\nb,2");
        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.grid.cursor.row, GridRow::Body(DisplayRow(1)));
            assert_eq!(pane.grid.cursor.col, AnyColumn(0));
        });

        cx.dispatch_action(editor::actions::Undo);
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &mut cx), "h,n\na,1\nb,2");
    }

    /// Inserting above the header row would make the blank line the new heading,
    /// which is why the boundary is refused rather than silently moved.
    #[gpui::test]
    async fn test_the_row_handle_refuses_the_header_row(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("h,n\na,1", cx).await;

        pane.update_in(&mut cx, |pane, window, cx| {
            pane.insert_row_above(GridRow::Header, window, cx);
        });
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "h,n\na,1");
        pane.read_with(&cx, |pane, _| {
            assert!(pane.grid.status_message.is_some());
        });
    }

    #[gpui::test]
    async fn test_the_column_handle_inserts_to_the_left_of_its_column(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\nc,d", cx).await;

        pane.update_in(&mut cx, |pane, window, cx| {
            pane.insert_column_left_of(AnyColumn(1), window, cx);
        });
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "a,,b\nc,,d");
        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.grid.cursor.col, AnyColumn(1));
        });

        cx.dispatch_action(editor::actions::Undo);
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &mut cx), "a,b\nc,d");
    }

    /// A sorted table still grows at the end of the file, and the caret has to
    /// find the new row wherever the sort put it.
    #[gpui::test]
    async fn test_the_plus_row_follows_the_sort_order(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("h,n\n3,c\n1,a\n2,b", cx).await;
        pane.update(&mut cx, |pane, cx| {
            pane.engine.applied_sorting = Some(AppliedSorting {
                col_idx: AnyColumn(0),
                direction: SortDirection::Asc,
            });
            pane.apply_sort(cx);
        });
        cx.run_until_parked();

        cx.dispatch_action(AppendRow);
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "h,n\n3,c\n1,a\n2,b\n,");
        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.body_row_count(), 4);
            assert_eq!(
                pane.row_ref(pane.grid.cursor.row),
                Some(RowRef::Data(DataRow(3))),
                "the caret should be in the appended row, not in the row that \
                 happens to be drawn last"
            );
        });
    }

    /// A filter can hide the appended row. Saying so is the only way for the
    /// user to tell that from a "+" that did nothing.
    #[gpui::test]
    async fn test_a_filter_that_hides_the_appended_row_says_so(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("h,n\na,1\nb,2", cx).await;
        pane.update(&mut cx, |pane, cx| {
            pane.toggle_filter(AnyColumn(0), Some("a".into()), cx);
        });
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| assert_eq!(pane.body_row_count(), 1));

        cx.dispatch_action(AppendRow);
        cx.run_until_parked();

        // The row is in the file, it just has nowhere to be drawn.
        assert_eq!(buffer_text(&editor, &mut cx), "h,n\na,1\nb,2\n,");
        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.body_row_count(), 1);
            assert!(
                pane.grid.status_message.is_some(),
                "the user has to be told the new row is filtered away"
            );
        });
    }
}
