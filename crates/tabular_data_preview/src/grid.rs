//! The spreadsheet grid: what is selected, where the caret is, and how the
//! pointer moves both.
//!
//! The cell editor itself, the keyboard handlers and the structural operations
//! live in [`crate::grid_editing`]; this module owns the state they work on.

use std::{cell::Cell as MutableCell, ops::Range, rc::Rc, sync::Arc};

use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, Bounds, Entity, Focusable as _, ListOffset, MouseButton, Point, Subscription,
    TextStyleRefinement, canvas,
};
use settings::Settings as _;
use theme_settings::ThemeSettings;
use ui::{ContextMenu, prelude::*};

use crate::{
    TabularDataPreviewPane,
    edit_engine::RowRef,
    grid_search::GridSearchState,
    types::{AnyColumn, DisplayRow, TableCell},
};

/// A row of the grid.
///
/// `Header` is the first physical line of the file. It only exists while the
/// parser produced a header row for it, and it is always drawn in the table's
/// sticky header so an edit to it lands on line 1 either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum GridRow {
    Header,
    Body(DisplayRow),
}

impl GridRow {
    /// Position of this row counting from the top of the grid, which is what
    /// the movement and rectangle maths work in.
    pub(crate) fn index(self) -> usize {
        match self {
            GridRow::Header => 0,
            GridRow::Body(row) => row.get() + 1,
        }
    }

    pub(crate) fn from_index(index: usize) -> Self {
        match index {
            0 => GridRow::Header,
            index => GridRow::Body(DisplayRow(index - 1)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GridCell {
    pub(crate) row: GridRow,
    pub(crate) col: AnyColumn,
}

impl GridCell {
    pub(crate) fn new(row: GridRow, col: AnyColumn) -> Self {
        Self { row, col }
    }
}

/// What the current selection is anchored to, which decides how it grows and
/// what the row and column operations act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GridSelectionKind {
    Cells,
    Rows,
    Columns,
    All,
}

/// Excel's two states while a cell is open for input: typing over the old value
/// versus editing it in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CellEditMode {
    /// Started by typing a character. The arrow keys commit and move on.
    Replace,
    /// Started by F2 or a double click. The arrow keys move within the text.
    Edit,
}

/// What a held mouse button is currently extending.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GridDrag {
    Cells,
    Rows,
    Columns,
}

/// The rectangle of cells covered by the selection, in grid coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GridRect {
    pub(crate) first_row: usize,
    pub(crate) last_row: usize,
    pub(crate) first_col: usize,
    pub(crate) last_col: usize,
}

impl GridRect {
    pub(crate) fn contains(&self, row: GridRow, col: AnyColumn) -> bool {
        let row = row.index();
        let col = col.get();
        (self.first_row..=self.last_row).contains(&row)
            && (self.first_col..=self.last_col).contains(&col)
    }

    pub(crate) fn contains_row(&self, row: GridRow) -> bool {
        (self.first_row..=self.last_row).contains(&row.index())
    }

    pub(crate) fn contains_col(&self, col: AnyColumn) -> bool {
        (self.first_col..=self.last_col).contains(&col.get())
    }

    pub(crate) fn cols(&self) -> Range<usize> {
        self.first_col..self.last_col + 1
    }

    pub(crate) fn cell_count(&self) -> usize {
        (self.last_row - self.first_row + 1).saturating_mul(self.last_col - self.first_col + 1)
    }
}

/// The bounds an element reported during the previous frame's prepaint.
///
/// The grid positions the overlay editor from real laid out geometry, which is
/// only known one frame late; `measured_at` tells a fresh measurement from one
/// left behind by an element that has since scrolled out of the list.
#[derive(Default)]
pub(crate) struct BoundsProbe {
    value: MutableCell<Option<Bounds<Pixels>>>,
    measured_at: MutableCell<u64>,
}

impl BoundsProbe {
    /// The bounds measured during the frame before `generation`, or `None` when
    /// the element was not painted then.
    pub(crate) fn measured(&self, generation: u64) -> Option<Bounds<Pixels>> {
        if self.measured_at.get().wrapping_add(1) == generation {
            self.value.get()
        } else {
            None
        }
    }

    /// The bounds measured during the frame `generation` itself.
    ///
    /// This is what a reader running between frames - a key handler - has to
    /// ask for, where [`Self::measured`] answers for the frame before the one
    /// being laid out, which is what an element asks for while it is built.
    pub(crate) fn painted(&self, generation: u64) -> Option<Bounds<Pixels>> {
        if self.measured_at.get() == generation {
            self.value.get()
        } else {
            None
        }
    }

    /// Records a measurement, reporting whether the reader needs another frame.
    ///
    /// An element that comes back after a gap counts as moved even when it
    /// lands on the same rectangle: the frame in between read `None` and drew
    /// the overlay somewhere else.
    pub(crate) fn record(&self, generation: u64, bounds: Bounds<Pixels>) -> bool {
        let was_measured_last_frame = self.measured_at.get().wrapping_add(1) == generation;
        let moved = !was_measured_last_frame || self.value.get() != Some(bounds);
        self.value.set(Some(bounds));
        self.measured_at.set(generation);
        moved
    }
}

pub(crate) struct GridState {
    /// The one editor every cell is typed into. It is mounted for the whole
    /// life of the pane and merely moved over the active cell, so that it never
    /// loses focus to list virtualization and input methods keep working.
    pub(crate) cell_editor: Entity<Editor>,
    pub(crate) cursor: GridCell,
    /// The far corner of a range selection; `None` when a single cell is
    /// selected.
    pub(crate) anchor: Option<GridCell>,
    pub(crate) selection_kind: GridSelectionKind,
    pub(crate) editing: Option<CellEditMode>,
    /// Where the cell being edited lives in the data, resolved when the edit
    /// started. Held in data coordinates so that a sort or filter changing the
    /// display order mid-edit cannot make the commit land on another row.
    pub(crate) editing_target: Option<(RowRef, AnyColumn)>,
    /// Where that cell is currently drawn, which is only used to place the
    /// overlay editor.
    pub(crate) editing_cell: Option<GridCell>,
    /// A message shown in the toolbar, e.g. when an operation was refused.
    pub(crate) status_message: Option<SharedString>,
    /// Whether that message stands until the state behind it changes rather
    /// than until the user's next move. A refused operation is only about the
    /// key that was just pressed, so it has to go once the user does something
    /// else; the search bar's "too many matches" describes the search itself
    /// and has to stay while the user walks the hits.
    pub(crate) status_message_is_sticky: bool,
    /// What the buffer search bar is currently highlighting.
    pub(crate) search: GridSearchState,
    pub(crate) drag: Option<GridDrag>,
    pub(crate) context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    pub(crate) active_cell_bounds: Rc<BoundsProbe>,
    pub(crate) root_bounds: Rc<BoundsProbe>,
    pub(crate) body_bounds: Rc<BoundsProbe>,
    pub(crate) render_generation: u64,
    /// Whether the last frame drew the overlay editor at a size the user can
    /// see, rather than the pixel it hides at while no cell is open.
    #[cfg(test)]
    pub(crate) cell_editor_was_drawn: MutableCell<bool>,
    _subscriptions: Vec<Subscription>,
}

impl GridState {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<TabularDataPreviewPane>) -> Self {
        let line_height = cell_line_height(cx);
        let cell_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text_style_refinement(cell_text_style(line_height, cx));
            editor
        });

        let subscriptions = vec![
            cx.subscribe_in(&cell_editor, window, |this, _editor, event, window, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    this.on_cell_editor_edited(window, cx);
                }
            }),
            cx.on_focus_out(
                &cell_editor.focus_handle(cx),
                window,
                |this, _event, window, cx| {
                    this.commit_editing(window, cx);
                },
            ),
        ];

        Self {
            cell_editor,
            cursor: GridCell::new(GridRow::Header, AnyColumn(0)),
            anchor: None,
            selection_kind: GridSelectionKind::Cells,
            editing: None,
            editing_target: None,
            editing_cell: None,
            status_message: None,
            status_message_is_sticky: false,
            search: GridSearchState::default(),
            drag: None,
            context_menu: None,
            active_cell_bounds: Rc::default(),
            root_bounds: Rc::default(),
            body_bounds: Rc::default(),
            render_generation: 0,
            #[cfg(test)]
            cell_editor_was_drawn: MutableCell::new(false),
            _subscriptions: subscriptions,
        }
    }
}

/// The style the cells draw their text in, and that the overlay editor types
/// in on top of them.
///
/// Both go through this one function: an overlay whose font differed from the
/// cell it covers would move the text the moment the cell was opened. It is the
/// buffer font rather than the UI font because the grid is a view of buffer
/// text, and it is the same font the file's text tab shows.
///
/// `line_height` is taken as an argument rather than derived so that the rows,
/// the cell text and the editor are all laid out from the one value the pane
/// settled on.
pub(crate) fn cell_text_style(line_height: Pixels, cx: &App) -> TextStyleRefinement {
    let settings = ThemeSettings::get_global(cx);
    let font = &settings.buffer_font;
    TextStyleRefinement {
        font_family: Some(font.family.clone()),
        font_features: Some(font.features.clone()),
        font_size: Some(settings.buffer_font_size(cx).into()),
        font_weight: Some(font.weight),
        line_height: Some(line_height.into()),
        ..Default::default()
    }
}

/// How tall one line of cell text is.
///
/// Derived from the buffer font, which is what the cells and the overlay editor
/// are drawn in: a row sized from the window's UI text would keep its height
/// when the user enlarges the buffer font, and the text would outgrow it.
///
/// Rounded to a whole pixel because that is what the editor does with the line
/// height it is handed ([`gpui::TextStyle::line_height_in_pixels`]), and a cell
/// whose text sat on a half pixel the editor rounded away would be half a pixel
/// shorter than the editor that opens over it.
pub(crate) fn cell_line_height(cx: &App) -> Pixels {
    let settings = ThemeSettings::get_global(cx);
    (settings.buffer_font_size(cx) * settings.buffer_line_height.value()).round()
}

/// How many rows a page key covers before the body has ever been laid out.
pub(crate) const ROWS_PER_PAGE_BEFORE_LAYOUT: usize = 10;

/// What the table draws around a row's line of text: the two pixel border every
/// cell carries so that highlighting the caret's cell does not nudge the text of
/// its neighbours, plus the line between two rows. Only used until the list has
/// drawn a row to measure.
const ROW_BORDER_HEIGHT: Pixels = px(5.);

/// Largest number of cells one grid operation may touch. Past this the vector
/// of cells the operation builds costs more than the edit is worth, so the grid
/// refuses and points at the text editor instead.
pub(crate) const MAX_GRID_OPERATION_CELLS: usize = 200_000;

/// A count as the user reads it, in groups of three digits: `200,000`.
pub(crate) fn grouped_digits(value: usize) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.char_indices() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// The spreadsheet name of a column: A, B, … Z, AA, AB, …
pub(crate) fn column_label(index: usize) -> String {
    let mut label = String::new();
    let mut remaining = index;
    loop {
        let digit = remaining % 26;
        label.insert(0, char::from(b'A' + digit as u8));
        if remaining < 26 {
            break;
        }
        remaining = remaining / 26 - 1;
    }
    label
}

/// Which way [`TabularDataPreviewPane::move_cursor`] takes the caret.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GridMotion {
    Up,
    Down,
    Left,
    Right,
    /// Home: the first column of the row.
    RowStart,
    /// End: the last column of the row.
    RowEnd,
    /// Ctrl+Home: the top left cell.
    GridStart,
    /// Ctrl+End: the bottom right cell.
    GridEnd,
    PageUp,
    PageDown,
    /// Ctrl+Up: the top of the column.
    ColumnStart,
    /// Ctrl+Down: the bottom of the column.
    ColumnEnd,
}

impl TabularDataPreviewPane {
    /// Whether the file's first line is a header row. With the setting off the
    /// parser leaves that line among the data rows and there is no header row
    /// to put a caret in.
    pub(crate) fn has_header_row(&self) -> bool {
        self.engine.contents.header_span.is_some()
    }

    pub(crate) fn grid_column_count(&self) -> usize {
        self.engine.contents.number_of_cols
    }

    /// Number of rows in the scrolling body, which is what the row list and the
    /// display to data mapping are sized by.
    /// Capped at the rows that actually exist: the mapping is recomputed a
    /// moment after the contents on a big file, so between the two the grid
    /// must not ask for rows the table no longer has.
    pub(crate) fn body_row_count(&self) -> usize {
        self.engine
            .d2d_mapping()
            .visible_row_count()
            .min(self.engine.contents.rows.len())
    }

    pub(crate) fn grid_is_empty(&self) -> bool {
        self.grid_column_count() == 0 || (!self.has_header_row() && self.body_row_count() == 0)
    }

    pub(crate) fn first_row_index(&self) -> usize {
        if self.has_header_row() { 0 } else { 1 }
    }

    pub(crate) fn last_row_index(&self) -> usize {
        self.body_row_count().max(self.first_row_index())
    }

    pub(crate) fn last_col_index(&self) -> usize {
        self.grid_column_count().saturating_sub(1)
    }

    pub(crate) fn clamp_cell(&self, cell: GridCell) -> GridCell {
        let row = cell
            .row
            .index()
            .clamp(self.first_row_index(), self.last_row_index());
        GridCell::new(
            GridRow::from_index(row),
            AnyColumn(cell.col.get().min(self.last_col_index())),
        )
    }

    /// Pulls the caret back inside the table after a parse changed its shape.
    pub(crate) fn clamp_cursor(&mut self) {
        if self.grid_is_empty() {
            return;
        }
        self.grid.cursor = self.clamp_cell(self.grid.cursor);
        self.grid.anchor = self.grid.anchor.map(|anchor| self.clamp_cell(anchor));
    }

    pub(crate) fn selection_rect(&self) -> GridRect {
        let cursor = self.grid.cursor;
        let anchor = self.grid.anchor.unwrap_or(cursor);
        let (cursor_row, anchor_row) = (cursor.row.index(), anchor.row.index());
        let (cursor_col, anchor_col) = (cursor.col.get(), anchor.col.get());

        let mut rect = GridRect {
            first_row: cursor_row.min(anchor_row),
            last_row: cursor_row.max(anchor_row),
            first_col: cursor_col.min(anchor_col),
            last_col: cursor_col.max(anchor_col),
        };

        match self.grid.selection_kind {
            GridSelectionKind::Cells => {}
            GridSelectionKind::Rows => {
                rect.first_col = 0;
                rect.last_col = self.last_col_index();
            }
            GridSelectionKind::Columns => {
                rect.first_row = self.first_row_index();
                rect.last_row = self.last_row_index();
            }
            GridSelectionKind::All => {
                rect.first_row = self.first_row_index();
                rect.last_row = self.last_row_index();
                rect.first_col = 0;
                rect.last_col = self.last_col_index();
            }
        }
        rect
    }

    pub(crate) fn selected_rows(&self) -> Vec<GridRow> {
        let rect = self.selection_rect();
        (rect.first_row..=rect.last_row)
            .map(GridRow::from_index)
            .collect()
    }

    /// The engine's name for `row`: `None` when the file has no header row to
    /// address, or when a display row past the end of the mapping has no data
    /// row behind it.
    pub(crate) fn row_ref(&self, row: GridRow) -> Option<RowRef> {
        match row {
            // An empty file has no header row yet, but it does have the one
            // implicit first line that typing into A1 creates.
            GridRow::Header => (self.has_header_row() || self.engine.contents.rows.is_empty())
                .then_some(RowRef::Header),
            GridRow::Body(display_row) => self
                .engine
                .d2d_mapping()
                .get_data_row(display_row)
                .map(RowRef::Data),
        }
    }

    pub(crate) fn selected_row_refs(&self) -> Vec<RowRef> {
        self.selected_rows()
            .into_iter()
            .filter_map(|row| self.row_ref(row))
            .collect()
    }

    pub(crate) fn cell_value(&self, row: GridRow, col: AnyColumn) -> SharedString {
        match self.row_ref(row) {
            Some(row_ref) => self.cell_value_at(row_ref, col),
            None => SharedString::default(),
        }
    }

    pub(crate) fn cell_value_at(&self, row: RowRef, col: AnyColumn) -> SharedString {
        let source = match row {
            RowRef::Header => Some(&self.engine.contents.headers),
            RowRef::Data(data_row) => self.engine.contents.get_row(data_row),
        };
        source
            .and_then(|row| row.get(col))
            .and_then(|cell| cell.display_value())
            .cloned()
            .unwrap_or_default()
    }

    /// Writes a value straight into the parsed table so the grid shows it at
    /// once, without waiting for the background reparse of a large file.
    ///
    /// Takes the row in data coordinates, so that a sorted or filtered table
    /// updates the row the edit actually went to.
    pub(crate) fn write_cached_cell(&mut self, row: RowRef, col: AnyColumn, value: &str) {
        let contents = Arc::make_mut(&mut self.engine.contents);
        let target = match row {
            RowRef::Header => Some(&mut contents.headers),
            RowRef::Data(data_row) => contents.rows.get_mut(*data_row),
        };
        let Some(cell) = target.and_then(|row| row.as_mut_slice().get_mut(col.get())) else {
            return;
        };
        // A virtual cell has no span in the buffer to point the cached value at;
        // the reparse that follows the edit materialises it instead.
        if let TableCell::Real { cached_value, .. } = cell {
            *cached_value = value.to_owned().into();
        }
    }

    /// Refuses an operation that would build a vector of cells big enough to
    /// freeze the window, telling the user what to do instead.
    pub(crate) fn within_cell_budget(&mut self, cells: usize, cx: &mut Context<Self>) -> bool {
        if cells <= MAX_GRID_OPERATION_CELLS {
            return true;
        }
        let limit = grouped_digits(MAX_GRID_OPERATION_CELLS);
        self.set_status_message(
            format!(
                "選択範囲が大きすぎます (最大 {limit} セル)。テキストを横に開いて編集してください"
            ),
            cx,
        );
        false
    }

    pub(crate) fn set_status_message(
        &mut self,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.grid.status_message = Some(message.into());
        self.grid.status_message_is_sticky = false;
        cx.notify();
    }

    /// A message that describes a state the user is still in, rather than the
    /// operation they just tried. It survives moving the caret.
    pub(crate) fn set_sticky_status_message(
        &mut self,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.grid.status_message = Some(message.into());
        self.grid.status_message_is_sticky = true;
        cx.notify();
    }

    pub(crate) fn clear_status_message(&mut self) {
        self.grid.status_message = None;
        self.grid.status_message_is_sticky = false;
    }

    /// Takes away a message that was only about an operation the grid refused,
    /// which every move of the caret and every edit that went through makes
    /// stale.
    pub(crate) fn clear_stale_status_message(&mut self) {
        if !self.grid.status_message_is_sticky {
            self.grid.status_message = None;
        }
    }

    /// The address shown in the toolbar, e.g. `B3`. Row 1 is the header when
    /// there is one, so that the address matches the line of the file.
    pub(crate) fn cursor_address(&self) -> String {
        let row_number = self.grid.cursor.row.index() + usize::from(self.has_header_row());
        format!("{}{}", column_label(self.grid.cursor.col.get()), row_number)
    }

    ///// Selection /////

    pub(crate) fn select_cell(&mut self, cell: GridCell, extend: bool, cx: &mut Context<Self>) {
        if self.grid_is_empty() {
            return;
        }
        let cell = self.clamp_cell(cell);
        self.clear_stale_status_message();
        if extend {
            self.grid.anchor.get_or_insert(self.grid.cursor);
            if self.grid.selection_kind == GridSelectionKind::All {
                self.grid.selection_kind = GridSelectionKind::Cells;
            }
        } else {
            self.grid.anchor = None;
            self.grid.selection_kind = GridSelectionKind::Cells;
        }
        self.grid.cursor = cell;
        // The search bar counts hits relative to the caret, so every move of it
        // is a change of which hit is the active one.
        self.notify_search_of_caret(cx);
        cx.notify();
    }

    pub(crate) fn select_row(&mut self, row: GridRow, extend: bool, cx: &mut Context<Self>) {
        if self.grid_is_empty() {
            return;
        }
        let row = GridRow::from_index(
            row.index()
                .clamp(self.first_row_index(), self.last_row_index()),
        );
        // The caret keeps its column, the way a spreadsheet leaves the active
        // cell of a selected row where it was; the selection covers every
        // column regardless, through `GridSelectionKind::Rows`.
        let col = AnyColumn(self.grid.cursor.col.get().min(self.last_col_index()));
        self.clear_stale_status_message();
        if extend {
            self.grid.anchor.get_or_insert(self.grid.cursor);
        } else {
            self.grid.anchor = Some(GridCell::new(row, col));
        }
        self.grid.cursor = GridCell::new(row, col);
        self.grid.selection_kind = GridSelectionKind::Rows;
        self.notify_search_of_caret(cx);
        cx.notify();
    }

    pub(crate) fn select_column(&mut self, col: AnyColumn, extend: bool, cx: &mut Context<Self>) {
        if self.grid_is_empty() {
            return;
        }
        let col = AnyColumn(col.get().min(self.last_col_index()));
        let row = GridRow::from_index(
            self.grid
                .cursor
                .row
                .index()
                .clamp(self.first_row_index(), self.last_row_index()),
        );
        self.clear_stale_status_message();
        if extend {
            self.grid.anchor.get_or_insert(self.grid.cursor);
        } else {
            self.grid.anchor = Some(GridCell::new(row, col));
        }
        self.grid.cursor = GridCell::new(row, col);
        self.grid.selection_kind = GridSelectionKind::Columns;
        self.notify_search_of_caret(cx);
        cx.notify();
    }

    pub(crate) fn select_all_cells(&mut self, cx: &mut Context<Self>) {
        if self.grid_is_empty() {
            return;
        }
        self.clear_stale_status_message();
        self.grid.anchor = Some(self.grid.cursor);
        self.grid.selection_kind = GridSelectionKind::All;
        cx.notify();
    }

    ///// Movement /////

    /// How many rows the scrolling body shows, which is what one PageUp or
    /// PageDown covers and how far a jump has to scroll.
    ///
    /// Read from the list rather than from a [`BoundsProbe`], whose reading is
    /// only valid while a frame is being built: this runs from key handlers,
    /// which is after the frame that measured it, not during the next one.
    pub(crate) fn rows_per_page(&self) -> usize {
        let height = self.list_state.viewport_bounds().size.height;
        if height <= px(0.) {
            // Nothing has been laid out yet, so a page is whatever the grid used
            // to assume before it could measure one.
            return ROWS_PER_PAGE_BEFORE_LAYOUT;
        }
        let rows = (height / self.drawn_row_height()).floor() as usize;
        rows.max(1)
    }

    /// How tall a row of the body actually is, taken from one the list has
    /// drawn. `row_height` is the height of the line of text alone, and
    /// dividing the viewport by that counts rows that are not there: the table
    /// draws several pixels of border around every line.
    fn drawn_row_height(&self) -> Pixels {
        self.list_state
            .bounds_for_item(self.list_state.logical_scroll_top().item_ix)
            .map(|bounds| bounds.size.height)
            .filter(|height| *height > px(0.))
            .unwrap_or(self.row_height + ROW_BORDER_HEIGHT)
            .max(px(1.))
    }

    pub(crate) fn move_cursor(
        &mut self,
        motion: GridMotion,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.grid_is_empty() {
            return;
        }
        let first_row = self.first_row_index();
        let last_row = self.last_row_index();
        let last_col = self.last_col_index();
        let page = self.rows_per_page();
        let cursor = self.grid.cursor;
        let row = cursor.row.index();
        let col = cursor.col.get();

        let target = match motion {
            GridMotion::Up => (row.saturating_sub(1).max(first_row), col),
            GridMotion::Down => ((row + 1).min(last_row), col),
            GridMotion::Left => (row, col.saturating_sub(1)),
            GridMotion::Right => (row, (col + 1).min(last_col)),
            GridMotion::RowStart => (row, 0),
            GridMotion::RowEnd => (row, last_col),
            GridMotion::GridStart => (first_row, 0),
            GridMotion::GridEnd => (last_row, last_col),
            GridMotion::PageUp => (row.saturating_sub(page).max(first_row), col),
            GridMotion::PageDown => ((row + page).min(last_row), col),
            GridMotion::ColumnStart => (first_row, col),
            GridMotion::ColumnEnd => (last_row, col),
        };

        self.select_cell(
            GridCell::new(GridRow::from_index(target.0), AnyColumn(target.1)),
            extend,
            cx,
        );
        self.scroll_cursor_into_view(window, cx);
    }

    ///// Scrolling /////

    pub(crate) fn scroll_cursor_into_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.grid.cursor.row {
            GridRow::Body(display_row) => self.scroll_row_into_view(display_row.get()),
            // The header row is drawn in the table's sticky header rather than
            // in the list, so there is no item to reveal; the list still has to
            // go back to its top, or Ctrl+Home leaves the caret's row offscreen.
            GridRow::Header => self.list_state.scroll_to(ListOffset::default()),
        }
        self.scroll_column_into_view(window, cx);
    }

    /// Brings a row of the scrolling body into view, by its index.
    ///
    /// `ListState::scroll_to_reveal_item` works the distance out by adding up
    /// the heights of every row in between, and a virtualized list only knows
    /// those for the rows it has drawn: the rest count as nothing at all once a
    /// change of the list's width has dropped their height hints. Revealing a
    /// row far away then moves the view by the height of the few drawn rows
    /// rather than reaching the row. A row index, on the other hand, places the
    /// viewport whether or not anything has been measured.
    fn scroll_row_into_view(&mut self, row: usize) {
        let top = self.list_state.logical_scroll_top();
        // A viewport scrolled part way into its first row shows one row less of
        // the rows below it.
        let rows_shown = self.rows_per_page() - usize::from(top.offset_in_item > px(0.));
        let first_row_below = top.item_ix + rows_shown;
        if row >= top.item_ix && row < first_row_below {
            return;
        }
        // Walking one row off the bottom moves the view by that one row, the way
        // an arrow key scrolls a spreadsheet. Anything further away goes to the
        // top of the view instead: landing it at the bottom would take a row
        // height that is exact to the pixel, and a page key or a search hit that
        // stops just short of the view is worse than one that arrives at the
        // top. The list clamps its scroll to the end of its content, so the last
        // rows of the file still fill the view from the bottom.
        let item_ix = if row == first_row_below {
            row.saturating_sub(rows_shown.saturating_sub(1))
        } else {
            row
        };
        self.list_state.scroll_to(ListOffset {
            item_ix,
            offset_in_item: px(0.),
        });
    }

    /// Brings the caret's column inside the horizontally scrolling part of the
    /// table, which the pinned row identifier column is not part of.
    fn scroll_column_into_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        const PINNED_COLUMNS: usize = 1;
        let rem_size = window.rem_size();
        let widths = self.column_widths.widths.read(cx);
        // The table column of a data column is offset by the row identifier column.
        let table_col = self.grid.cursor.col.get() + PINNED_COLUMNS;
        if table_col + 1 > widths.cols() {
            return;
        }

        // `pinned_width(n)` is the total width of the first `n` columns, which
        // makes every column's offset and width a difference of two of them.
        let scrollable_start = widths.pinned_width(PINNED_COLUMNS, rem_size);
        let left = widths.pinned_width(table_col, rem_size) - scrollable_start;
        let width =
            widths.pinned_width(table_col + 1, rem_size) - widths.pinned_width(table_col, rem_size);

        let handle = &self
            .table_interaction_state
            .read(cx)
            .horizontal_scroll_handle;
        let viewport = handle.bounds().size.width;
        if viewport <= px(0.) {
            return;
        }
        let offset = handle.offset();
        let scrolled = -offset.x;
        let target = if left < scrolled {
            left
        } else if left + width > scrolled + viewport {
            (left + width - viewport).max(px(0.))
        } else {
            return;
        };
        handle.set_offset(Point {
            x: -target,
            y: offset.y,
        });
    }

    ///// Pointer /////

    pub(crate) fn on_cell_mouse_down(
        &mut self,
        cell: GridCell,
        button: MouseButton,
        modifiers_shift: bool,
        click_count: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match button {
            MouseButton::Left => {
                if click_count > 1 {
                    self.select_cell(cell, false, cx);
                    self.start_editing(CellEditMode::Edit, window, cx);
                    return;
                }
                self.commit_editing(window, cx);
                self.select_cell(cell, modifiers_shift, cx);
                self.grid.drag = Some(GridDrag::Cells);
            }
            MouseButton::Right => {
                if !self.selection_rect().contains(cell.row, cell.col) {
                    self.select_cell(cell, false, cx);
                }
                self.deploy_grid_context_menu(position, window, cx);
            }
            _ => {}
        }
    }

    pub(crate) fn on_cell_drag(&mut self, cell: GridCell, cx: &mut Context<Self>) {
        if self.grid.drag != Some(GridDrag::Cells) {
            return;
        }
        self.select_cell(cell, true, cx);
    }

    pub(crate) fn on_row_mouse_down(
        &mut self,
        row: GridRow,
        button: MouseButton,
        modifiers_shift: bool,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match button {
            MouseButton::Left => {
                self.commit_editing(window, cx);
                self.select_row(row, modifiers_shift, cx);
                self.grid.drag = Some(GridDrag::Rows);
            }
            MouseButton::Right => {
                if !self.selection_rect().contains_row(row) {
                    self.select_row(row, false, cx);
                }
                self.deploy_grid_context_menu(position, window, cx);
            }
            _ => {}
        }
    }

    pub(crate) fn on_row_drag(&mut self, row: GridRow, cx: &mut Context<Self>) {
        if self.grid.drag != Some(GridDrag::Rows) {
            return;
        }
        self.select_row(row, true, cx);
    }

    pub(crate) fn on_column_mouse_down(
        &mut self,
        col: AnyColumn,
        button: MouseButton,
        modifiers_shift: bool,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match button {
            MouseButton::Left => {
                self.commit_editing(window, cx);
                self.select_column(col, modifiers_shift, cx);
                self.grid.drag = Some(GridDrag::Columns);
            }
            MouseButton::Right => {
                if !self.selection_rect().contains_col(col) {
                    self.select_column(col, false, cx);
                }
                self.deploy_grid_context_menu(position, window, cx);
            }
            _ => {}
        }
    }

    pub(crate) fn on_column_drag(&mut self, col: AnyColumn, cx: &mut Context<Self>) {
        if self.grid.drag != Some(GridDrag::Columns) {
            return;
        }
        self.select_column(col, true, cx);
    }

    pub(crate) fn end_drag(&mut self) {
        self.grid.drag = None;
    }

    /// An overlay that paints nothing and only reports where its parent ended
    /// up laid out, which is the only way to learn a virtualized row's
    /// on screen rectangle.
    pub(crate) fn bounds_probe(&self, probe: &Rc<BoundsProbe>, cx: &Context<Self>) -> AnyElement {
        let probe = probe.clone();
        let generation = self.grid.render_generation;
        let view = cx.entity().downgrade();
        canvas(
            move |bounds, _window, cx| {
                if probe.record(generation, bounds) {
                    // The pane is gone when the window is closing between this
                    // frame's layout and its paint; nothing is left to redraw.
                    view.update(cx, |_, cx| cx.notify()).ok();
                }
            },
            |_bounds, _state, _window, _cx| {},
        )
        .absolute()
        .size_full()
        .into_any_element()
    }

    #[cfg(test)]
    pub(crate) fn cell_editor_is_visible(&self) -> bool {
        self.grid.cell_editor_was_drawn.get()
    }

    pub(crate) fn focus_cell_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.grid.cell_editor.focus_handle(cx);
        if !handle.is_focused(window) {
            window.focus(&handle, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_column_label() {
        assert_eq!(column_label(0), "A");
        assert_eq!(column_label(25), "Z");
        assert_eq!(column_label(26), "AA");
        assert_eq!(column_label(27), "AB");
        assert_eq!(column_label(51), "AZ");
        assert_eq!(column_label(53), "BB");
        assert_eq!(column_label(701), "ZZ");
        assert_eq!(column_label(702), "AAA");
    }

    #[test]
    fn test_grouped_digits() {
        assert_eq!(grouped_digits(0), "0");
        assert_eq!(grouped_digits(999), "999");
        assert_eq!(grouped_digits(1_000), "1,000");
        assert_eq!(grouped_digits(12_345), "12,345");
        assert_eq!(grouped_digits(MAX_GRID_OPERATION_CELLS), "200,000");
        assert_eq!(grouped_digits(1_234_567), "1,234,567");
    }

    #[test]
    fn test_grid_row_index_round_trip() {
        assert_eq!(GridRow::Header.index(), 0);
        assert_eq!(GridRow::Body(DisplayRow(0)).index(), 1);
        assert_eq!(GridRow::from_index(0), GridRow::Header);
        assert_eq!(GridRow::from_index(3), GridRow::Body(DisplayRow(2)));
        for index in 0..10 {
            assert_eq!(GridRow::from_index(index).index(), index);
        }
    }

    #[test]
    fn test_grid_rect_contains() {
        let rect = GridRect {
            first_row: 1,
            last_row: 3,
            first_col: 2,
            last_col: 4,
        };
        assert!(rect.contains(GridRow::Body(DisplayRow(0)), AnyColumn(2)));
        assert!(rect.contains(GridRow::Body(DisplayRow(2)), AnyColumn(4)));
        assert!(!rect.contains(GridRow::Header, AnyColumn(2)));
        assert!(!rect.contains(GridRow::Body(DisplayRow(0)), AnyColumn(5)));
        assert_eq!(rect.cols(), 2..5);
    }
}
