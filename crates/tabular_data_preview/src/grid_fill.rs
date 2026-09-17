//! Excel's two pointer gestures on the edge of a selection: the fill handle at
//! its bottom right corner, which stretches the block along one axis, and the
//! border itself, which moves the block (or copies it with the secondary
//! modifier held).
//!
//! Both are single block gestures - the handle and the grabbable border are not
//! drawn at all while several blocks are selected - and both land as one batch
//! of cell writes, so that one gesture is one undo step.
//!
//! The values a fill produces are worked out over the grid's own order, top to
//! bottom and left to right, whichever way the pointer went: a series is a
//! property of that order, so dragging upwards out of `1, 2, 3` extrapolates
//! backwards to `0, -1, -2` rather than continuing to `4, 5, 6`.

use gpui::{AnyElement, CursorStyle, MouseButton, MouseDownEvent, actions};
use ui::{div, prelude::*};

use crate::{
    TabularDataPreviewPane,
    grid::{GridCell, GridDrag, GridRect, GridRow, GridSelectionKind},
    types::AnyColumn,
};

actions!(
    tabular_data,
    [
        /// 選択範囲の先頭行を範囲全体にコピーします。
        FillDown,
        /// 選択範囲の先頭列を範囲全体にコピーします。
        FillRight,
    ]
);

/// The side of the fill handle square, which is what the pointer has to find at
/// the corner of the selection.
const FILL_HANDLE_SIZE: Pixels = px(6.);

/// How wide the band along the selection's border is that grabs the block
/// instead of starting a new selection.
const MOVE_BORDER_WIDTH: Pixels = px(3.);

/// How thick the outline is that shows where a fill or a move would land.
const DRAG_PREVIEW_WIDTH: Pixels = px(2.);

/// A number as a cell spells it, kept as an integer scaled by its decimals so
/// that extending a series never has to format a float back: `12.50` is `1250`
/// with two decimals, and `12.50 + 0.25` stays `12.75` rather than becoming
/// `12.749999999999998`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CellNumber {
    scaled: i128,
    decimals: u32,
}

/// Longest run of digits a value may hold and still be read as a number. Past
/// this the scaled form would overflow, and a cell holding a 40 digit account
/// number is not a series anyway.
const MAX_NUMBER_DIGITS: usize = 30;

impl CellNumber {
    fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        let (negative, digits) = match value.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, value.strip_prefix('+').unwrap_or(value)),
        };
        let (integer, fraction) = match digits.split_once('.') {
            Some((integer, fraction)) => (integer, fraction),
            None => (digits, ""),
        };
        // A lone sign, a bare `.5` and a trailing `5.` are all left to the
        // copying rule rather than rewritten into a spelling of their own.
        if integer.is_empty() || (digits.contains('.') && fraction.is_empty()) {
            return None;
        }
        if integer.len() + fraction.len() > MAX_NUMBER_DIGITS {
            return None;
        }
        let mut scaled: i128 = 0;
        for byte in integer.bytes().chain(fraction.bytes()) {
            if !byte.is_ascii_digit() {
                return None;
            }
            scaled = scaled
                .checked_mul(10)?
                .checked_add(i128::from(byte - b'0'))?;
        }
        Some(Self {
            scaled: if negative { -scaled } else { scaled },
            decimals: u32::try_from(fraction.len()).ok()?,
        })
    }

    /// The same number written with more decimals, so that `1` and `1.5` can be
    /// compared and stepped in one scale.
    fn rescaled(self, decimals: u32) -> Option<Self> {
        let factor = 10i128.checked_pow(decimals.checked_sub(self.decimals)?)?;
        Some(Self {
            scaled: self.scaled.checked_mul(factor)?,
            decimals,
        })
    }

    fn to_text(self) -> String {
        let sign = if self.scaled < 0 { "-" } else { "" };
        let magnitude = self.scaled.unsigned_abs();
        if self.decimals == 0 {
            return format!("{sign}{magnitude}");
        }
        let divisor = 10u128.pow(self.decimals);
        let width = self.decimals as usize;
        format!(
            "{sign}{}.{:0width$}",
            magnitude / divisor,
            magnitude % divisor
        )
    }
}

/// The numbers `source` spells out, brought to one scale, together with the
/// step between them. `None` when a value is not a number, or when the steps
/// are uneven - neither is a series to extend.
fn numeric_series(source: &[String]) -> Option<(Vec<CellNumber>, i128)> {
    let parsed: Vec<CellNumber> = source
        .iter()
        .map(|value| CellNumber::parse(value))
        .collect::<Option<_>>()?;
    let decimals = parsed.iter().map(|number| number.decimals).max()?;
    let numbers: Vec<CellNumber> = parsed
        .iter()
        .map(|number| number.rescaled(decimals))
        .collect::<Option<_>>()?;

    let mut step = None;
    for pair in numbers.windows(2) {
        let [previous, next] = pair else {
            continue;
        };
        let difference = next.scaled.checked_sub(previous.scaled)?;
        match step {
            None => step = Some(difference),
            Some(step) if step == difference => {}
            Some(_) => return None,
        }
    }
    // A single value has no step of its own; the caller supplies Excel's 1.
    Some((numbers, step.unwrap_or(0)))
}

/// The values a fill writes, in the order it writes them: the first is the cell
/// next to the source block in the direction the pointer went.
///
/// `source` is always in the grid's own order, whichever way the fill goes.
/// `invert` is Excel's Ctrl, which flips the default both ways: a single number
/// becomes a counted series, and a series is stamped as a plain copy.
pub(crate) fn fill_values(
    source: &[String],
    count: usize,
    forward: bool,
    invert: bool,
) -> Vec<String> {
    if source.is_empty() || count == 0 {
        return Vec::new();
    }
    if let Some(values) = numeric_fill(source, count, forward, invert) {
        return values;
    }
    // Repeating stamps the whole block again and again, which going backwards
    // means its last value lands first.
    (0..count)
        .map(|offset| {
            let index = offset % source.len();
            let index = if forward {
                index
            } else {
                source.len() - 1 - index
            };
            source.get(index).cloned().unwrap_or_default()
        })
        .collect()
}

fn numeric_fill(
    source: &[String],
    count: usize,
    forward: bool,
    invert: bool,
) -> Option<Vec<String>> {
    let (numbers, step) = numeric_series(source)?;
    let single = numbers.len() == 1;
    let extend = if single { invert } else { !invert };
    if !extend {
        return None;
    }
    let decimals = numbers.first()?.decimals;
    let step = if single {
        10i128.checked_pow(decimals)?
    } else {
        step
    };
    let anchor = if forward {
        numbers.last()?.scaled
    } else {
        numbers.first()?.scaled
    };

    let mut values = Vec::with_capacity(count);
    for offset in 1..=count {
        let delta = step.checked_mul(i128::try_from(offset).ok()?)?;
        let scaled = if forward {
            anchor.checked_add(delta)?
        } else {
            anchor.checked_sub(delta)?
        };
        values.push(CellNumber { scaled, decimals }.to_text());
    }
    Some(values)
}

/// Which way a fill went and how far, read off the block it started on and the
/// rectangle the pointer stretched it to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FillAxis {
    vertical: bool,
    forward: bool,
    extent: usize,
}

fn fill_axis(origin: GridRect, filled: GridRect) -> Option<FillAxis> {
    let (vertical, forward, extent) = if filled.last_row > origin.last_row {
        (true, true, filled.last_row - origin.last_row)
    } else if filled.first_row < origin.first_row {
        (true, false, origin.first_row - filled.first_row)
    } else if filled.last_col > origin.last_col {
        (false, true, filled.last_col - origin.last_col)
    } else if filled.first_col < origin.first_col {
        (false, false, origin.first_col - filled.first_col)
    } else {
        return None;
    };
    Some(FillAxis {
        vertical,
        forward,
        extent,
    })
}

/// The rectangle a fill of `origin` reaching `target` covers: the block itself
/// plus the run it was stretched by, along the one axis the pointer moved
/// furthest on, which is the axis Excel picks too.
fn stretched_rect(origin: GridRect, target: GridCell) -> GridRect {
    let row = target.row.index();
    let col = target.col.get();
    let below = row.saturating_sub(origin.last_row);
    let above = origin.first_row.saturating_sub(row);
    let right = col.saturating_sub(origin.last_col);
    let left = origin.first_col.saturating_sub(col);
    let vertical = below.max(above);
    let horizontal = right.max(left);

    if vertical == 0 && horizontal == 0 {
        return origin;
    }
    if vertical >= horizontal {
        GridRect {
            first_row: origin.first_row.min(row),
            last_row: origin.last_row.max(row),
            ..origin
        }
    } else {
        GridRect {
            first_col: origin.first_col.min(col),
            last_col: origin.last_col.max(col),
            ..origin
        }
    }
}

/// `start` moved by `shift` and held between `lowest` and `highest`, which is
/// how a dragged block stays whole inside the table.
fn shifted_start(start: usize, shift: isize, lowest: usize, highest: usize) -> usize {
    let highest = highest.max(lowest);
    (start as isize + shift).clamp(lowest as isize, highest as isize) as usize
}

impl TabularDataPreviewPane {
    ///// Gesture availability /////

    /// The cell whose corner carries the fill handle: the bottom right of a
    /// single block selection.
    ///
    /// `None` while several blocks are selected, because there is no one corner
    /// to drag, and while a cell is open for input, where the handle would sit
    /// under the overlay editor.
    pub(crate) fn fill_handle_cell(&self) -> Option<GridCell> {
        if self.grid_is_empty()
            || self.grid.editing.is_some()
            || self.has_multiple_selection_ranges()
            || self.grid.drag.is_some()
        {
            return None;
        }
        let rect = self.selection_rect();
        Some(GridCell::new(
            GridRow::from_index(rect.last_row),
            AnyColumn(rect.last_col),
        ))
    }

    /// Whether the selection's border can be grabbed to move the block.
    ///
    /// Off while the table is sorted or filtered: the rows on screen are not
    /// the rows of the file, so "put this block two rows further down" has no
    /// one meaning. The fill handle stays on, because a fill writes the rows
    /// the user can see, which is what Excel does with a filtered list too.
    pub(crate) fn block_move_is_available(&self) -> bool {
        !self.grid_is_empty()
            && self.grid.editing.is_none()
            && self.grid.drag.is_none()
            && !self.has_multiple_selection_ranges()
            && self.engine.d2d_mapping().is_identity()
    }

    ///// Pointer /////

    fn start_fill_drag(&mut self, cx: &mut Context<Self>) {
        let origin = self.selection_rect();
        self.grid.drag = Some(GridDrag::Fill { origin });
        self.grid.drag_preview = Some(origin);
        cx.notify();
    }

    fn start_move_drag(&mut self, grabbed: GridCell, cx: &mut Context<Self>) {
        let origin = self.selection_rect();
        self.grid.drag = Some(GridDrag::Move { origin, grabbed });
        self.grid.drag_preview = Some(origin);
        cx.notify();
    }

    pub(crate) fn update_fill_preview(
        &mut self,
        origin: GridRect,
        target: GridCell,
        cx: &mut Context<Self>,
    ) {
        let preview = stretched_rect(origin, self.clamp_cell(target));
        if self.grid.drag_preview != Some(preview) {
            self.grid.drag_preview = Some(preview);
            cx.notify();
        }
    }

    pub(crate) fn update_move_preview(
        &mut self,
        origin: GridRect,
        grabbed: GridCell,
        target: GridCell,
        cx: &mut Context<Self>,
    ) {
        let target = self.clamp_cell(target);
        let row_shift = target.row.index() as isize - grabbed.row.index() as isize;
        let col_shift = target.col.get() as isize - grabbed.col.get() as isize;
        let preview = self.shifted_rect(origin, row_shift, col_shift);
        if self.grid.drag_preview != Some(preview) {
            self.grid.drag_preview = Some(preview);
            cx.notify();
        }
    }

    fn shifted_rect(&self, rect: GridRect, row_shift: isize, col_shift: isize) -> GridRect {
        let first_row = shifted_start(
            rect.first_row,
            row_shift,
            self.first_row_index(),
            self.last_row_index()
                .saturating_sub(rect.row_count().saturating_sub(1)),
        );
        let first_col = shifted_start(
            rect.first_col,
            col_shift,
            0,
            self.last_col_index()
                .saturating_sub(rect.col_count().saturating_sub(1)),
        );
        GridRect {
            first_row,
            last_row: first_row + rect.row_count() - 1,
            first_col,
            last_col: first_col + rect.col_count() - 1,
        }
    }

    ///// Fill /////

    /// Writes the stretched part of a fill, reading the source values in the
    /// order the grid shows them so that a filtered table is filled down the
    /// rows the user can see.
    pub(crate) fn commit_fill(
        &mut self,
        origin: GridRect,
        filled: GridRect,
        invert: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(axis) = fill_axis(origin, filled) else {
            return;
        };
        let lines: Vec<usize> = if axis.vertical {
            origin.cols().collect()
        } else {
            origin.rows().collect()
        };
        if !self.within_cell_budget(lines.len().saturating_mul(axis.extent), cx) {
            return;
        }

        let mut cells = Vec::with_capacity(lines.len() * axis.extent);
        for line in lines {
            let source: Vec<String> = if axis.vertical {
                origin
                    .rows()
                    .map(|row| {
                        self.cell_value(GridRow::from_index(row), AnyColumn(line))
                            .to_string()
                    })
                    .collect()
            } else {
                origin
                    .cols()
                    .map(|col| {
                        self.cell_value(GridRow::from_index(line), AnyColumn(col))
                            .to_string()
                    })
                    .collect()
            };
            for (offset, value) in fill_values(&source, axis.extent, axis.forward, invert)
                .into_iter()
                .enumerate()
            {
                let distance = offset + 1;
                let (row_index, col_index) = if axis.vertical {
                    let Some(row) = step_from(origin, axis.forward, true, distance) else {
                        continue;
                    };
                    (row, line)
                } else {
                    let Some(col) = step_from(origin, axis.forward, false, distance) else {
                        continue;
                    };
                    (line, col)
                };
                let Some(row_ref) = self.row_ref(GridRow::from_index(row_index)) else {
                    continue;
                };
                cells.push((row_ref, AnyColumn(col_index), value));
            }
        }
        if cells.is_empty() {
            return;
        }

        let edits = self
            .with_edit_engine(cx, |engine| engine.set_cells(&cells))
            .unwrap_or_default();
        for (row, col, value) in &cells {
            self.write_cached_cell(*row, *col, value);
        }
        self.clear_status_message();
        self.apply_buffer_edits(edits, cx);
        self.select_rect(filled, cx);
        cx.notify();
    }

    /// Ctrl+D and Ctrl+R: the selection's first row or first column copied over
    /// the rest of it, which is a fill whose source is that one line.
    pub(crate) fn fill_within_selection(&mut self, vertical: bool, cx: &mut Context<Self>) {
        // Ctrl+D and Ctrl+R are the editor's own keys too, and the grid only
        // takes them over while no cell is open for input.
        if self.grid_is_empty() || self.grid.editing.is_some() {
            return;
        }
        if self.refuse_on_multiple_ranges(cx) {
            return;
        }
        let mut filled = self.selection_rect();
        // Selecting a column selects the heading with it, and a fill down from
        // the heading would stamp the column's name over every value in it. The
        // user asking for a whole column cannot mean that, so the fill starts at
        // the first row of the body instead. A cell range that reaches into the
        // heading is left alone: that one the user drew by hand.
        if vertical
            && self.has_header_row()
            && matches!(
                self.grid.selection_kind,
                GridSelectionKind::Columns | GridSelectionKind::All
            )
            && filled.first_row < filled.last_row
        {
            filled.first_row += 1;
        }
        let origin = if vertical {
            GridRect {
                last_row: filled.first_row,
                ..filled
            }
        } else {
            GridRect {
                last_col: filled.first_col,
                ..filled
            }
        };
        self.commit_fill(origin, filled, false, cx);
    }

    ///// Move /////

    /// Moves the block to where the border was dragged, or copies it when the
    /// secondary modifier was held as the button came up.
    ///
    /// The cleared source cells and the written destination cells go in as one
    /// list, the clears first: the engine keeps one value per cell and takes the
    /// last one given, so the overlap between the two rectangles is written
    /// rather than emptied.
    pub(crate) fn commit_block_move(
        &mut self,
        origin: GridRect,
        destination: GridRect,
        copy: bool,
        cx: &mut Context<Self>,
    ) {
        if destination == origin {
            return;
        }
        let moved_cells = origin.cell_count();
        if !self.within_cell_budget(moved_cells.saturating_mul(2), cx) {
            return;
        }

        let mut cells = Vec::with_capacity(moved_cells * 2);
        let mut values = Vec::with_capacity(moved_cells);
        for (row_offset, row_index) in origin.rows().enumerate() {
            for (col_offset, col_index) in origin.cols().enumerate() {
                let row = GridRow::from_index(row_index);
                values.push((
                    row_offset,
                    col_offset,
                    self.cell_value(row, AnyColumn(col_index)).to_string(),
                ));
                if !copy && let Some(row_ref) = self.row_ref(row) {
                    cells.push((row_ref, AnyColumn(col_index), String::new()));
                }
            }
        }
        for (row_offset, col_offset, value) in values {
            let row = GridRow::from_index(destination.first_row + row_offset);
            let Some(row_ref) = self.row_ref(row) else {
                continue;
            };
            cells.push((
                row_ref,
                AnyColumn(destination.first_col + col_offset),
                value,
            ));
        }
        if cells.is_empty() {
            return;
        }

        let edits = self
            .with_edit_engine(cx, |engine| engine.set_cells(&cells))
            .unwrap_or_default();
        for (row, col, value) in &cells {
            self.write_cached_cell(*row, *col, value);
        }
        self.clear_status_message();
        self.apply_buffer_edits(edits, cx);
        self.select_rect(destination, cx);
        cx.notify();
    }

    /// Leaves `rect` as the one selected block, the way a spreadsheet selects
    /// what a gesture just wrote. A gesture that wrote inside the selection it
    /// already had leaves it alone, so that Ctrl+D on selected rows does not
    /// quietly turn them into a cell range - nor a whole column, whose fill
    /// deliberately starts one row below the block the user has selected.
    fn select_rect(&mut self, rect: GridRect, cx: &mut Context<Self>) {
        if !self.has_multiple_selection_ranges() && self.selection_rect().contains_rect(rect) {
            return;
        }
        self.grid.previous_ranges.clear();
        self.grid.anchor = Some(GridCell::new(
            GridRow::from_index(rect.first_row),
            AnyColumn(rect.first_col),
        ));
        self.grid.cursor =
            GridCell::new(GridRow::from_index(rect.last_row), AnyColumn(rect.last_col));
        self.grid.selection_kind = GridSelectionKind::Cells;
        self.notify_search_of_caret(cx);
    }

    ///// Rendering /////

    /// The parts of these two gestures that hang off one body cell: the
    /// invisible bands that grab the selection's border, the fill handle at its
    /// corner, and the outline of where a drag in flight would land.
    ///
    /// All of them are absolutely positioned, so the cell's wrapper has to be a
    /// flex container, and they are ordered so that the fill handle is painted
    /// last: at the bottom right corner it shares a cell with two border bands,
    /// and the later hitbox is the one the pointer finds.
    pub(crate) fn grid_drag_overlays(&self, cell: GridCell, cx: &Context<Self>) -> Vec<AnyElement> {
        let mut overlays = self.render_move_border_bands(cell, cx);
        overlays.extend(self.render_drag_preview_edges(cell, cx));
        overlays.extend(self.render_fill_handle(cell, cx));
        overlays
    }

    fn render_fill_handle(&self, cell: GridCell, cx: &Context<Self>) -> Option<AnyElement> {
        if self.fill_handle_cell() != Some(cell) {
            return None;
        }
        Some(
            div()
                .absolute()
                .right_0()
                .bottom_0()
                .size(FILL_HANDLE_SIZE)
                .bg(cx.theme().colors().border_focused)
                .cursor(CursorStyle::Crosshair)
                .debug_selector(|| FILL_HANDLE_DEBUG_SELECTOR.to_owned())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _event: &MouseDownEvent, window, cx| {
                        // Stopping here keeps the press off the cell underneath,
                        // which also keeps it from the grid's own root, where a
                        // click is what puts the caret back in the cell editor
                        // and makes this pane the active one.
                        this.focus_cell_editor(window, cx);
                        this.start_fill_drag(cx);
                        cx.stop_propagation();
                    }),
                )
                .into_any_element(),
        )
    }

    fn render_move_border_bands(&self, cell: GridCell, cx: &Context<Self>) -> Vec<AnyElement> {
        if !self.block_move_is_available() {
            return Vec::new();
        }
        let rect = self.selection_rect();
        if !rect.contains(cell.row, cell.col) {
            return Vec::new();
        }
        sides_of(rect, cell)
            .into_iter()
            .map(|side| {
                side.place(div(), MOVE_BORDER_WIDTH)
                    .cursor(CursorStyle::OpenHand)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _event: &MouseDownEvent, window, cx| {
                            this.focus_cell_editor(window, cx);
                            this.start_move_drag(cell, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .into_any_element()
            })
            .collect()
    }

    fn render_drag_preview_edges(&self, cell: GridCell, cx: &Context<Self>) -> Vec<AnyElement> {
        let Some(rect) = self.grid.drag_preview else {
            return Vec::new();
        };
        if !rect.contains(cell.row, cell.col) {
            return Vec::new();
        }
        let color = cx.theme().colors().border_focused;
        sides_of(rect, cell)
            .into_iter()
            .map(|side| {
                side.place(div(), DRAG_PREVIEW_WIDTH)
                    .bg(color)
                    .into_any_element()
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BorderSide {
    Top,
    Bottom,
    Left,
    Right,
}

impl BorderSide {
    /// Lays `element` along this side of the cell it hangs in, `width` thick.
    fn place<E: Styled>(self, element: E, width: Pixels) -> E {
        let element = element.absolute();
        match self {
            BorderSide::Top => element.top_0().left_0().right_0().h(width),
            BorderSide::Bottom => element.bottom_0().left_0().right_0().h(width),
            BorderSide::Left => element.left_0().top_0().bottom_0().w(width),
            BorderSide::Right => element.right_0().top_0().bottom_0().w(width),
        }
    }
}

/// Which of `rect`'s outer sides run along `cell`, which is how a rectangle
/// spanning many cells is outlined one cell at a time.
fn sides_of(rect: GridRect, cell: GridCell) -> Vec<BorderSide> {
    let row = cell.row.index();
    let col = cell.col.get();
    let mut sides = Vec::with_capacity(4);
    if row == rect.first_row {
        sides.push(BorderSide::Top);
    }
    if row == rect.last_row {
        sides.push(BorderSide::Bottom);
    }
    if col == rect.first_col {
        sides.push(BorderSide::Left);
    }
    if col == rect.last_col {
        sides.push(BorderSide::Right);
    }
    sides
}

/// The row or column `distance` cells past `origin` in the direction a fill
/// went, or `None` when that falls off the top or the left of the grid.
fn step_from(origin: GridRect, forward: bool, vertical: bool, distance: usize) -> Option<usize> {
    let (first, last) = if vertical {
        (origin.first_row, origin.last_row)
    } else {
        (origin.first_col, origin.last_col)
    };
    if forward {
        last.checked_add(distance)
    } else {
        first.checked_sub(distance)
    }
}

/// Marks the fill handle so that a test can check it is drawn where the
/// selection ends. `debug_selector` compiles away outside of tests.
pub(crate) const FILL_HANDLE_DEBUG_SELECTOR: &str = "tabular-grid-fill-handle";

#[cfg(test)]
mod tests {
    use super::*;

    fn values(source: &[&str]) -> Vec<String> {
        source.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn test_a_single_value_is_copied() {
        assert_eq!(
            fill_values(&values(&["x"]), 3, true, false),
            values(&["x", "x", "x"])
        );
        assert_eq!(
            fill_values(&values(&["7"]), 2, true, false),
            values(&["7", "7"])
        );
        assert_eq!(
            fill_values(&values(&["7"]), 2, false, false),
            values(&["7", "7"])
        );
    }

    #[test]
    fn test_ctrl_counts_a_single_number_up() {
        assert_eq!(
            fill_values(&values(&["7"]), 3, true, true),
            values(&["8", "9", "10"])
        );
        // Upwards the same gesture counts back, because the series is read over
        // the grid's own order.
        assert_eq!(
            fill_values(&values(&["7"]), 3, false, true),
            values(&["6", "5", "4"])
        );
        // A word has no number to count, so Ctrl changes nothing.
        assert_eq!(
            fill_values(&values(&["x"]), 2, true, true),
            values(&["x", "x"])
        );
    }

    #[test]
    fn test_an_even_series_is_extended() {
        assert_eq!(
            fill_values(&values(&["1", "2", "3"]), 3, true, false),
            values(&["4", "5", "6"])
        );
        assert_eq!(
            fill_values(&values(&["1", "2", "3"]), 3, false, false),
            values(&["0", "-1", "-2"])
        );
        assert_eq!(
            fill_values(&values(&["10", "20"]), 2, true, false),
            values(&["30", "40"])
        );
        assert_eq!(
            fill_values(&values(&["3", "2", "1"]), 2, true, false),
            values(&["0", "-1"])
        );
    }

    /// Decimals are stepped as scaled integers, so the values come back spelled
    /// the way the source was rather than as the nearest float.
    #[test]
    fn test_a_decimal_series_keeps_its_decimals() {
        assert_eq!(
            fill_values(&values(&["1.10", "1.20"]), 2, true, false),
            values(&["1.30", "1.40"])
        );
        assert_eq!(
            fill_values(&values(&["1", "1.5"]), 2, true, false),
            values(&["2.0", "2.5"])
        );
        assert_eq!(
            fill_values(&values(&["0.1", "0.2", "0.3"]), 1, true, false),
            values(&["0.4"])
        );
    }

    #[test]
    fn test_an_uneven_or_non_numeric_block_repeats() {
        assert_eq!(
            fill_values(&values(&["1", "2", "4"]), 4, true, false),
            values(&["1", "2", "4", "1"])
        );
        assert_eq!(
            fill_values(&values(&["a", "b"]), 3, true, false),
            values(&["a", "b", "a"])
        );
        // Going backwards the block is stamped bottom up.
        assert_eq!(
            fill_values(&values(&["a", "b", "c"]), 4, false, false),
            values(&["c", "b", "a", "c"])
        );
        assert_eq!(
            fill_values(&values(&["1", "", "3"]), 2, true, false),
            values(&["1", ""])
        );
    }

    /// Ctrl flips a series back to a plain copy, which is the other half of
    /// Excel's modifier.
    #[test]
    fn test_ctrl_copies_a_series_instead_of_extending_it() {
        assert_eq!(
            fill_values(&values(&["1", "2"]), 4, true, true),
            values(&["1", "2", "1", "2"])
        );
    }

    #[test]
    fn test_values_too_long_to_scale_are_copied() {
        let account = "1234567890123456789012345678901";
        assert_eq!(
            fill_values(&values(&[account]), 2, true, true),
            values(&[account, account])
        );
    }

    #[test]
    fn test_a_negative_series_crosses_zero() {
        assert_eq!(
            fill_values(&values(&["-2", "-1"]), 4, true, false),
            values(&["0", "1", "2", "3"])
        );
    }

    #[test]
    fn test_nothing_to_write() {
        assert!(fill_values(&values(&["a"]), 0, true, false).is_empty());
        assert!(fill_values(&[], 3, true, false).is_empty());
    }

    fn rect(first_row: usize, last_row: usize, first_col: usize, last_col: usize) -> GridRect {
        GridRect {
            first_row,
            last_row,
            first_col,
            last_col,
        }
    }

    /// The pointer decides the axis, and only one axis grows: dragging down and
    /// a little to the right fills down, not into a bigger rectangle.
    #[test]
    fn test_the_longer_drag_picks_the_axis() {
        let origin = rect(1, 1, 0, 0);
        assert_eq!(
            stretched_rect(origin, GridCell::new(GridRow::from_index(5), AnyColumn(1))),
            rect(1, 5, 0, 0)
        );
        assert_eq!(
            stretched_rect(origin, GridCell::new(GridRow::from_index(2), AnyColumn(4))),
            rect(1, 1, 0, 4)
        );
        // A tie goes to the vertical axis, the direction a spreadsheet grows in.
        assert_eq!(
            stretched_rect(origin, GridCell::new(GridRow::from_index(3), AnyColumn(2))),
            rect(1, 3, 0, 0)
        );
    }

    /// Dragging the handle back inside the block is Excel's "clear", which this
    /// grid deliberately does not do: nothing is written at all.
    #[test]
    fn test_dragging_inside_the_block_fills_nothing() {
        let origin = rect(1, 4, 0, 2);
        let inside = stretched_rect(origin, GridCell::new(GridRow::from_index(2), AnyColumn(1)));
        assert_eq!(inside, origin);
        assert_eq!(fill_axis(origin, inside), None);
    }

    #[test]
    fn test_fill_axis_reads_the_stretched_rectangle() {
        let origin = rect(2, 3, 1, 1);
        assert_eq!(
            fill_axis(origin, rect(2, 6, 1, 1)),
            Some(FillAxis {
                vertical: true,
                forward: true,
                extent: 3
            })
        );
        assert_eq!(
            fill_axis(origin, rect(0, 3, 1, 1)),
            Some(FillAxis {
                vertical: true,
                forward: false,
                extent: 2
            })
        );
        assert_eq!(
            fill_axis(origin, rect(2, 3, 1, 4)),
            Some(FillAxis {
                vertical: false,
                forward: true,
                extent: 3
            })
        );
        assert_eq!(
            fill_axis(origin, rect(2, 3, 0, 1)),
            Some(FillAxis {
                vertical: false,
                forward: false,
                extent: 1
            })
        );
    }

    #[test]
    fn test_a_shifted_block_keeps_its_shape() {
        assert_eq!(shifted_start(3, 2, 1, 10), 5);
        assert_eq!(shifted_start(3, -5, 1, 10), 1);
        assert_eq!(shifted_start(3, 20, 1, 10), 10);
        // A block taller than what is left below it cannot move at all.
        assert_eq!(shifted_start(1, 3, 1, 0), 1);
    }

    #[test]
    fn test_step_from_stops_at_the_edges() {
        let origin = rect(1, 2, 0, 1);
        assert_eq!(step_from(origin, true, true, 1), Some(3));
        assert_eq!(step_from(origin, false, true, 1), Some(0));
        assert_eq!(step_from(origin, false, true, 2), None);
        assert_eq!(step_from(origin, true, false, 2), Some(3));
        assert_eq!(step_from(origin, false, false, 1), None);
    }

    ///// The gestures, end to end /////

    use crate::{
        grid::{GridSelectionKind, MULTI_RANGE_REFUSAL, SelectionChange},
        grid_editing::tests::open_grid,
        types::DisplayRow,
    };
    use gpui::{Entity, Focusable as _, Modifiers, TestAppContext, VisualTestContext};

    fn body_cell(row: usize, col: usize) -> GridCell {
        GridCell::new(GridRow::Body(DisplayRow(row)), AnyColumn(col))
    }

    fn buffer_text(editor: &Entity<editor::Editor>, cx: &mut VisualTestContext) -> String {
        editor.read_with(cx, |editor, cx| editor.text(cx))
    }

    fn select(
        pane: &Entity<TabularDataPreviewPane>,
        from: GridCell,
        to: GridCell,
        cx: &mut VisualTestContext,
    ) {
        pane.update(cx, |pane, cx| {
            pane.change_cell_selection(from, SelectionChange::Replace, cx);
            pane.change_cell_selection(to, SelectionChange::Extend, cx);
        });
    }

    /// Drags the fill handle of the current selection out to `to` and lets go,
    /// which is the path the pointer takes: the handle starts the drag, the
    /// cells under the pointer move the preview, the button up commits it.
    fn drag_fill_handle(
        pane: &Entity<TabularDataPreviewPane>,
        to: GridCell,
        modifiers: Modifiers,
        cx: &mut VisualTestContext,
    ) {
        pane.update(cx, |pane, cx| {
            let origin = pane.selection_rect();
            pane.grid.drag = Some(GridDrag::Fill { origin });
            pane.grid.drag_preview = Some(origin);
            pane.on_cell_drag(to, cx);
            pane.end_drag(modifiers, cx);
        });
        cx.run_until_parked();
    }

    /// The same for the selection's border: grabbed on `grabbed` and dropped
    /// with `grabbed` over `to`.
    fn drag_block_border(
        pane: &Entity<TabularDataPreviewPane>,
        grabbed: GridCell,
        to: GridCell,
        modifiers: Modifiers,
        cx: &mut VisualTestContext,
    ) {
        pane.update(cx, |pane, cx| {
            let origin = pane.selection_rect();
            pane.grid.drag = Some(GridDrag::Move { origin, grabbed });
            pane.grid.drag_preview = Some(origin);
            pane.on_cell_drag(to, cx);
            pane.end_drag(modifiers, cx);
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_dragging_the_handle_copies_one_cell_down(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("h,v\nx,1\n,2\n,3", cx).await;

        select(&pane, body_cell(0, 0), body_cell(0, 0), &mut cx);
        drag_fill_handle(&pane, body_cell(2, 0), Modifiers::none(), &mut cx);

        assert_eq!(buffer_text(&editor, &mut cx), "h,v\nx,1\nx,2\nx,3");
        pane.read_with(&cx, |pane, _| {
            let rect = pane.selection_rect();
            assert_eq!((rect.first_row, rect.last_row), (1, 3));
        });

        // The whole fill is one batch of edits.
        cx.dispatch_action(editor::actions::Undo);
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &mut cx), "h,v\nx,1\n,2\n,3");
    }

    #[gpui::test]
    async fn test_dragging_the_handle_extends_a_series(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("h\n1\n2\nx\nx\nx", cx).await;

        select(&pane, body_cell(0, 0), body_cell(1, 0), &mut cx);
        drag_fill_handle(&pane, body_cell(4, 0), Modifiers::none(), &mut cx);

        assert_eq!(buffer_text(&editor, &mut cx), "h\n1\n2\n3\n4\n5");
    }

    /// Ctrl on the way up turns one number into a counted series. It is read at
    /// the end of the gesture, which is when Excel reads it.
    #[gpui::test]
    async fn test_ctrl_on_release_counts_a_single_number_up(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("h\n7\nx\nx", cx).await;

        select(&pane, body_cell(0, 0), body_cell(0, 0), &mut cx);
        drag_fill_handle(&pane, body_cell(2, 0), Modifiers::secondary_key(), &mut cx);

        assert_eq!(buffer_text(&editor, &mut cx), "h\n7\n8\n9");
    }

    #[gpui::test]
    async fn test_dragging_the_handle_sideways_fills_the_row(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b,c\n5,,", cx).await;

        select(&pane, body_cell(0, 0), body_cell(0, 0), &mut cx);
        drag_fill_handle(&pane, body_cell(0, 2), Modifiers::none(), &mut cx);

        assert_eq!(buffer_text(&editor, &mut cx), "a,b,c\n5,5,5");
    }

    #[gpui::test]
    async fn test_dragging_the_handle_upwards_extrapolates_backwards(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("h\nx\nx\n3\n4", cx).await;

        select(&pane, body_cell(2, 0), body_cell(3, 0), &mut cx);
        drag_fill_handle(&pane, body_cell(0, 0), Modifiers::none(), &mut cx);

        assert_eq!(buffer_text(&editor, &mut cx), "h\n1\n2\n3\n4");
    }

    /// A fill writes the rows the user can see, so under a filter it skips the
    /// hidden row in between - which is what Excel does with a filtered list.
    #[gpui::test]
    async fn test_filling_a_filtered_table_writes_the_visible_rows(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("k,v\na,1\nb,2\na,3\na,4", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.toggle_filter(AnyColumn(0), Some("a".into()), cx);
        });
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| assert_eq!(pane.body_row_count(), 3));

        select(&pane, body_cell(0, 1), body_cell(0, 1), &mut cx);
        drag_fill_handle(&pane, body_cell(2, 1), Modifiers::none(), &mut cx);

        assert_eq!(buffer_text(&editor, &mut cx), "k,v\na,1\nb,2\na,1\na,1");

        cx.dispatch_action(editor::actions::Undo);
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &mut cx), "k,v\na,1\nb,2\na,3\na,4");
    }

    #[gpui::test]
    async fn test_dragging_the_handle_back_inside_writes_nothing(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("h\n1\n2\n3", cx).await;

        select(&pane, body_cell(0, 0), body_cell(2, 0), &mut cx);
        drag_fill_handle(&pane, body_cell(1, 0), Modifiers::none(), &mut cx);

        assert_eq!(buffer_text(&editor, &mut cx), "h\n1\n2\n3");
    }

    #[gpui::test]
    async fn test_ctrl_d_copies_the_first_row_over_the_selection(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\n1,2\n3,4\n5,6", cx).await;

        select(&pane, body_cell(0, 0), body_cell(2, 1), &mut cx);
        cx.simulate_keystrokes("ctrl-d");
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "a,b\n1,2\n1,2\n1,2");
        // The fill stayed inside the selection, so the selection is unchanged.
        pane.read_with(&cx, |pane, _| {
            let rect = pane.selection_rect();
            assert_eq!((rect.first_row, rect.last_row), (1, 3));
        });
    }

    /// A fill that wrote inside the selection leaves it alone, so selected rows
    /// stay selected rows: turning them into a cell range would quietly change
    /// what the next Delete or the next color covers.
    #[gpui::test]
    async fn test_ctrl_d_on_selected_rows_leaves_them_selected_as_rows(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\n1,2\n3,4", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.change_row_selection(GridRow::Body(DisplayRow(0)), SelectionChange::Replace, cx);
            pane.change_row_selection(GridRow::Body(DisplayRow(1)), SelectionChange::Extend, cx);
        });
        cx.simulate_keystrokes("ctrl-d");
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "a,b\n1,2\n1,2");
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.grid.selection_kind,
                GridSelectionKind::Rows,
                "the rows the fill covered are still selected as rows"
            );
        });
    }

    #[gpui::test]
    async fn test_ctrl_r_copies_the_first_column_over_the_selection(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b,c\n1,2,3", cx).await;

        select(&pane, body_cell(0, 0), body_cell(0, 2), &mut cx);
        cx.simulate_keystrokes("ctrl-r");
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "a,b,c\n1,1,1");
    }

    /// Ctrl+D is the editor's "select next occurrence" everywhere else, so the
    /// grid has to reach it at all - and has to leave it alone while a cell is
    /// open for input, where the keys belong to the text being typed.
    #[gpui::test]
    async fn test_ctrl_d_does_nothing_while_a_cell_is_open(cx: &mut TestAppContext) {
        let (_pane, editor, mut cx) = open_grid("a,b\n1,2\n3,4", cx).await;

        cx.simulate_keystrokes("down");
        cx.simulate_input("z");
        cx.simulate_keystrokes("ctrl-d");
        cx.run_until_parked();
        cx.dispatch_action(crate::CommitEditing);
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "a,b\nz,2\n3,4");
    }

    #[gpui::test]
    async fn test_a_multiple_selection_refuses_ctrl_d(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\n1,2\n3,4", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.change_cell_selection(body_cell(0, 0), SelectionChange::Replace, cx);
            pane.change_cell_selection(body_cell(1, 0), SelectionChange::Extend, cx);
            pane.change_cell_selection(body_cell(0, 1), SelectionChange::Add, cx);
            pane.change_cell_selection(body_cell(1, 1), SelectionChange::Extend, cx);
        });
        cx.simulate_keystrokes("ctrl-d");
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "a,b\n1,2\n3,4");
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.grid.status_message.as_deref(),
                Some(MULTI_RANGE_REFUSAL)
            );
        });
    }

    #[gpui::test]
    async fn test_dragging_the_border_moves_the_block(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b,c\n1,2,\n3,4,\n,,", cx).await;

        select(&pane, body_cell(0, 0), body_cell(1, 1), &mut cx);
        drag_block_border(
            &pane,
            body_cell(0, 0),
            body_cell(1, 1),
            Modifiers::none(),
            &mut cx,
        );

        assert_eq!(buffer_text(&editor, &mut cx), "a,b,c\n,,\n,1,2\n,3,4");
        pane.read_with(&cx, |pane, _| {
            let rect = pane.selection_rect();
            assert_eq!((rect.first_row, rect.first_col), (2, 1));
        });

        // The clear and the write are one batch, so one undo puts it all back.
        cx.dispatch_action(editor::actions::Undo);
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &mut cx), "a,b,c\n1,2,\n3,4,\n,,");
    }

    #[gpui::test]
    async fn test_ctrl_on_release_copies_the_block_instead(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b,c\n1,2,\n,,\n,,", cx).await;

        select(&pane, body_cell(0, 0), body_cell(0, 1), &mut cx);
        drag_block_border(
            &pane,
            body_cell(0, 0),
            body_cell(2, 0),
            Modifiers::secondary_key(),
            &mut cx,
        );

        assert_eq!(buffer_text(&editor, &mut cx), "a,b,c\n1,2,\n,,\n1,2,");
    }

    /// A block dragged past the last row stops at it whole rather than losing
    /// the rows that would fall off.
    #[gpui::test]
    async fn test_a_moved_block_stays_inside_the_table(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\n1,2\n3,4\n,", cx).await;

        select(&pane, body_cell(0, 0), body_cell(1, 1), &mut cx);
        drag_block_border(
            &pane,
            body_cell(0, 0),
            body_cell(2, 1),
            Modifiers::none(),
            &mut cx,
        );

        // Two rows starting on the last one would need a fourth row; the block
        // lands on the last two instead.
        assert_eq!(buffer_text(&editor, &mut cx), "a,b\n,\n1,2\n3,4");
    }

    /// A row that came out of the file shorter than the table is wide has no
    /// cell to write the block's second column into; the engine grows the row
    /// rather than dropping the value.
    #[gpui::test]
    async fn test_a_block_moved_onto_a_short_row_grows_it(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b,c\n1,2,3\nx", cx).await;

        select(&pane, body_cell(0, 0), body_cell(0, 1), &mut cx);
        drag_block_border(
            &pane,
            body_cell(0, 0),
            body_cell(1, 0),
            Modifiers::none(),
            &mut cx,
        );

        assert_eq!(buffer_text(&editor, &mut cx), "a,b,c\n,,3\n1,2");
    }

    /// Sorted or filtered, the rows on screen are not the rows of the file, so
    /// the border stops being grabbable at all.
    #[gpui::test]
    async fn test_the_border_cannot_be_grabbed_while_filtered(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("k,v\na,1\nb,2\na,3", cx).await;

        pane.read_with(&cx, |pane, _| assert!(pane.block_move_is_available()));

        pane.update(&mut cx, |pane, cx| {
            pane.toggle_filter(AnyColumn(0), Some("a".into()), cx);
        });
        cx.run_until_parked();

        pane.read_with(&cx, |pane, _| {
            assert!(!pane.block_move_is_available());
            // The fill handle stays: a fill writes what the user can see.
            assert!(pane.fill_handle_cell().is_some());
        });
    }

    #[gpui::test]
    async fn test_the_handle_is_drawn_at_the_end_of_a_single_block(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("a,b\n1,2\n3,4", cx).await;

        select(&pane, body_cell(0, 0), body_cell(1, 1), &mut cx);
        cx.run_until_parked();

        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.fill_handle_cell(), Some(body_cell(1, 1)));
        });
        let handle = cx
            .debug_bounds(FILL_HANDLE_DEBUG_SELECTOR)
            .expect("the fill handle should be drawn at the corner of the selection");
        assert!(
            handle.size.width > px(0.) && handle.size.height > px(0.),
            "the fill handle was drawn with no size ({handle:?})"
        );

        // Two blocks have no one corner to drag, so the handle goes away.
        pane.update(&mut cx, |pane, cx| {
            pane.change_cell_selection(body_cell(0, 1), SelectionChange::Add, cx);
        });
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| assert_eq!(pane.fill_handle_cell(), None));
        assert_eq!(cx.debug_bounds(FILL_HANDLE_DEBUG_SELECTOR), None);
    }

    /// The handle sits on top of a cell that takes presses of its own, so the
    /// press has to reach the handle and stop there - and, because stopping it
    /// also keeps it from the grid's root, the handle has to put the caret back
    /// in the grid itself, the way any other press in the grid does.
    #[gpui::test]
    async fn test_pressing_the_handle_starts_a_fill_rather_than_a_selection(
        cx: &mut TestAppContext,
    ) {
        let (pane, _editor, mut cx) = open_grid("h\n1\n2\n3", cx).await;

        select(&pane, body_cell(0, 0), body_cell(0, 0), &mut cx);
        cx.run_until_parked();
        let handle = cx
            .debug_bounds(FILL_HANDLE_DEBUG_SELECTOR)
            .expect("the fill handle should be drawn at the corner of the selection");

        let grid_focus = pane.read_with(&cx, |pane, _| pane.focus_handle.clone());
        cx.update(|window, cx| window.focus(&grid_focus, cx));
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert!(
                !pane
                    .read(cx)
                    .grid
                    .cell_editor
                    .focus_handle(cx)
                    .is_focused(window),
                "the fixture has to start with the caret away from the cell editor"
            );
        });

        cx.simulate_mouse_down(handle.center(), MouseButton::Left, Modifiers::none());
        cx.run_until_parked();

        pane.read_with(&cx, |pane, _| {
            assert!(
                matches!(pane.grid.drag, Some(GridDrag::Fill { .. })),
                "the press on the handle should start a fill, not a cell drag \
                 ({:?})",
                pane.grid.drag
            );
            assert_eq!(
                pane.selection_rect(),
                rect(1, 1, 0, 0),
                "the press must not move the selection it is about to stretch"
            );
        });
        cx.update(|window, cx| {
            assert!(
                pane.read(cx)
                    .grid
                    .cell_editor
                    .focus_handle(cx)
                    .is_focused(window),
                "the press has to take the caret back, since it never reaches the root"
            );
        });
    }

    /// The colors are painted on every block, and a block that is a whole row
    /// stays one stored entry rather than one per cell.
    #[gpui::test]
    async fn test_colors_cover_every_selected_block(cx: &mut TestAppContext) {
        use crate::grid_styles::{CellColor, StyleField};

        let (pane, _editor, mut cx) = open_grid("a,b,c\n1,2,3\n4,5,6", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.change_cell_selection(body_cell(0, 0), SelectionChange::Replace, cx);
            pane.change_row_selection(GridRow::Body(DisplayRow(1)), SelectionChange::Add, cx);
            pane.set_selection_color(StyleField::Fill, CellColor::Red, cx);
        });
        cx.run_until_parked();

        pane.read_with(&cx, |pane, cx| {
            let styles = pane.styles.read(cx).styles();
            assert_eq!(styles.style_for(1, 0).fill, Some(CellColor::Red));
            assert_eq!(styles.style_for(2, 0).fill, Some(CellColor::Red));
            assert_eq!(styles.style_for(2, 2).fill, Some(CellColor::Red));
            // The cell block did not spill onto the rest of its row.
            assert_eq!(styles.style_for(1, 1).fill, None);
            assert_eq!(
                pane.selection_style_scopes().len(),
                2,
                "each block paints its own scope"
            );
        });
    }

    /// Pinning the search to a multiple selection can only name one rectangle,
    /// so it names the one around all of the blocks.
    #[gpui::test]
    async fn test_the_search_range_is_the_rectangle_around_every_block(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("a,b,c\n1,2,3\n4,5,6\n7,8,9", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.change_cell_selection(body_cell(0, 0), SelectionChange::Replace, cx);
            pane.change_cell_selection(body_cell(2, 2), SelectionChange::Add, cx);
        });

        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.selection_bounding_rect(),
                GridRect {
                    first_row: 1,
                    last_row: 3,
                    first_col: 0,
                    last_col: 2,
                }
            );
            assert_eq!(pane.grid.selection_kind, GridSelectionKind::Cells);
        });
    }

    /// Selecting a whole column selects its heading with it, and a fill down
    /// from the heading would stamp the column's name over every value in it.
    /// The fill starts at the first row of the body instead - and leaves the
    /// column selected as a column.
    #[gpui::test]
    async fn test_ctrl_d_on_a_whole_column_starts_below_the_heading(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("h,k\n1,a\n2,b\n3,c", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.change_column_selection(AnyColumn(0), SelectionChange::Replace, cx);
        });
        cx.simulate_keystrokes("ctrl-d");
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "h,k\n1,a\n1,b\n1,c");
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.grid.selection_kind,
                GridSelectionKind::Columns,
                "the column the fill covered is still selected as a column"
            );
        });
    }

    /// A cell range the user drew by hand that happens to reach into the heading
    /// is left alone: that one they asked for.
    #[gpui::test]
    async fn test_ctrl_d_on_a_hand_drawn_range_still_starts_at_its_top(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("h,k\n1,a\n2,b", cx).await;

        select(
            &pane,
            GridCell::new(GridRow::Header, AnyColumn(0)),
            body_cell(1, 0),
            &mut cx,
        );
        cx.simulate_keystrokes("ctrl-d");
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "h,k\nh,a\nh,b");
    }

    /// Without a heading row there is nothing to skip, so a whole column fills
    /// from its own first row.
    #[gpui::test]
    async fn test_ctrl_d_on_a_column_of_a_headerless_file(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("1,a\n2,b\n3,c", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.toggle_header_row(cx);
        });
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| assert!(!pane.has_header_row()));

        pane.update(&mut cx, |pane, cx| {
            pane.change_column_selection(AnyColumn(0), SelectionChange::Replace, cx);
        });
        cx.simulate_keystrokes("ctrl-d");
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "1,a\n1,b\n1,c");
    }
}
