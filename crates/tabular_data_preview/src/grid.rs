//! The spreadsheet grid: what is selected, where the caret is, and how the
//! pointer moves both.
//!
//! The cell editor itself, the keyboard handlers and the structural operations
//! live in [`crate::grid_editing`]; this module owns the state they work on.

use std::{cell::Cell as MutableCell, collections::HashSet, ops::Range, rc::Rc, sync::Arc};

use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, Bounds, Entity, Focusable as _, ListOffset, Modifiers, MouseButton, Point,
    Subscription, TextStyleRefinement, canvas,
};
use settings::Settings as _;
use theme_settings::ThemeSettings;
use ui::{ContextMenu, prelude::*};

use crate::{
    TabularDataPreviewPane,
    edit_engine::RowRef,
    grid_editing::GridMenuTarget,
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
    /// The fill handle at the corner of the selection, dragged to copy the
    /// block along one axis.
    Fill {
        origin: GridRect,
    },
    /// The selection's own border, dragged to move the block (or, with the
    /// secondary modifier held when the button comes up, to copy it).
    Move {
        origin: GridRect,
        grabbed: GridCell,
    },
}

/// How a pointer or key event changes the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SelectionChange {
    /// No modifier: a single range, replacing everything selected before.
    Replace,
    /// Shift: the current range grows to reach the new cell.
    Extend,
    /// Ctrl (Cmd on macOS): a new range starts at the new cell and the ranges
    /// selected before it stay.
    Add,
}

impl SelectionChange {
    /// What the modifiers held during a click mean for the selection.
    pub(crate) fn from_modifiers(modifiers: Modifiers) -> Self {
        if modifiers.secondary() {
            SelectionChange::Add
        } else if modifiers.shift {
            SelectionChange::Extend
        } else {
            SelectionChange::Replace
        }
    }
}

/// One block of the selection: the corner it started from, the corner it
/// reaches, and what it is anchored to.
///
/// Kept as the two cells rather than as a resolved [`GridRect`] because a row or
/// column range covers whatever the table is wide or tall *now*, and the table's
/// shape changes under the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GridSelectionRange {
    pub(crate) anchor: GridCell,
    pub(crate) focus: GridCell,
    pub(crate) kind: GridSelectionKind,
}

/// What the grid says when an operation only makes sense on one block, which is
/// the wording Excel refuses the same operations with.
pub(crate) const MULTI_RANGE_REFUSAL: &str = "複数の選択範囲に対してはこの操作を実行できません。";

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

    pub(crate) fn rows(&self) -> Range<usize> {
        self.first_row..self.last_row + 1
    }

    pub(crate) fn cell_count(&self) -> usize {
        (self.last_row - self.first_row + 1).saturating_mul(self.last_col - self.first_col + 1)
    }

    pub(crate) fn row_count(&self) -> usize {
        self.last_row - self.first_row + 1
    }

    pub(crate) fn col_count(&self) -> usize {
        self.last_col - self.first_col + 1
    }

    pub(crate) fn contains_rect(&self, other: GridRect) -> bool {
        self.first_row <= other.first_row
            && self.last_row >= other.last_row
            && self.first_col <= other.first_col
            && self.last_col >= other.last_col
    }

    /// The smallest rectangle covering both, which is how the pieces of a
    /// multiple selection are reduced for the things that can only take one.
    pub(crate) fn union(self, other: GridRect) -> GridRect {
        GridRect {
            first_row: self.first_row.min(other.first_row),
            last_row: self.last_row.max(other.last_row),
            first_col: self.first_col.min(other.first_col),
            last_col: self.last_col.max(other.last_col),
        }
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
    /// The ranges Ctrl+click added before the current one, oldest first.
    ///
    /// The current range - the one Shift and the arrow keys grow, and the one
    /// the active cell lives in - is deliberately *not* in here: it stays
    /// `cursor` plus `anchor` plus `selection_kind`, so that everything that can
    /// only act on one rectangle keeps acting on the last one, the way a
    /// spreadsheet does.
    pub(crate) previous_ranges: Vec<GridSelectionRange>,
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
    /// Where a fill or a border drag would land if the button came up now.
    pub(crate) drag_preview: Option<GridRect>,
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
            previous_ranges: Vec::new(),
            editing: None,
            editing_target: None,
            editing_cell: None,
            status_message: None,
            status_message_is_sticky: false,
            search: GridSearchState::default(),
            drag: None,
            drag_preview: None,
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
        let clamped: Vec<GridSelectionRange> = self
            .grid
            .previous_ranges
            .iter()
            .map(|range| GridSelectionRange {
                anchor: self.clamp_cell(range.anchor),
                focus: self.clamp_cell(range.focus),
                kind: range.kind,
            })
            .collect();
        self.grid.previous_ranges = clamped;
    }

    /// The rectangle one block of the selection covers, in grid coordinates.
    fn range_rect(&self, range: GridSelectionRange) -> GridRect {
        let (focus_row, anchor_row) = (range.focus.row.index(), range.anchor.row.index());
        let (focus_col, anchor_col) = (range.focus.col.get(), range.anchor.col.get());

        let mut rect = GridRect {
            first_row: focus_row.min(anchor_row),
            last_row: focus_row.max(anchor_row),
            first_col: focus_col.min(anchor_col),
            last_col: focus_col.max(anchor_col),
        };

        match range.kind {
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

    /// The block the active cell is in, which is the one every operation that
    /// only makes sense on a single rectangle - pasting, inserting rows or
    /// columns, pinning the search - works on.
    pub(crate) fn current_selection_range(&self) -> GridSelectionRange {
        GridSelectionRange {
            anchor: self.grid.anchor.unwrap_or(self.grid.cursor),
            focus: self.grid.cursor,
            kind: self.grid.selection_kind,
        }
    }

    pub(crate) fn selection_rect(&self) -> GridRect {
        self.range_rect(self.current_selection_range())
    }

    /// Every selected block, in the order they were selected, the current one
    /// last.
    pub(crate) fn selection_rects(&self) -> Vec<GridRect> {
        self.selection_blocks()
            .into_iter()
            .map(|(rect, _kind)| rect)
            .collect()
    }

    /// The same, with what each block is anchored to, which the colors need in
    /// order to keep a whole row or column as one entry.
    pub(crate) fn selection_blocks(&self) -> Vec<(GridRect, GridSelectionKind)> {
        let mut blocks: Vec<(GridRect, GridSelectionKind)> = self
            .grid
            .previous_ranges
            .iter()
            .map(|range| (self.range_rect(*range), range.kind))
            .collect();
        blocks.push((self.selection_rect(), self.grid.selection_kind));
        blocks
    }

    pub(crate) fn has_multiple_selection_ranges(&self) -> bool {
        !self.grid.previous_ranges.is_empty()
    }

    /// The smallest rectangle covering the whole selection. Used where only one
    /// rectangle can be expressed at all.
    pub(crate) fn selection_bounding_rect(&self) -> GridRect {
        self.selection_rects()
            .into_iter()
            .reduce(GridRect::union)
            .unwrap_or_else(|| self.selection_rect())
    }

    /// How many cells the selection names, counting a cell two blocks share
    /// twice.
    ///
    /// This is what the cell budget is measured against, before any of those
    /// cells is resolved: a selection far taller than the table would otherwise
    /// cost a pass over every row it names before anything noticed its size.
    /// Counting an overlap twice only ever refuses earlier than necessary, and
    /// the writes themselves are deduplicated.
    pub(crate) fn selection_area(&self) -> usize {
        self.selection_rects()
            .iter()
            .map(GridRect::cell_count)
            .fold(0usize, usize::saturating_add)
    }

    /// Whether any block of the selection covers `cell`.
    ///
    /// Written without [`Self::selection_rects`] because this runs once per
    /// drawn cell, and the single block case - which is nearly every frame -
    /// must not allocate a vector to answer it.
    pub(crate) fn selection_contains(&self, row: GridRow, col: AnyColumn) -> bool {
        self.any_selected_block(|rect| rect.contains(row, col))
    }

    pub(crate) fn selection_contains_row(&self, row: GridRow) -> bool {
        self.any_selected_block(|rect| rect.contains_row(row))
    }

    pub(crate) fn selection_contains_col(&self, col: AnyColumn) -> bool {
        self.any_selected_block(|rect| rect.contains_col(col))
    }

    fn any_selected_block(&self, covers: impl Fn(GridRect) -> bool) -> bool {
        covers(self.selection_rect())
            || self
                .grid
                .previous_ranges
                .iter()
                .any(|range| covers(self.range_rect(*range)))
    }

    pub(crate) fn selected_rows(&self) -> Vec<GridRow> {
        let rect = self.selection_rect();
        (rect.first_row..=rect.last_row)
            .map(GridRow::from_index)
            .collect()
    }

    /// Every row any block of the selection covers, top to bottom and without
    /// repeats, which is what the row operations and the colors act on.
    pub(crate) fn all_selected_rows(&self) -> Vec<GridRow> {
        let mut rows: Vec<usize> = self
            .selection_rects()
            .iter()
            .flat_map(|rect| rect.rows())
            .collect();
        rows.sort_unstable();
        rows.dedup();
        rows.into_iter().map(GridRow::from_index).collect()
    }

    pub(crate) fn all_selected_row_refs(&self) -> Vec<RowRef> {
        self.all_selected_rows()
            .into_iter()
            .filter_map(|row| self.row_ref(row))
            .collect()
    }

    /// Every column any block of the selection covers, left to right and
    /// without repeats.
    pub(crate) fn all_selected_cols(&self) -> Vec<AnyColumn> {
        let mut cols: Vec<usize> = self
            .selection_rects()
            .iter()
            .flat_map(|rect| rect.cols())
            .collect();
        cols.sort_unstable();
        cols.dedup();
        cols.into_iter().map(AnyColumn).collect()
    }

    /// Every cell of the selection, resolved to data coordinates and without
    /// repeats, so that overlapping blocks are written once rather than twice.
    pub(crate) fn all_selected_cells(&self) -> Vec<(RowRef, AnyColumn)> {
        let mut cells: Vec<(RowRef, AnyColumn)> = Vec::new();
        let mut seen = HashSet::new();
        for rect in self.selection_rects() {
            for row_index in rect.rows() {
                let Some(row_ref) = self.row_ref(GridRow::from_index(row_index)) else {
                    continue;
                };
                for col in rect.cols() {
                    if seen.insert((row_ref, col)) {
                        cells.push((row_ref, AnyColumn(col)));
                    }
                }
            }
        }
        cells
    }

    /// Refuses an operation that Excel also refuses on a multiple selection.
    pub(crate) fn refuse_on_multiple_ranges(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.has_multiple_selection_ranges() {
            return false;
        }
        self.set_status_message(MULTI_RANGE_REFUSAL, cx);
        true
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

    /// Puts the block the active cell is in away, so that a new one can start
    /// beside it.
    fn push_current_range(&mut self) {
        let range = self.current_selection_range();
        self.grid.previous_ranges.push(range);
    }

    fn reset_to_single_range(&mut self) {
        self.grid.previous_ranges.clear();
    }

    pub(crate) fn select_cell(&mut self, cell: GridCell, extend: bool, cx: &mut Context<Self>) {
        let change = if extend {
            SelectionChange::Extend
        } else {
            SelectionChange::Replace
        };
        self.change_cell_selection(cell, change, cx);
    }

    pub(crate) fn change_cell_selection(
        &mut self,
        cell: GridCell,
        change: SelectionChange,
        cx: &mut Context<Self>,
    ) {
        if self.grid_is_empty() {
            return;
        }
        let cell = self.clamp_cell(cell);
        self.clear_stale_status_message();
        match change {
            SelectionChange::Extend => {
                self.grid.anchor.get_or_insert(self.grid.cursor);
                if self.grid.selection_kind == GridSelectionKind::All {
                    self.grid.selection_kind = GridSelectionKind::Cells;
                }
            }
            SelectionChange::Add => {
                self.push_current_range();
                self.grid.anchor = None;
                self.grid.selection_kind = GridSelectionKind::Cells;
            }
            SelectionChange::Replace => {
                self.reset_to_single_range();
                self.grid.anchor = None;
                self.grid.selection_kind = GridSelectionKind::Cells;
            }
        }
        self.grid.cursor = cell;
        // The search bar counts hits relative to the caret, so every move of it
        // is a change of which hit is the active one.
        self.notify_search_of_caret(cx);
        cx.notify();
    }

    pub(crate) fn select_row(&mut self, row: GridRow, extend: bool, cx: &mut Context<Self>) {
        let change = if extend {
            SelectionChange::Extend
        } else {
            SelectionChange::Replace
        };
        self.change_row_selection(row, change, cx);
    }

    pub(crate) fn change_row_selection(
        &mut self,
        row: GridRow,
        change: SelectionChange,
        cx: &mut Context<Self>,
    ) {
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
        match change {
            SelectionChange::Extend => {
                self.grid.anchor.get_or_insert(self.grid.cursor);
            }
            SelectionChange::Add => {
                self.push_current_range();
                self.grid.anchor = Some(GridCell::new(row, col));
            }
            SelectionChange::Replace => {
                self.reset_to_single_range();
                self.grid.anchor = Some(GridCell::new(row, col));
            }
        }
        self.grid.cursor = GridCell::new(row, col);
        self.grid.selection_kind = GridSelectionKind::Rows;
        self.notify_search_of_caret(cx);
        cx.notify();
    }

    pub(crate) fn select_column(&mut self, col: AnyColumn, extend: bool, cx: &mut Context<Self>) {
        let change = if extend {
            SelectionChange::Extend
        } else {
            SelectionChange::Replace
        };
        self.change_column_selection(col, change, cx);
    }

    pub(crate) fn change_column_selection(
        &mut self,
        col: AnyColumn,
        change: SelectionChange,
        cx: &mut Context<Self>,
    ) {
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
        match change {
            SelectionChange::Extend => {
                self.grid.anchor.get_or_insert(self.grid.cursor);
            }
            SelectionChange::Add => {
                self.push_current_range();
                self.grid.anchor = Some(GridCell::new(row, col));
            }
            SelectionChange::Replace => {
                self.reset_to_single_range();
                self.grid.anchor = Some(GridCell::new(row, col));
            }
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
        self.reset_to_single_range();
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
    pub(crate) fn scroll_column_into_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        modifiers: Modifiers,
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
                let change = SelectionChange::from_modifiers(modifiers);
                self.change_cell_selection(cell, change, cx);
                self.grid.drag = Some(GridDrag::Cells);
            }
            MouseButton::Right => {
                if !self.selection_contains(cell.row, cell.col) {
                    self.select_cell(cell, false, cx);
                }
                self.deploy_grid_context_menu(GridMenuTarget::Cell, position, window, cx);
            }
            _ => {}
        }
    }

    pub(crate) fn on_cell_drag(&mut self, cell: GridCell, cx: &mut Context<Self>) {
        match self.grid.drag {
            Some(GridDrag::Cells) => self.select_cell(cell, true, cx),
            Some(GridDrag::Fill { origin }) => self.update_fill_preview(origin, cell, cx),
            Some(GridDrag::Move { origin, grabbed }) => {
                self.update_move_preview(origin, grabbed, cell, cx)
            }
            _ => {}
        }
    }

    pub(crate) fn on_row_mouse_down(
        &mut self,
        row: GridRow,
        button: MouseButton,
        modifiers: Modifiers,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match button {
            MouseButton::Left => {
                self.commit_editing(window, cx);
                let change = SelectionChange::from_modifiers(modifiers);
                self.change_row_selection(row, change, cx);
                self.grid.drag = Some(GridDrag::Rows);
            }
            MouseButton::Right => {
                if !self.row_selection_survives_a_right_click_on(row) {
                    self.select_row(row, false, cx);
                }
                self.deploy_grid_context_menu(GridMenuTarget::RowHeader, position, window, cx);
            }
            _ => {}
        }
    }

    /// Whether a right click on `row`'s heading should keep the selection it
    /// found rather than replace it with that one row.
    ///
    /// A spreadsheet keeps it only when the rows were picked as rows: several
    /// rows selected by their headings are meant to be deleted or colored
    /// together, and replacing that would undo the user's work. A cell range
    /// that merely happens to cross the row says nothing about the row, so the
    /// heading menu's row operations get the row that was actually clicked.
    fn row_selection_survives_a_right_click_on(&self, row: GridRow) -> bool {
        self.selection_contains_row(row)
            && self.selection_blocks().iter().any(|(rect, kind)| {
                matches!(kind, GridSelectionKind::Rows | GridSelectionKind::All)
                    && rect.contains_row(row)
            })
    }

    /// The same for a column heading, with one more condition: the column
    /// heading's menu can open that column's filter popover, and the popover is
    /// registered by the caret column's own button and by no other. Keeping a
    /// selection whose caret sits on a different column would leave that entry
    /// pointing at the wrong column, so the caret has to come along.
    fn column_selection_survives_a_right_click_on(&self, col: AnyColumn) -> bool {
        self.grid.cursor.col == col
            && self.selection_contains_col(col)
            && self.selection_blocks().iter().any(|(rect, kind)| {
                matches!(kind, GridSelectionKind::Columns | GridSelectionKind::All)
                    && rect.contains_col(col)
            })
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
        modifiers: Modifiers,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match button {
            MouseButton::Left => {
                self.commit_editing(window, cx);
                let change = SelectionChange::from_modifiers(modifiers);
                self.change_column_selection(col, change, cx);
                self.grid.drag = Some(GridDrag::Columns);
            }
            MouseButton::Right => {
                if !self.column_selection_survives_a_right_click_on(col) {
                    self.select_column(col, false, cx);
                }
                self.deploy_grid_context_menu(
                    GridMenuTarget::ColumnHeader(col),
                    position,
                    window,
                    cx,
                );
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

    pub(crate) fn end_drag(&mut self, modifiers: Modifiers, cx: &mut Context<Self>) {
        let drag = self.grid.drag.take();
        let preview = self.grid.drag_preview.take();
        match (drag, preview) {
            (Some(GridDrag::Fill { origin }), Some(preview)) => {
                self.commit_fill(origin, preview, modifiers.secondary(), cx);
            }
            (Some(GridDrag::Move { origin, .. }), Some(preview)) => {
                self.commit_block_move(origin, preview, modifiers.secondary(), cx);
            }
            _ => {}
        }
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
    use crate::grid_editing::tests::open_grid;
    use gpui::TestAppContext;

    fn body_cell(row: usize, col: usize) -> GridCell {
        GridCell::new(GridRow::Body(DisplayRow(row)), AnyColumn(col))
    }

    fn rect(first_row: usize, last_row: usize, first_col: usize, last_col: usize) -> GridRect {
        GridRect {
            first_row,
            last_row,
            first_col,
            last_col,
        }
    }

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

    #[test]
    fn test_grid_rect_union() {
        assert_eq!(rect(1, 2, 0, 1).union(rect(5, 6, 3, 4)), rect(1, 6, 0, 4));
        assert_eq!(rect(1, 2, 0, 1).union(rect(1, 2, 0, 1)), rect(1, 2, 0, 1));
    }

    #[test]
    fn test_selection_change_reads_the_modifiers() {
        assert_eq!(
            SelectionChange::from_modifiers(Modifiers::none()),
            SelectionChange::Replace
        );
        assert_eq!(
            SelectionChange::from_modifiers(Modifiers::shift()),
            SelectionChange::Extend
        );
        assert_eq!(
            SelectionChange::from_modifiers(Modifiers::secondary_key()),
            SelectionChange::Add
        );
        // Ctrl+Shift+click keeps adding rather than extending, as in Excel.
        let both = Modifiers {
            shift: true,
            ..Modifiers::secondary_key()
        };
        assert_eq!(SelectionChange::from_modifiers(both), SelectionChange::Add);
    }

    #[gpui::test]
    async fn test_ctrl_click_adds_a_block_and_keeps_the_others(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("a,b,c\n1,2,3\n4,5,6\n7,8,9", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.change_cell_selection(body_cell(0, 0), SelectionChange::Replace, cx);
            pane.change_cell_selection(body_cell(1, 0), SelectionChange::Extend, cx);
            pane.change_cell_selection(body_cell(2, 2), SelectionChange::Add, cx);
        });

        pane.read_with(&cx, |pane, _| {
            assert!(pane.has_multiple_selection_ranges());
            assert_eq!(
                pane.selection_rects(),
                vec![rect(1, 2, 0, 0), rect(3, 3, 2, 2)]
            );
            // The last block is what a single rectangle reader sees, and the
            // active cell is in it.
            assert_eq!(pane.selection_rect(), rect(3, 3, 2, 2));
            assert_eq!(pane.grid.cursor, body_cell(2, 2));
            assert!(pane.selection_contains(GridRow::Body(DisplayRow(0)), AnyColumn(0)));
            assert!(pane.selection_contains(GridRow::Body(DisplayRow(2)), AnyColumn(2)));
            assert!(!pane.selection_contains(GridRow::Body(DisplayRow(0)), AnyColumn(2)));
            assert_eq!(pane.selection_bounding_rect(), rect(1, 3, 0, 2));
        });
    }

    #[gpui::test]
    async fn test_shift_grows_only_the_added_block(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("a,b,c\n1,2,3\n4,5,6\n7,8,9", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.change_cell_selection(body_cell(0, 0), SelectionChange::Replace, cx);
            pane.change_cell_selection(body_cell(2, 1), SelectionChange::Add, cx);
            pane.change_cell_selection(body_cell(2, 2), SelectionChange::Extend, cx);
        });

        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.selection_rects(),
                vec![rect(1, 1, 0, 0), rect(3, 3, 1, 2)]
            );
        });
    }

    #[gpui::test]
    async fn test_a_plain_click_collapses_back_to_one_block(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("a,b,c\n1,2,3\n4,5,6", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.change_cell_selection(body_cell(0, 0), SelectionChange::Replace, cx);
            pane.change_cell_selection(body_cell(1, 2), SelectionChange::Add, cx);
            pane.change_cell_selection(body_cell(1, 1), SelectionChange::Replace, cx);
        });

        pane.read_with(&cx, |pane, _| {
            assert!(!pane.has_multiple_selection_ranges());
            assert_eq!(pane.selection_rects(), vec![rect(2, 2, 1, 1)]);
        });
    }

    /// Moving the caret with the arrow keys is a plain click's worth of change:
    /// the added blocks go, the way they do in a spreadsheet.
    #[gpui::test]
    async fn test_moving_the_caret_drops_the_added_blocks(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("a,b,c\n1,2,3\n4,5,6", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.change_cell_selection(body_cell(0, 0), SelectionChange::Replace, cx);
            pane.change_cell_selection(body_cell(1, 2), SelectionChange::Add, cx);
        });
        pane.read_with(&cx, |pane, _| assert!(pane.has_multiple_selection_ranges()));

        cx.simulate_keystrokes("up");
        pane.read_with(&cx, |pane, _| {
            assert!(!pane.has_multiple_selection_ranges());
            assert_eq!(pane.grid.cursor, body_cell(0, 2));
        });
    }

    #[gpui::test]
    async fn test_ctrl_clicking_row_and_column_headers_adds_them(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("a,b,c\n1,2,3\n4,5,6", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.change_row_selection(GridRow::Body(DisplayRow(0)), SelectionChange::Replace, cx);
            pane.change_column_selection(AnyColumn(2), SelectionChange::Add, cx);
        });

        pane.read_with(&cx, |pane, _| {
            // The row covers every column, the column every row.
            assert_eq!(
                pane.selection_rects(),
                vec![rect(1, 1, 0, 2), rect(0, 2, 2, 2)]
            );
            assert_eq!(
                pane.all_selected_rows(),
                vec![
                    GridRow::Header,
                    GridRow::Body(DisplayRow(0)),
                    GridRow::Body(DisplayRow(1)),
                ]
            );
            assert_eq!(
                pane.all_selected_cols(),
                vec![AnyColumn(0), AnyColumn(1), AnyColumn(2)]
            );
        });
    }

    /// Two blocks sharing cells must not hand those cells to an operation
    /// twice: a double write would take two undo steps' worth of edits and
    /// count twice against the cell budget.
    #[gpui::test]
    async fn test_overlapping_blocks_list_every_cell_once(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("a,b,c\n1,2,3\n4,5,6\n7,8,9", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.change_cell_selection(body_cell(0, 0), SelectionChange::Replace, cx);
            pane.change_cell_selection(body_cell(1, 1), SelectionChange::Extend, cx);
            pane.change_cell_selection(body_cell(1, 1), SelectionChange::Add, cx);
            pane.change_cell_selection(body_cell(2, 2), SelectionChange::Extend, cx);
        });

        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.selection_rects(),
                vec![rect(1, 2, 0, 1), rect(2, 3, 1, 2)]
            );
            let cells = pane.all_selected_cells();
            let unique: HashSet<_> = cells.iter().collect();
            assert_eq!(cells.len(), unique.len(), "{cells:?} repeats a cell");
            // Four cells per block, with the one at their crossing shared.
            assert_eq!(cells.len(), 7);
        });
    }

    /// A parse that shrinks the table has to pull the added blocks back inside
    /// it, or an operation would resolve rows the table no longer has.
    #[gpui::test]
    async fn test_a_smaller_table_clamps_the_added_blocks(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("a,b,c\n1,2,3", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.change_cell_selection(body_cell(0, 0), SelectionChange::Replace, cx);
            pane.change_cell_selection(body_cell(0, 2), SelectionChange::Add, cx);
            pane.grid.previous_ranges = vec![GridSelectionRange {
                anchor: body_cell(40, 9),
                focus: body_cell(50, 12),
                kind: GridSelectionKind::Cells,
            }];
            pane.clamp_cursor();
        });

        pane.read_with(&cx, |pane, _| {
            // The fixture has one body row, so every corner lands on it.
            assert_eq!(
                pane.selection_rects(),
                vec![rect(1, 1, 2, 2), rect(1, 1, 2, 2)]
            );
        });
    }
}
