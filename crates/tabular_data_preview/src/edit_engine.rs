//! Pure computation of the buffer edits behind the spreadsheet-style grid.
//!
//! Every method resolves the anchors held by [`TableLikeContent`] against one snapshot and returns
//! edits sorted by ascending, non-overlapping offsets, so the caller can hand the whole batch to a
//! single `Buffer::edit` call and get exactly one undo step. `Buffer::edit` coalesces adjacent
//! edits; the batches produced here keep their meaning when it does.
//!
//! The snapshot must be the one the contents were parsed from: an anchor taken from a different
//! version resolves to a different place, which would silently corrupt the file. [`EditEngine::new`]
//! checks that in debug builds, so reparse (`reparse_now`) before building an engine whenever the
//! buffer may have moved on.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use text::{BufferSnapshot, ToOffset};
use ui::table_row::TableRow;

use crate::types::{AnyColumn, CellContentSpan, DataRow, TableCell, TableLikeContent};

/// A single edit to the buffer. Offsets are byte offsets into `snapshot`, ascending and
/// non-overlapping across the returned vector.
pub(crate) type BufferEdit = (Range<usize>, String);

/// A block of cell values in row-major order. Rows may be ragged.
pub(crate) type CellBlock = Vec<Vec<String>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum RowRef {
    Header,
    Data(DataRow),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowInsertPosition {
    Before(RowRef),
    After(RowRef),
    AtEnd,
}

/// Quotes `value` per RFC 4180: only when it holds the delimiter, a quote or a line terminator.
/// An empty value stays empty; surrounding whitespace is not a reason to quote.
pub(crate) fn quote_field(value: &str, delimiter: char) -> String {
    if value.is_empty() {
        return String::new();
    }
    if value.contains(delimiter) || value.contains(['"', '\n', '\r']) {
        always_quoted(value)
    } else {
        value.to_owned()
    }
}

/// Renders a block the way spreadsheets put it on the clipboard: tab separated cells, one line per
/// row, quoting any cell that would otherwise be misread when the text is parsed back.
pub(crate) fn block_to_clipboard_text(block: &CellBlock) -> String {
    block
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| clipboard_field(value))
                .collect::<Vec<_>>()
                .join("\t")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Reads clipboard text back into a block. `parse_clipboard_table` answers `None` for a payload
/// that is a single cell, which the grid still has to be able to paste, so that case is unquoted
/// here and returned as a one by one block.
pub(crate) fn clipboard_text_to_block(text: &str) -> CellBlock {
    if let Some(block) = editor::markdown_actions::parse_clipboard_table(text) {
        return block;
    }
    vec![vec![unquote_field(text)]]
}

/// Undoes RFC 4180 quoting for one field. Text that is not wrapped in quotes is its own value.
fn unquote_field(value: &str) -> String {
    let Some(inner) = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    else {
        return value.to_owned();
    };

    let mut unquoted = String::with_capacity(inner.len());
    let mut characters = inner.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '"' && characters.peek() == Some(&'"') {
            characters.next();
        }
        unquoted.push(character);
    }
    unquoted
}

fn clipboard_field(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    // `editor::markdown_actions::parse_clipboard_table` falls back to a comma separator when the
    // payload holds no tab at all, so a comma has to be quoted too for the round trip to survive.
    if value.contains(['\t', ',', '"', '\n', '\r']) {
        always_quoted(value)
    } else {
        value.to_owned()
    }
}

fn always_quoted(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        if character == '"' {
            quoted.push('"');
        }
        quoted.push(character);
    }
    quoted.push('"');
    quoted
}

/// Relative order of two edits that start at the same offset: an in-place cell rewrite comes before
/// text appended to the same row, which comes before whole rows appended after it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum EditOrder {
    InPlace,
    AppendToRow,
    AppendRows,
}

struct PendingEdit {
    range: Range<usize>,
    text: String,
    order: EditOrder,
}

/// Offsets of one row: the row itself (without its line terminator) and each of its real cells.
/// Virtual padding cells have no buffer position and are therefore absent.
struct RowLayout {
    range: Range<usize>,
    cell_ranges: Vec<Range<usize>>,
}

pub(crate) struct EditEngine<'a> {
    contents: &'a TableLikeContent,
    snapshot: &'a BufferSnapshot,
    delimiter: char,
}

impl<'a> EditEngine<'a> {
    pub(crate) fn new(
        contents: &'a TableLikeContent,
        snapshot: &'a BufferSnapshot,
        delimiter: char,
    ) -> Self {
        debug_assert!(
            contents
                .source_version
                .as_ref()
                .is_none_or(|version| version == snapshot.version()),
            "the edit engine needs the snapshot its contents were parsed from, \
             otherwise every anchor resolves to the wrong offset"
        );
        Self {
            contents,
            snapshot,
            delimiter,
        }
    }

    /// Writes `value` into the cell. A virtual cell is turned into a real one by appending
    /// delimiters to the end of its row. A cell that was quoted in the source stays quoted.
    pub(crate) fn set_cell(&self, row: RowRef, col: AnyColumn, value: &str) -> Vec<BufferEdit> {
        self.set_cells(&[(row, col, value.to_owned())])
    }

    /// Writes several cells at once, merging every virtual cell of one row into a single append.
    pub(crate) fn set_cells(&self, cells: &[(RowRef, AnyColumn, String)]) -> Vec<BufferEdit> {
        self.finish_keeping_last_row(self.write_cells(cells, false))
    }

    pub(crate) fn clear_cells(&self, cells: &[(RowRef, AnyColumn)]) -> Vec<BufferEdit> {
        let mut columns_by_row: BTreeMap<RowRef, BTreeSet<usize>> = BTreeMap::new();
        for (row, col) in cells {
            columns_by_row
                .entry(self.resolve(*row))
                .or_default()
                .insert(**col);
        }

        let mut edits = Vec::new();
        for (row, columns) in columns_by_row {
            let Some(layout) = self.row_layout(row) else {
                log::warn!("ignoring a request to clear cells of the missing row {row:?}");
                continue;
            };
            for col in columns {
                let Some(cell_range) = layout.cell_ranges.get(col) else {
                    continue;
                };
                edits.push(PendingEdit {
                    range: cell_range.clone(),
                    text: String::new(),
                    order: EditOrder::InPlace,
                });
            }
        }
        self.finish_keeping_last_row(edits)
    }

    /// Inserts `count` blank rows, each holding `number_of_cols` empty cells.
    ///
    /// `Before(RowRef::Header)` is a no-op: the inserted blank line would become the file's first
    /// line and therefore the new header, silently demoting the real header to a data row.
    pub(crate) fn insert_rows(&self, at: RowInsertPosition, count: usize) -> Vec<BufferEdit> {
        if count == 0 {
            return Vec::new();
        }
        let blank_row = self.blank_row_text();

        match at {
            RowInsertPosition::Before(RowRef::Header) if self.contents.header_span.is_some() => {
                log::warn!("refusing to insert a row above the header row");
                Vec::new()
            }
            RowInsertPosition::Before(row) => {
                let Some(layout) = self.row_layout(self.resolve(row)) else {
                    log::warn!("ignoring a request to insert rows above the missing row {row:?}");
                    return Vec::new();
                };
                let mut text = String::new();
                for _ in 0..count {
                    text.push_str(&blank_row);
                    text.push('\n');
                }
                vec![(layout.range.start..layout.range.start, text)]
            }
            RowInsertPosition::After(row) => {
                let Some(layout) = self.row_layout(self.resolve(row)) else {
                    log::warn!("ignoring a request to insert rows below the missing row {row:?}");
                    return Vec::new();
                };
                vec![(
                    layout.range.end..layout.range.end,
                    repeat_rows_after(&blank_row, count),
                )]
            }
            RowInsertPosition::AtEnd => match self.last_row_end() {
                // Appending after the last row's content instead of after the buffer's final
                // newline keeps the file's trailing newline exactly as it was.
                Some(offset) => vec![(offset..offset, repeat_rows_after(&blank_row, count))],
                None => {
                    let mut text = String::new();
                    for _ in 0..count {
                        text.push_str(&blank_row);
                        text.push('\n');
                    }
                    vec![(0..0, text)]
                }
            },
        }
    }

    /// Deletes whole rows including their line terminator. The last row of a buffer that has no
    /// trailing terminator takes the one before it instead, unless that would swallow a blank row;
    /// a buffer's only row keeps its line and loses just its content.
    pub(crate) fn delete_rows(&self, rows: &[RowRef]) -> Vec<BufferEdit> {
        let mut ranges = Vec::new();
        let mut last_row_included = false;
        for row in rows {
            let Some(layout) = self.row_layout(self.resolve(*row)) else {
                log::warn!("ignoring a request to delete the missing row {row:?}");
                continue;
            };
            let range = layout.range;
            let mut following = self.snapshot.chars_at(range.end);
            match following.next() {
                Some('\n') => ranges.push(range.start..range.end + 1),
                Some('\r') => {
                    let terminator_len = if following.next() == Some('\n') { 2 } else { 1 };
                    ranges.push(range.start..range.end + terminator_len);
                }
                // Nothing follows the last row of a buffer that has no trailing terminator, so it
                // has to take the one before it instead.
                _ => {
                    last_row_included = true;
                    ranges.push(range);
                }
            }
        }

        let mut ranges = merge_ranges(ranges);
        if last_row_included
            && let Some(last) = ranges.last_mut()
            && self.line_before_is_not_blank(last.start)
        {
            last.start -= 1;
        }

        ranges
            .into_iter()
            .map(|range| (range, String::new()))
            .collect()
    }

    /// Whether `offset` is preceded by a terminator that belongs to a line with content. Taking a
    /// blank line's terminator would delete that whole (still wanted) row along with this one.
    fn line_before_is_not_blank(&self, offset: usize) -> bool {
        if offset == 0 {
            return false;
        }
        let mut preceding = self.snapshot.reversed_chars_at(offset);
        preceding.next() == Some('\n')
            && preceding
                .next()
                .is_some_and(|character| character != '\n' && character != '\r')
    }

    /// Inserts `count` columns at `index` (`0..=number_of_cols`). Rows with fewer real cells than
    /// `index` are padded at their end so every row ends up with at least `index + count` cells.
    pub(crate) fn insert_columns(&self, index: usize, count: usize) -> Vec<BufferEdit> {
        if count == 0 {
            return Vec::new();
        }
        if index > self.contents.number_of_cols {
            log::warn!(
                "ignoring a request to insert columns at {index}, past the table's {} columns",
                self.contents.number_of_cols
            );
            return Vec::new();
        }
        let delimiters = self.delimiter.to_string().repeat(count);

        let mut edits = Vec::new();
        for row in self.all_rows() {
            let Some(layout) = self.row_layout(row) else {
                continue;
            };
            if let Some(cell_range) = layout.cell_ranges.get(index) {
                edits.push(PendingEdit {
                    range: cell_range.start..cell_range.start,
                    text: delimiters.clone(),
                    order: EditOrder::InPlace,
                });
                continue;
            }

            // A row that has no cell at `index` is short: give it every missing cell at once.
            let existing_cells = layout.cell_ranges.len().max(1);
            let missing_cells = (index + count).saturating_sub(existing_cells);
            if missing_cells > 0 {
                edits.push(PendingEdit {
                    range: layout.range.end..layout.range.end,
                    text: self.delimiter.to_string().repeat(missing_cells),
                    order: EditOrder::AppendToRow,
                });
            }
        }
        finish(edits)
    }

    /// Deletes columns together with one neighbouring delimiter each. Columns are handled in
    /// consecutive runs so that a run reaching the end of a row takes exactly one delimiter for the
    /// whole run rather than one per column. Rows without a real cell at that column are left alone.
    pub(crate) fn delete_columns(&self, cols: &[AnyColumn]) -> Vec<BufferEdit> {
        let columns: BTreeSet<usize> = cols.iter().map(|col| **col).collect();
        if columns.is_empty() {
            return Vec::new();
        }

        let mut edits = Vec::new();
        for row in self.all_rows() {
            let Some(layout) = self.row_layout(row) else {
                continue;
            };

            let mut runs: Vec<(usize, usize)> = Vec::new();
            for &col in columns.range(..layout.cell_ranges.len()) {
                match runs.last_mut() {
                    Some((_, run_end)) if *run_end + 1 == col => *run_end = col,
                    _ => runs.push((col, col)),
                }
            }

            let mut ranges = Vec::new();
            for (run_start, run_end) in runs {
                let (Some(first_cell), Some(last_cell)) = (
                    layout.cell_ranges.get(run_start),
                    layout.cell_ranges.get(run_end),
                ) else {
                    continue;
                };
                if let Some(next_cell) = layout.cell_ranges.get(run_end + 1) {
                    ranges.push(first_cell.start..next_cell.start);
                } else if let Some(previous_cell) = run_start
                    .checked_sub(1)
                    .and_then(|previous| layout.cell_ranges.get(previous))
                {
                    ranges.push(previous_cell.end..last_cell.end);
                } else {
                    // Every cell of the row is going away, so only its content is deleted and the
                    // row survives as a blank line.
                    ranges.push(first_cell.start..last_cell.end);
                }
            }
            edits.extend(merge_ranges(ranges).into_iter().map(|range| PendingEdit {
                range,
                text: String::new(),
                order: EditOrder::InPlace,
            }));
        }
        self.finish_keeping_last_row(edits)
    }

    /// Writes `block` over `rows`, one block row per entry, starting at `first_col` in each of
    /// them. Block rows past the end of `rows` become new rows at the end of the file, and rows
    /// too short for the block grow at their end.
    ///
    /// `rows` is the grid's own resolution of its selection, in the order the grid shows it, so a
    /// sorted or filtered table is written the way it is read rather than in data order. Everything
    /// lands in one batch of edits, which makes the whole paste a single undo step.
    pub(crate) fn paste_block_rows(
        &self,
        rows: &[RowRef],
        first_col: AnyColumn,
        block: &CellBlock,
    ) -> Vec<BufferEdit> {
        if block.is_empty() {
            return Vec::new();
        }
        let first_col = *first_col;

        let mut written_rows = BTreeSet::new();
        let mut cells = Vec::new();
        let mut appended_rows: Vec<&Vec<String>> = Vec::new();
        for (offset, values) in block.iter().enumerate() {
            let Some(row) = rows.get(offset).map(|row| self.resolve(*row)) else {
                appended_rows.push(values);
                continue;
            };
            if !self.row_exists(row) {
                log::warn!("ignoring a pasted block row aimed at the missing row {row:?}");
                continue;
            }
            if !written_rows.insert(row) {
                log::warn!("ignoring a pasted block row aimed at {row:?} for the second time");
                continue;
            }
            cells.extend(values.iter().enumerate().map(|(column_offset, value)| {
                (row, AnyColumn(first_col + column_offset), value.clone())
            }));
        }

        let mut edits = self.write_cells(&cells, true);
        if !appended_rows.is_empty() {
            // With no row to append after, the new rows start the file and each needs its own
            // terminator; otherwise every row is introduced by one.
            let (anchor, terminate_each_row) = match self.last_row_end() {
                Some(offset) => (offset, false),
                // The implicit first row was just written at offset 0 by `write_cells`.
                None if !cells.is_empty() => (0, false),
                None => (0, true),
            };
            let mut text = String::new();
            for values in appended_rows {
                if !terminate_each_row {
                    text.push('\n');
                }
                text.push_str(&self.new_row_text(first_col, values));
                if terminate_each_row {
                    text.push('\n');
                }
            }
            edits.push(PendingEdit {
                range: anchor..anchor,
                text,
                order: EditOrder::AppendRows,
            });
        }
        self.finish_keeping_last_row(edits)
    }

    /// Reads the values of a rectangular selection. Virtual and missing cells read as empty.
    pub(crate) fn copy_block(&self, rows: &[RowRef], cols: Range<usize>) -> CellBlock {
        rows.iter()
            .map(|row| {
                let cells = self.row_cells(*row);
                cols.clone()
                    .map(|col| {
                        cells
                            .and_then(|cells| cells.get(col))
                            .and_then(|cell| cell.display_value())
                            .map(|value| value.to_string())
                            .unwrap_or_default()
                    })
                    .collect()
            })
            .collect()
    }

    fn write_cells(
        &self,
        cells: &[(RowRef, AnyColumn, String)],
        allow_new_columns: bool,
    ) -> Vec<PendingEdit> {
        let mut cells_by_row: BTreeMap<RowRef, BTreeMap<usize, &str>> = BTreeMap::new();
        for (row, col, value) in cells {
            cells_by_row
                .entry(self.resolve(*row))
                .or_default()
                .insert(**col, value.as_str());
        }

        let mut edits = Vec::new();
        for (row, targets) in cells_by_row {
            let Some(layout) = self.row_layout(row) else {
                log::warn!("ignoring a request to write to the missing row {row:?}");
                continue;
            };
            let cell_count = layout.cell_ranges.len();
            // An empty table has no columns yet, so the first written cell defines them.
            let column_limit = if allow_new_columns || self.is_empty_table() {
                usize::MAX
            } else {
                self.contents.number_of_cols
            };

            let mut last_appended_col = None;
            for (&col, value) in &targets {
                if col >= column_limit {
                    log::warn!(
                        "ignoring a request to write to column {col}, past the table's \
                         {column_limit} columns"
                    );
                    continue;
                }
                match layout.cell_ranges.get(col) {
                    Some(cell_range) => edits.push(PendingEdit {
                        range: cell_range.clone(),
                        text: self.encode(value, self.is_quoted(cell_range)),
                        order: EditOrder::InPlace,
                    }),
                    None => last_appended_col = Some(col),
                }
            }

            if let Some(last_appended_col) = last_appended_col {
                let mut text = String::new();
                for col in cell_count..=last_appended_col {
                    // `col == 0` only happens on a row that has no cells at all.
                    if col > 0 {
                        text.push(self.delimiter);
                    }
                    if let Some(value) = targets.get(&col) {
                        text.push_str(&self.encode(value, false));
                    }
                }
                edits.push(PendingEdit {
                    range: layout.range.end..layout.range.end,
                    text,
                    order: EditOrder::AppendToRow,
                });
            }
        }
        edits
    }

    /// Sorts the edits and, when they would leave the buffer's last row without any text, keeps the
    /// row alive with a terminator. A row needs either content or a terminator to survive a reparse,
    /// so emptying the last row of a file that does not end in a newline would delete the row.
    fn finish_keeping_last_row(&self, edits: Vec<PendingEdit>) -> Vec<BufferEdit> {
        let mut edits = finish(edits);

        let buffer_len = self.snapshot.len();
        let Some(last_row) = self.last_row_range() else {
            return edits;
        };
        if last_row.end != buffer_len || last_row.is_empty() {
            return edits;
        }

        let mut remaining_len = 0;
        let mut cursor = last_row.start;
        for (range, text) in &edits {
            if range.end <= last_row.start {
                continue;
            }
            remaining_len += range.start.max(last_row.start).saturating_sub(cursor) + text.len();
            cursor = cursor.max(range.end);
        }
        remaining_len += last_row.end.saturating_sub(cursor);

        if remaining_len == 0 {
            edits.push((buffer_len..buffer_len, "\n".to_owned()));
        }
        edits
    }

    fn last_row_range(&self) -> Option<Range<usize>> {
        self.contents
            .row_spans
            .last()
            .or(self.contents.header_span.as_ref())
            .map(|span| self.span_offsets(span))
    }

    fn row_exists(&self, row: RowRef) -> bool {
        match row {
            RowRef::Header => self.contents.header_span.is_some(),
            RowRef::Data(data_row) => {
                *data_row < self.contents.rows.len() || (self.is_empty_table() && *data_row == 0)
            }
        }
    }

    fn encode(&self, value: &str, keep_quotes: bool) -> String {
        let encoded = quote_field(value, self.delimiter);
        if keep_quotes && !encoded.starts_with('"') {
            always_quoted(value)
        } else {
            encoded
        }
    }

    fn is_quoted(&self, cell_range: &Range<usize>) -> bool {
        cell_range.len() >= 2 && self.snapshot.chars_at(cell_range.start).next() == Some('"')
    }

    fn new_row_text(&self, left_col: usize, values: &[String]) -> String {
        let mut text = self.delimiter.to_string().repeat(left_col);
        for (offset, value) in values.iter().enumerate() {
            if offset > 0 {
                text.push(self.delimiter);
            }
            text.push_str(&quote_field(value, self.delimiter));
        }
        text
    }

    fn blank_row_text(&self) -> String {
        self.delimiter
            .to_string()
            .repeat(self.contents.number_of_cols.saturating_sub(1))
    }

    /// A buffer with nothing in it still has one editable cell in the grid; both the header and the
    /// first data row address it.
    fn is_empty_table(&self) -> bool {
        self.contents.rows.is_empty()
            && self.contents.header_span.is_none()
            && self.contents.number_of_cols == 0
    }

    fn resolve(&self, row: RowRef) -> RowRef {
        match row {
            RowRef::Header if self.is_empty_table() => RowRef::Data(DataRow(0)),
            row => row,
        }
    }

    fn all_rows(&self) -> Vec<RowRef> {
        let mut rows = Vec::with_capacity(self.contents.rows.len() + 1);
        if self.contents.header_span.is_some() {
            rows.push(RowRef::Header);
        }
        rows.extend((0..self.contents.rows.len()).map(|index| RowRef::Data(DataRow(index))));
        rows
    }

    fn row_cells(&self, row: RowRef) -> Option<&TableRow<TableCell>> {
        match row {
            RowRef::Header => self
                .contents
                .header_span
                .is_some()
                .then_some(&self.contents.headers),
            RowRef::Data(data_row) => self.contents.rows.get(*data_row),
        }
    }

    fn row_layout(&self, row: RowRef) -> Option<RowLayout> {
        if self.is_empty_table() {
            return match row {
                RowRef::Data(DataRow(0)) => Some(RowLayout {
                    range: 0..0,
                    cell_ranges: Vec::new(),
                }),
                _ => None,
            };
        }

        let (span, cells) = match row {
            RowRef::Header => (self.contents.header_span.as_ref()?, &self.contents.headers),
            RowRef::Data(data_row) => (
                self.contents.row_spans.get(*data_row)?,
                self.contents.rows.get(*data_row)?,
            ),
        };

        Some(RowLayout {
            range: self.span_offsets(span),
            // Real cells always precede the virtual padding, so stopping at the first virtual cell
            // yields exactly the cells that exist in the buffer.
            cell_ranges: cells
                .as_slice()
                .iter()
                .map_while(|cell| match cell {
                    TableCell::Real { position, .. } => Some(self.span_offsets(position)),
                    TableCell::Virtual => None,
                })
                .collect(),
        })
    }

    fn last_row_end(&self) -> Option<usize> {
        self.last_row_range().map(|range| range.end)
    }

    fn span_offsets(&self, span: &CellContentSpan) -> Range<usize> {
        span.start.to_offset(self.snapshot)..span.end.to_offset(self.snapshot)
    }
}

fn repeat_rows_after(row_text: &str, count: usize) -> String {
    let mut text = String::new();
    for _ in 0..count {
        text.push('\n');
        text.push_str(row_text);
    }
    text
}

fn merge_ranges(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.sort_by_key(|range| (range.start, range.end));

    let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    merged.retain(|range| !range.is_empty());
    merged
}

fn finish(mut edits: Vec<PendingEdit>) -> Vec<BufferEdit> {
    edits.sort_by(|left, right| {
        left.range
            .start
            .cmp(&right.range.start)
            .then(left.order.cmp(&right.order))
    });
    edits.dedup_by(|later, earlier| later.range == earlier.range && later.text == earlier.text);

    edits
        .into_iter()
        .filter(|edit| !edit.text.is_empty() || !edit.range.is_empty())
        .map(|edit| (edit.range, edit.text))
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::parser::from_buffer_with_delimiter;
    use text::{Buffer, BufferId, ReplicaId};

    pub(crate) fn new_buffer(text: &str) -> Buffer {
        let buffer_id = BufferId::new(1).expect("1 is a valid buffer id");
        Buffer::new(ReplicaId::LOCAL, buffer_id, text.to_string())
    }

    pub(crate) fn span_offsets(span: &CellContentSpan, snapshot: &BufferSnapshot) -> Range<usize> {
        span.start.to_offset(snapshot)..span.end.to_offset(snapshot)
    }

    fn apply(text: &str, edits: Vec<BufferEdit>) -> String {
        let mut buffer = new_buffer(text);
        if !edits.is_empty() {
            buffer.edit(edits);
        }
        buffer.text()
    }

    /// Runs `build` against the parsed contents of `text` and applies the resulting edits to it.
    fn edited(
        text: &str,
        delimiter: char,
        first_row_is_header: bool,
        build: impl FnOnce(&EditEngine<'_>) -> Vec<BufferEdit>,
    ) -> String {
        let buffer = new_buffer(text);
        let snapshot = buffer.snapshot().clone();
        let contents = from_buffer_with_delimiter(&snapshot, delimiter, first_row_is_header);
        let edits = build(&EditEngine::new(&contents, &snapshot, delimiter));
        assert_ascending_and_disjoint(&edits);
        apply(text, edits)
    }

    fn assert_ascending_and_disjoint(edits: &[BufferEdit]) {
        let mut previous_end = 0;
        for (range, _) in edits {
            assert!(
                range.start >= previous_end,
                "edits must be ascending and non-overlapping, got {edits:?}"
            );
            previous_end = range.end;
        }
    }

    fn csv(text: &str, build: impl FnOnce(&EditEngine<'_>) -> Vec<BufferEdit>) -> String {
        edited(text, ',', true, build)
    }

    fn data(row: usize) -> RowRef {
        RowRef::Data(DataRow(row))
    }

    #[test]
    fn quote_field_only_quotes_when_required() {
        assert_eq!(quote_field("plain", ','), "plain");
        assert_eq!(quote_field("", ','), "");
        assert_eq!(quote_field("  padded  ", ','), "  padded  ");
        assert_eq!(quote_field("a,b", ','), r#""a,b""#);
        assert_eq!(quote_field("a,b", ';'), "a,b");
        assert_eq!(quote_field("a;b", ';'), r#""a;b""#);
        assert_eq!(quote_field(r#"say "hi""#, ','), r#""say ""hi""""#);
        assert_eq!(quote_field("two\nlines", ','), "\"two\nlines\"");
        assert_eq!(quote_field("carriage\rreturn", ','), "\"carriage\rreturn\"");
        assert_eq!(quote_field("tabbed\there", ','), "tabbed\there");
        assert_eq!(quote_field("tabbed\there", '\t'), "\"tabbed\there\"");
    }

    #[test]
    fn block_to_clipboard_text_matches_spreadsheet_format() {
        let block = vec![
            vec!["a".to_string(), "b".to_string()],
            vec!["with\ttab".to_string(), "with\"quote".to_string()],
            vec!["multi\nline".to_string()],
        ];

        assert_eq!(
            block_to_clipboard_text(&block),
            "a\tb\n\"with\ttab\"\t\"with\"\"quote\"\n\"multi\nline\""
        );
        assert_eq!(block_to_clipboard_text(&Vec::new()), "");
    }

    #[test]
    fn clipboard_text_round_trips_through_the_editor_parser() {
        let blocks = [
            vec![vec!["a".to_string(), "b".to_string()]],
            vec![vec!["a".to_string()], vec!["b".to_string()]],
            vec![
                vec!["with\ttab".to_string(), "with,comma".to_string()],
                vec!["with\"quote".to_string(), "multi\nline".to_string()],
            ],
            vec![
                vec![String::new(), "b".to_string()],
                vec!["c".to_string(), String::new()],
            ],
            vec![
                vec!["名前".to_string(), "説明".to_string()],
                vec!["言った \"ね\"".to_string(), "改行\nあり".to_string()],
            ],
            // A single column block whose cells hold commas would be split on them if the payload
            // had no quotes at all, because the parser falls back to a comma separator.
            vec![vec!["a,b".to_string()], vec!["c".to_string()]],
        ];

        for block in blocks {
            let text = block_to_clipboard_text(&block);
            assert_eq!(
                clipboard_text_to_block(&text),
                block,
                "round trip failed for {block:?} rendered as {text:?}"
            );
        }
    }

    #[test]
    fn clipboard_text_round_trips_a_single_cell() {
        for value in [
            "plain",
            "",
            "with\ttab",
            "with,comma",
            "say \"hi\"",
            "multi\nline",
            "日本語のセル",
        ] {
            let block = vec![vec![value.to_string()]];
            let text = block_to_clipboard_text(&block);
            assert_eq!(
                clipboard_text_to_block(&text),
                block,
                "round trip failed for {value:?} rendered as {text:?}"
            );
        }
    }

    #[test]
    fn set_cell_replaces_a_real_cell() {
        assert_eq!(
            csv("a,b\nc,d", |engine| engine.set_cell(
                data(0),
                AnyColumn(1),
                "changed"
            )),
            "a,b\nc,changed"
        );
    }

    #[test]
    fn set_cell_quotes_a_value_that_needs_it() {
        assert_eq!(
            csv("a,b\nc,d", |engine| engine.set_cell(
                data(0),
                AnyColumn(0),
                "x,y"
            )),
            "a,b\n\"x,y\",d"
        );
    }

    #[test]
    fn set_cell_keeps_the_quotes_of_an_already_quoted_cell() {
        assert_eq!(
            csv("a,b\n\"c\",d", |engine| engine.set_cell(
                data(0),
                AnyColumn(0),
                "plain"
            )),
            "a,b\n\"plain\",d"
        );
    }

    #[test]
    fn set_cell_writes_into_an_empty_cell() {
        assert_eq!(
            csv("a,b\n,d", |engine| engine.set_cell(
                data(0),
                AnyColumn(0),
                "x"
            )),
            "a,b\nx,d"
        );
    }

    #[test]
    fn set_cell_writes_into_the_header() {
        assert_eq!(
            csv("a,b\nc,d", |engine| engine.set_cell(
                RowRef::Header,
                AnyColumn(1),
                "renamed"
            )),
            "a,renamed\nc,d"
        );
    }

    #[test]
    fn set_cell_materializes_a_virtual_cell() {
        assert_eq!(
            csv("a,b,c\nd", |engine| engine.set_cell(
                data(0),
                AnyColumn(2),
                "z"
            )),
            "a,b,c\nd,,z"
        );
    }

    #[test]
    fn set_cell_materializes_a_virtual_cell_before_a_trailing_newline() {
        assert_eq!(
            csv("a,b,c\nd\n", |engine| engine.set_cell(
                data(0),
                AnyColumn(2),
                "z"
            )),
            "a,b,c\nd,,z\n"
        );
    }

    #[test]
    fn set_cells_merges_virtual_cells_of_one_row() {
        assert_eq!(
            csv("a,b,c,d\ne", |engine| engine.set_cells(&[
                (data(0), AnyColumn(1), "two".to_string()),
                (data(0), AnyColumn(3), "four".to_string()),
            ])),
            "a,b,c,d\ne,two,,four"
        );
    }

    #[test]
    fn set_cells_combines_in_place_and_appended_cells() {
        assert_eq!(
            csv("a,b,c\nd,e", |engine| engine.set_cells(&[
                (data(0), AnyColumn(1), "E".to_string()),
                (data(0), AnyColumn(2), "F".to_string()),
                (RowRef::Header, AnyColumn(0), "A".to_string()),
            ])),
            "A,b,c\nd,E,F"
        );
    }

    #[test]
    fn set_cell_ignores_out_of_range_positions() {
        assert_eq!(
            csv("a,b\nc,d", |engine| engine.set_cell(
                data(7),
                AnyColumn(0),
                "x"
            )),
            "a,b\nc,d"
        );
        assert_eq!(
            csv("a,b\nc,d", |engine| engine.set_cell(
                data(0),
                AnyColumn(9),
                "x"
            )),
            "a,b\nc,d"
        );
        assert_eq!(
            edited("a,b\nc,d", ',', false, |engine| engine.set_cell(
                RowRef::Header,
                AnyColumn(0),
                "x"
            )),
            "a,b\nc,d"
        );
    }

    #[test]
    fn set_cell_starts_an_empty_buffer() {
        assert_eq!(
            csv("", |engine| engine.set_cell(data(0), AnyColumn(0), "x")),
            "x"
        );
        assert_eq!(
            csv("", |engine| engine.set_cell(data(0), AnyColumn(2), "x")),
            ",,x"
        );
        assert_eq!(
            csv("", |engine| engine.set_cell(
                RowRef::Header,
                AnyColumn(1),
                "h"
            )),
            ",h"
        );
    }

    #[test]
    fn emptying_the_last_row_keeps_it_alive() {
        // Without the added newline the now empty last line would not parse as a row at all.
        assert_eq!(
            csv("a\nb\nc", |engine| engine.set_cell(
                data(1),
                AnyColumn(0),
                ""
            )),
            "a\nb\n\n"
        );
        assert_eq!(
            csv("a\nb\nc", |engine| engine
                .clear_cells(&[(data(1), AnyColumn(0))])),
            "a\nb\n\n"
        );
        assert_eq!(
            csv("a,b\nc,d\ne,f", |engine| engine.clear_cells(&[
                (data(1), AnyColumn(0)),
                (data(1), AnyColumn(1)),
            ])),
            // The delimiter keeps the row on the line, so no terminator has to be added.
            "a,b\nc,d\n,"
        );
        // A file that already ends in a newline needs nothing added.
        assert_eq!(
            csv("a\nb\nc\n", |engine| engine.set_cell(
                data(1),
                AnyColumn(0),
                ""
            )),
            "a\nb\n\n"
        );
        // Rows that keep some text are left alone.
        assert_eq!(
            csv("a,b\nc,d\ne,f", |engine| engine
                .clear_cells(&[(data(1), AnyColumn(0))])),
            "a,b\nc,d\n,f"
        );
    }

    #[test]
    fn clear_cells_empties_real_cells_only() {
        assert_eq!(
            csv("a,b,c\nd,\"e\",f", |engine| engine.clear_cells(&[
                (data(0), AnyColumn(1)),
                (data(0), AnyColumn(2)),
                (RowRef::Header, AnyColumn(0)),
            ])),
            ",b,c\nd,,"
        );
        assert_eq!(
            csv("a,b,c\nd", |engine| engine
                .clear_cells(&[(data(0), AnyColumn(2))])),
            "a,b,c\nd"
        );
    }

    #[test]
    fn insert_rows_before_a_row() {
        assert_eq!(
            csv("a,b\nc,d\n", |engine| engine
                .insert_rows(RowInsertPosition::Before(data(0)), 1)),
            "a,b\n,\nc,d\n"
        );
        // A row above the header would become the new header, so it is refused.
        assert_eq!(
            csv("a,b\nc,d\n", |engine| engine
                .insert_rows(RowInsertPosition::Before(RowRef::Header), 2)),
            "a,b\nc,d\n"
        );
        // Without a header row the first data row can take a row above it.
        assert_eq!(
            edited("a,b\nc,d\n", ',', false, |engine| engine
                .insert_rows(RowInsertPosition::Before(data(0)), 1)),
            ",\na,b\nc,d\n"
        );
    }

    #[test]
    fn insert_rows_after_a_row() {
        assert_eq!(
            csv("a,b\nc,d\n", |engine| engine
                .insert_rows(RowInsertPosition::After(RowRef::Header), 1)),
            "a,b\n,\nc,d\n"
        );
    }

    #[test]
    fn insert_rows_at_end_keeps_the_trailing_newline_as_it_was() {
        assert_eq!(
            csv("a,b\nc,d\n", |engine| engine
                .insert_rows(RowInsertPosition::AtEnd, 2)),
            "a,b\nc,d\n,\n,\n"
        );
        assert_eq!(
            csv("a,b\nc,d", |engine| engine
                .insert_rows(RowInsertPosition::AtEnd, 1)),
            "a,b\nc,d\n,"
        );
    }

    #[test]
    fn insert_rows_into_an_empty_buffer() {
        assert_eq!(
            csv("", |engine| engine.insert_rows(RowInsertPosition::AtEnd, 2)),
            "\n\n"
        );
    }

    #[test]
    fn insert_rows_after_the_header_of_a_headers_only_file() {
        assert_eq!(
            csv("a,b,c", |engine| engine
                .insert_rows(RowInsertPosition::AtEnd, 1)),
            "a,b,c\n,,"
        );
    }

    #[test]
    fn delete_rows_takes_the_line_with_them() {
        assert_eq!(
            csv("a,b\nc,d\ne,f\n", |engine| engine.delete_rows(&[data(0)])),
            "a,b\ne,f\n"
        );
        assert_eq!(
            csv("a,b\nc,d\ne,f", |engine| engine.delete_rows(&[data(1)])),
            "a,b\nc,d"
        );
        assert_eq!(
            csv("a,b\nc,d\ne,f", |engine| engine
                .delete_rows(&[RowRef::Header])),
            "c,d\ne,f"
        );
        // The last row of a file that ends in a newline keeps that newline for its predecessor.
        assert_eq!(
            csv("a,b\nc,d\n", |engine| engine.delete_rows(&[data(0)])),
            "a,b\n"
        );
    }

    #[test]
    fn delete_rows_does_not_swallow_a_blank_row_above_the_last_one() {
        assert_eq!(
            csv("a,b\nc,d\n\ne,f", |engine| engine.delete_rows(&[data(2)])),
            "a,b\nc,d\n\n"
        );
        // Deleting the blank row too still collapses both lines.
        assert_eq!(
            csv("a,b\nc,d\n\ne,f", |engine| engine
                .delete_rows(&[data(1), data(2)])),
            "a,b\nc,d"
        );
        // A blank first line is a row of its own and must survive its neighbour's deletion.
        assert_eq!(csv("\na", |engine| engine.delete_rows(&[data(0)])), "\n");
    }

    #[test]
    fn delete_rows_handles_several_rows_at_once() {
        assert_eq!(
            csv("a,b\nc,d\ne,f\ng,h", |engine| engine
                .delete_rows(&[data(0), data(2)])),
            "a,b\ne,f"
        );
        assert_eq!(
            csv("a,b\nc,d\ne,f", |engine| engine
                .delete_rows(&[data(0), data(1)])),
            "a,b"
        );
    }

    #[test]
    fn delete_rows_of_the_only_row_keeps_the_line() {
        assert_eq!(
            csv("a,b", |engine| engine.delete_rows(&[RowRef::Header])),
            ""
        );
    }

    #[test]
    fn delete_rows_removes_a_multiline_row_whole() {
        assert_eq!(
            csv("h1,h2\n\"x\ny\",b\nc,d", |engine| engine
                .delete_rows(&[data(0)])),
            "h1,h2\nc,d"
        );
    }

    #[test]
    fn insert_columns_at_the_front() {
        assert_eq!(
            csv("a,b\nc,d", |engine| engine.insert_columns(0, 1)),
            ",a,b\n,c,d"
        );
    }

    #[test]
    fn insert_columns_in_the_middle() {
        assert_eq!(
            csv("a,b,c\nd,e,f", |engine| engine.insert_columns(1, 2)),
            "a,,,b,c\nd,,,e,f"
        );
    }

    #[test]
    fn insert_columns_at_the_end_pads_short_rows() {
        assert_eq!(
            csv("a,b,c\nd\ne,f,g", |engine| engine.insert_columns(3, 1)),
            "a,b,c,\nd,,,\ne,f,g,"
        );
    }

    #[test]
    fn insert_columns_rejects_an_index_past_the_table() {
        assert_eq!(
            csv("a,b\nc,d", |engine| engine.insert_columns(3, 1)),
            "a,b\nc,d"
        );
    }

    #[test]
    fn delete_columns_removes_the_cell_and_one_delimiter() {
        assert_eq!(
            csv("a,b,c\nd,e,f", |engine| engine
                .delete_columns(&[AnyColumn(1)])),
            "a,c\nd,f"
        );
        assert_eq!(
            csv("a,b,c\nd,e,f", |engine| engine
                .delete_columns(&[AnyColumn(2)])),
            "a,b\nd,e"
        );
    }

    #[test]
    fn delete_columns_of_the_only_cell_keeps_the_row() {
        // The third newline keeps the now empty last row from disappearing on the next parse.
        assert_eq!(
            csv("a\nb\nc", |engine| engine.delete_columns(&[AnyColumn(0)])),
            "\n\n\n"
        );
        assert_eq!(
            csv("a\nb\nc\n", |engine| engine.delete_columns(&[AnyColumn(0)])),
            "\n\n\n"
        );
    }

    #[test]
    fn delete_columns_takes_one_delimiter_per_run() {
        assert_eq!(
            csv("a,b,c\nd,e,f", |engine| engine
                .delete_columns(&[AnyColumn(1), AnyColumn(2)])),
            "a\nd"
        );
        assert_eq!(
            csv("a,b,c,d\ne,f,g\nh,i", |engine| engine
                .delete_columns(&[AnyColumn(2), AnyColumn(3)])),
            "a,b\ne,f\nh,i"
        );
        assert_eq!(
            csv("a,b,c,d\ne,f,g,h", |engine| engine
                .delete_columns(&[AnyColumn(0), AnyColumn(1)])),
            "c,d\ng,h"
        );
        // Two separate runs each take their own delimiter.
        assert_eq!(
            csv("a,b,c,d,e\nf,g,h,i,j", |engine| engine.delete_columns(&[
                AnyColumn(0),
                AnyColumn(2),
                AnyColumn(3)
            ])),
            "b,e\ng,j"
        );
    }

    #[test]
    fn delete_columns_handles_several_columns_and_short_rows() {
        assert_eq!(
            csv("a,b,c,d\ne,f\ng,h,i,j", |engine| engine
                .delete_columns(&[AnyColumn(1), AnyColumn(3)])),
            "a,c\ne\ng,i"
        );
    }

    #[test]
    fn paste_block_rows_writes_into_existing_cells() {
        assert_eq!(
            csv(
                "a,b,c
d,e,f
g,h,i",
                |engine| engine.paste_block_rows(
                    &[data(0), data(1)],
                    AnyColumn(1),
                    &vec![
                        vec!["1".to_string(), "2".to_string()],
                        vec!["3".to_string(), "4".to_string()],
                    ]
                )
            ),
            "a,b,c
d,1,2
g,3,4"
        );
    }

    #[test]
    fn paste_block_rows_follows_the_display_order_it_is_given() {
        // What a sorted or filtered table hands over: display order, not data order.
        assert_eq!(
            csv(
                "h1,h2
a,b
c,d
e,f",
                |engine| engine.paste_block_rows(
                    &[data(2), data(0)],
                    AnyColumn(0),
                    &vec![
                        vec!["X".to_string(), "Y".to_string()],
                        vec!["Z".to_string(), "W".to_string()],
                    ]
                )
            ),
            "h1,h2
Z,W
c,d
X,Y"
        );
    }

    #[test]
    fn paste_block_rows_appends_missing_rows() {
        assert_eq!(
            csv(
                "a,b
c,d
",
                |engine| engine.paste_block_rows(
                    &[data(0)],
                    AnyColumn(0),
                    &vec![
                        vec!["1".to_string(), "2".to_string()],
                        vec!["3".to_string(), "4".to_string()],
                        vec!["5".to_string(), "6".to_string()],
                    ]
                )
            ),
            "a,b
1,2
3,4
5,6
"
        );
    }

    #[test]
    fn paste_block_rows_skips_missing_and_repeated_rows() {
        assert_eq!(
            csv(
                "a,b
c,d
e,f",
                |engine| engine.paste_block_rows(
                    &[data(0), data(9), data(0)],
                    AnyColumn(0),
                    &vec![
                        vec!["1".to_string()],
                        vec!["2".to_string()],
                        vec!["3".to_string()],
                    ]
                )
            ),
            "a,b
1,d
e,f"
        );
    }

    #[test]
    fn paste_block_rows_grows_rows_that_are_too_short() {
        assert_eq!(
            csv(
                "a,b
c,d",
                |engine| engine.paste_block_rows(
                    &[data(0)],
                    AnyColumn(1),
                    &vec![vec!["1".to_string(), "2".to_string(), "3".to_string()]]
                )
            ),
            "a,b
c,1,2,3"
        );
    }

    #[test]
    fn paste_block_rows_starting_at_the_header_offsets_columns() {
        assert_eq!(
            csv(
                "a,b,c
d,e,f",
                |engine| engine.paste_block_rows(
                    &[RowRef::Header, data(0)],
                    AnyColumn(2),
                    &vec![
                        vec!["x".to_string()],
                        vec!["y".to_string()],
                        vec!["z".to_string()],
                    ]
                )
            ),
            "a,b,x
d,e,y
,,z"
        );
    }

    #[test]
    fn paste_block_rows_quotes_values_that_need_it() {
        assert_eq!(
            csv(
                "a,b
c,d",
                |engine| engine.paste_block_rows(
                    &[data(0)],
                    AnyColumn(0),
                    &vec![vec!["x,y".to_string()], vec!["say \"hi\"".to_string()],]
                )
            ),
            "a,b
\"x,y\",d
\"say \"\"hi\"\"\""
        );
    }

    #[test]
    fn paste_block_rows_fills_an_empty_buffer() {
        assert_eq!(
            csv("", |engine| engine.paste_block_rows(
                &[data(0)],
                AnyColumn(0),
                &vec![
                    vec!["1".to_string(), "2".to_string()],
                    vec!["3".to_string(), "4".to_string()],
                ]
            )),
            "1,2
3,4"
        );
    }

    #[test]
    fn copy_block_reads_values_and_pads_with_empties() {
        let buffer = new_buffer("a,b,c\nd,e\nf,g,h");
        let snapshot = buffer.snapshot().clone();
        let contents = from_buffer_with_delimiter(&snapshot, ',', true);
        let engine = EditEngine::new(&contents, &snapshot, ',');

        assert_eq!(
            engine.copy_block(&[RowRef::Header, data(0), data(1)], 1..3),
            vec![
                vec!["b".to_string(), "c".to_string()],
                vec!["e".to_string(), String::new()],
                vec!["g".to_string(), "h".to_string()],
            ]
        );
        assert_eq!(
            engine.copy_block(&[data(9)], 0..2),
            vec![vec![String::new(), String::new()]]
        );
    }

    #[test]
    fn copy_block_unescapes_quoted_values() {
        let buffer = new_buffer("a,b\n\"x,y\",\"say \"\"hi\"\"\"");
        let snapshot = buffer.snapshot().clone();
        let contents = from_buffer_with_delimiter(&snapshot, ',', true);
        let engine = EditEngine::new(&contents, &snapshot, ',');

        let block = engine.copy_block(&[data(0)], 0..2);
        assert_eq!(
            block,
            vec![vec!["x,y".to_string(), "say \"hi\"".to_string()]]
        );
        assert_eq!(
            block_to_clipboard_text(&block),
            "\"x,y\"\t\"say \"\"hi\"\"\""
        );
        assert_eq!(
            clipboard_text_to_block(&block_to_clipboard_text(&block)),
            block
        );
    }

    #[test]
    fn edits_use_the_configured_delimiter() {
        assert_eq!(
            edited("a\tb\tc\nd\te", '\t', true, |engine| engine.set_cell(
                data(0),
                AnyColumn(2),
                "x\ty"
            )),
            "a\tb\tc\nd\te\t\"x\ty\""
        );
        assert_eq!(
            edited("a\tb\nc\td", '\t', true, |engine| engine.set_cell(
                data(0),
                AnyColumn(1),
                "x,y"
            )),
            "a\tb\nc\tx,y"
        );
    }

    #[test]
    fn blank_rows_are_editable() {
        assert_eq!(
            csv("a,b\n\nc,d", |engine| engine.set_cell(
                data(0),
                AnyColumn(1),
                "x"
            )),
            "a,b\n,x\nc,d"
        );
        assert_eq!(
            csv("a,b\n\nc,d", |engine| engine.delete_rows(&[data(0)])),
            "a,b\nc,d"
        );
    }

    #[test]
    fn multiline_cells_round_trip() {
        assert_eq!(
            csv("a,b\n\"x\ny\",d", |engine| engine.set_cell(
                data(0),
                AnyColumn(0),
                "p\nq"
            )),
            "a,b\n\"p\nq\",d"
        );
        assert_eq!(
            csv("a,b\n\"x\ny\",d", |engine| engine.set_cell(
                data(0),
                AnyColumn(1),
                "z"
            )),
            "a,b\n\"x\ny\",z"
        );
    }

    #[test]
    fn edits_are_computed_for_a_headerless_table() {
        assert_eq!(
            edited("a,b\nc,d", ',', false, |engine| engine.set_cell(
                data(0),
                AnyColumn(0),
                "x"
            )),
            "x,b\nc,d"
        );
        assert_eq!(
            edited("a,b\nc,d", ',', false, |engine| engine
                .delete_rows(&[data(0)])),
            "c,d"
        );
    }
}
