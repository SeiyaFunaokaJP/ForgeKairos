//! Comparing two tables: which rows line up with which, which rows were added
//! or removed, and which cells inside a pair of rows differ.
//!
//! Kept free of context and window handles so that the view can hand a
//! comparison to a background thread, and so that the alignment rules can be
//! tested without a window.

use std::ops::Range;

use imara_diff::{Algorithm, Diff, InternedInput, Interner};
use ui::SharedString;

/// Rows past this many are left out of the comparison on either side. A table
/// that long costs more to align and to hold aligned than the comparison is
/// worth, so the view says what it left out instead of freezing the window.
pub(crate) const MAX_DIFF_ROWS: usize = 100_000;

/// Fraction of a row's cells that has to be identical for two rows inside one
/// change to be shown as a single changed row rather than as a removal followed
/// by an addition.
const MODIFIED_ROW_SIMILARITY: f32 = 0.5;

/// Longest cell the character level comparison runs on. Past this the whole
/// value is reported as differing, because the character diff is quadratic in
/// the worst case and a cell this long is unreadable in a table cell anyway.
pub(crate) const MAX_CELL_DIFF_BYTES: usize = 512;

/// The value a row has at `column`, where a row that stops short of that column
/// reads as empty rather than as missing: a row of two cells and a row of three
/// whose last cell is empty hold the same values.
pub(crate) fn cell_at(row: &[SharedString], column: usize) -> &str {
    row.get(column).map(SharedString::as_ref).unwrap_or("")
}

/// A row with its trailing empty cells cut off, which is the form two rows are
/// matched up in: by [`cell_at`] a row that grew an empty last cell holds the
/// same values as it did before, so it must not read as a different row either.
fn significant_cells(row: &[SharedString]) -> &[SharedString] {
    let mut end = row.len();
    while end > 0 && row.get(end - 1).is_some_and(|cell| cell.is_empty()) {
        end -= 1;
    }
    row.get(..end).unwrap_or(row)
}

/// One side of a comparison, parsed into plain cell values.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct DiffSide {
    /// The first line, when the grid reads it as a header. Compared against the
    /// other side's header rather than taking part in the row alignment, so
    /// that a table whose header changed does not read as every row changing.
    pub(crate) header: Option<Vec<SharedString>>,
    pub(crate) rows: Vec<Vec<SharedString>>,
    /// Rows dropped because this side is longer than [`MAX_DIFF_ROWS`].
    pub(crate) dropped_rows: usize,
}

impl DiffSide {
    pub(crate) fn parse(text: &str, delimiter: char, first_row_is_header: bool) -> Self {
        let mut rows = crate::parser::parse_cell_rows(text, delimiter);
        let header = if first_row_is_header && !rows.is_empty() {
            Some(rows.remove(0))
        } else {
            None
        };
        let dropped_rows = rows.len().saturating_sub(MAX_DIFF_ROWS);
        rows.truncate(MAX_DIFF_ROWS);
        Self {
            header,
            rows,
            dropped_rows,
        }
    }

    /// A side built straight from cell values, which is how a comparison of two
    /// blocks of cells taken out of one grid is expressed.
    pub(crate) fn from_rows(rows: Vec<Vec<SharedString>>) -> Self {
        Self {
            header: None,
            rows,
            dropped_rows: 0,
        }
    }

    pub(crate) fn column_count(&self) -> usize {
        self.header
            .iter()
            .map(|header| header.len())
            .chain(self.rows.iter().map(|row| row.len()))
            .max()
            .unwrap_or(0)
    }

    /// The number the view puts in front of a row, counting the header as the
    /// first row of the file the way the grid's row numbers do.
    pub(crate) fn row_number(&self, row: usize) -> usize {
        row + 1 + usize::from(self.header.is_some())
    }

    pub(crate) fn row(&self, row: usize) -> &[SharedString] {
        self.rows.get(row).map(Vec::as_slice).unwrap_or(&[])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffRowKind {
    Unchanged,
    /// Only the right hand side has this row.
    Added,
    /// Only the left hand side has this row.
    Removed,
    /// Both sides have the row, and at least one cell in it differs.
    Modified,
}

impl DiffRowKind {
    pub(crate) fn is_change(self) -> bool {
        !matches!(self, DiffRowKind::Unchanged)
    }
}

/// One line of the aligned table: what the left side shows on it, what the
/// right side shows, and how the two relate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiffRow {
    pub(crate) kind: DiffRowKind,
    /// Index into the left side's rows; `None` on an added row, where the left
    /// hand pane shows nothing at all.
    pub(crate) left_row: Option<usize>,
    pub(crate) right_row: Option<usize>,
    /// Columns whose two values differ. Only filled for [`DiffRowKind::Modified`].
    pub(crate) changed_columns: Vec<usize>,
}

impl DiffRow {
    fn unchanged(left_row: usize, right_row: usize) -> Self {
        Self {
            kind: DiffRowKind::Unchanged,
            left_row: Some(left_row),
            right_row: Some(right_row),
            changed_columns: Vec::new(),
        }
    }

    fn removed(left_row: usize) -> Self {
        Self {
            kind: DiffRowKind::Removed,
            left_row: Some(left_row),
            right_row: None,
            changed_columns: Vec::new(),
        }
    }

    fn added(right_row: usize) -> Self {
        Self {
            kind: DiffRowKind::Added,
            left_row: None,
            right_row: Some(right_row),
            changed_columns: Vec::new(),
        }
    }
}

/// Two tables lined up against each other, row by row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TableDiff {
    /// The two header rows against each other, when either side has one.
    pub(crate) header: Option<DiffRow>,
    /// The body rows in display order. Both sides' row indices increase down
    /// this list, so that either pane read on its own is still the file's own
    /// order.
    pub(crate) rows: Vec<DiffRow>,
    pub(crate) column_count: usize,
    pub(crate) added_rows: usize,
    pub(crate) removed_rows: usize,
    pub(crate) modified_rows: usize,
    /// Whether either side was cut short at [`MAX_DIFF_ROWS`].
    pub(crate) truncated: bool,
}

impl TableDiff {
    pub(crate) fn compute(left: &DiffSide, right: &DiffSide) -> Self {
        let column_count = left.column_count().max(right.column_count());
        let header = align_headers(left, right, column_count);
        let rows = align_rows(left, right, column_count);

        let mut diff = Self {
            header,
            rows,
            column_count,
            added_rows: 0,
            removed_rows: 0,
            modified_rows: 0,
            truncated: left.dropped_rows > 0 || right.dropped_rows > 0,
        };
        for row in diff
            .rows
            .iter()
            .chain(diff.header.iter())
            .map(|row| row.kind)
        {
            match row {
                DiffRowKind::Unchanged => {}
                DiffRowKind::Added => diff.added_rows += 1,
                DiffRowKind::Removed => diff.removed_rows += 1,
                DiffRowKind::Modified => diff.modified_rows += 1,
            }
        }
        diff
    }

    pub(crate) fn change_count(&self) -> usize {
        self.added_rows + self.removed_rows + self.modified_rows
    }

    /// Indices into [`Self::rows`] of the first row of every run of changed
    /// rows, which is what "previous" and "next" step through: a block of five
    /// inserted rows is one stop rather than five.
    pub(crate) fn change_blocks(&self) -> Vec<usize> {
        let mut blocks = Vec::new();
        let mut previous_was_change = false;
        for (index, row) in self.rows.iter().enumerate() {
            let is_change = row.kind.is_change();
            if is_change && !previous_was_change {
                blocks.push(index);
            }
            previous_was_change = is_change;
        }
        blocks
    }

    pub(crate) fn changed_row_indices(&self) -> Vec<usize> {
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.kind.is_change())
            .map(|(index, _)| index)
            .collect()
    }
}

fn align_headers(left: &DiffSide, right: &DiffSide, column_count: usize) -> Option<DiffRow> {
    match (left.header.as_deref(), right.header.as_deref()) {
        (None, None) => None,
        (Some(_), None) => Some(DiffRow::removed(0)),
        (None, Some(_)) => Some(DiffRow::added(0)),
        (Some(left_header), Some(right_header)) => {
            let changed_columns = changed_columns(left_header, right_header, column_count);
            Some(if changed_columns.is_empty() {
                DiffRow::unchanged(0, 0)
            } else {
                DiffRow {
                    kind: DiffRowKind::Modified,
                    left_row: Some(0),
                    right_row: Some(0),
                    changed_columns,
                }
            })
        }
    }
}

fn align_rows(left: &DiffSide, right: &DiffSide, column_count: usize) -> Vec<DiffRow> {
    // Built field by field rather than with `InternedInput::new`, whose token
    // source trait is only implemented for text: the tokens here are whole
    // rows, compared cell by cell rather than as a line of text, so that two
    // rows written with different quoting intern to the same token.
    let mut input = InternedInput {
        before: Vec::with_capacity(left.rows.len()),
        after: Vec::with_capacity(right.rows.len()),
        interner: Interner::new(left.rows.len() + right.rows.len()),
    };
    input.update_before(left.rows.iter().map(|row| significant_cells(row)));
    input.update_after(right.rows.iter().map(|row| significant_cells(row)));
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    // Canonicalizes where an ambiguous change is placed - inserting a row equal
    // to its neighbour can be attributed to either position - so that the same
    // pair of files always lines up the same way.
    diff.postprocess_no_heuristic(&input);

    let mut rows = Vec::with_capacity(left.rows.len().max(right.rows.len()));
    let mut left_row = 0usize;
    let mut right_row = 0usize;
    for hunk in diff.hunks() {
        while left_row < hunk.before.start as usize {
            rows.push(DiffRow::unchanged(left_row, right_row));
            left_row += 1;
            right_row += 1;
        }

        pair_changed_rows(
            left,
            right,
            left_row..hunk.before.end as usize,
            right_row..hunk.after.end as usize,
            column_count,
            &mut rows,
        );
        left_row = hunk.before.end as usize;
        right_row = hunk.after.end as usize;
    }
    while left_row < left.rows.len() && right_row < right.rows.len() {
        rows.push(DiffRow::unchanged(left_row, right_row));
        left_row += 1;
        right_row += 1;
    }
    rows
}

/// Lines up the rows inside one change.
///
/// Both sides are walked in step, and a pair whose cells are alike enough is
/// shown as one changed row with the differing cells marked; a pair that is not
/// becomes a removal and an addition on two lines. Walking in step rather than
/// looking for the closest match anywhere in the change is what keeps each
/// pane in its own file's order, which is the whole point of showing them side
/// by side.
fn pair_changed_rows(
    left: &DiffSide,
    right: &DiffSide,
    removed: Range<usize>,
    added: Range<usize>,
    column_count: usize,
    rows: &mut Vec<DiffRow>,
) {
    let paired = removed.len().min(added.len());
    let mut unpaired_left = Vec::new();
    let mut unpaired_right = Vec::new();
    for offset in 0..paired {
        let left_row = removed.start + offset;
        let right_row = added.start + offset;
        let changed_columns =
            changed_columns(left.row(left_row), right.row(right_row), column_count);
        if rows_are_a_pair(changed_columns.len(), column_count) {
            flush_unpaired_rows(&mut unpaired_left, &mut unpaired_right, rows);
            rows.push(DiffRow {
                kind: DiffRowKind::Modified,
                left_row: Some(left_row),
                right_row: Some(right_row),
                changed_columns,
            });
        } else {
            unpaired_left.push(left_row);
            unpaired_right.push(right_row);
        }
    }
    unpaired_left.extend(removed.start + paired..removed.end);
    unpaired_right.extend(added.start + paired..added.end);
    flush_unpaired_rows(&mut unpaired_left, &mut unpaired_right, rows);
}

/// Writes out a run of rows that did not pair up, as the left side's removals
/// followed by the right side's additions - one block taken out and one put in,
/// the way a comparison shows a change with no row by row correspondence in it.
///
/// Flushed before every pair rather than once at the end of the change, because
/// a pair that comes after an unpaired run has higher row numbers than the run
/// does: emitting the run last would walk either pane's row numbers backwards.
fn flush_unpaired_rows(
    unpaired_left: &mut Vec<usize>,
    unpaired_right: &mut Vec<usize>,
    rows: &mut Vec<DiffRow>,
) {
    rows.extend(unpaired_left.drain(..).map(DiffRow::removed));
    rows.extend(unpaired_right.drain(..).map(DiffRow::added));
}

/// Whether two rows of a change are the same row with edits in it.
///
/// A table of one column has nothing to measure - the single cell differs, or
/// the rows would not be in the change at all - so there the rows in
/// corresponding positions are always taken as a pair, which is what makes an
/// edited value in a one column file read as a change rather than as a deleted
/// row next to an unrelated new one.
fn rows_are_a_pair(changed_columns: usize, column_count: usize) -> bool {
    if column_count <= 1 {
        return true;
    }
    let matching = column_count.saturating_sub(changed_columns);
    matching as f32 / column_count as f32 >= MODIFIED_ROW_SIMILARITY
}

fn changed_columns(
    left: &[SharedString],
    right: &[SharedString],
    column_count: usize,
) -> Vec<usize> {
    (0..column_count.max(left.len()).max(right.len()))
        .filter(|&column| cell_at(left, column) != cell_at(right, column))
        .collect()
}

/// The parts of two cell values that differ, as byte ranges into each of them.
///
/// Both sides are returned because the left pane highlights what was taken out
/// and the right pane what was put in, the way a text diff underlines the words
/// it changed inside a line.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct CellCharDiff {
    pub(crate) left: Vec<Range<usize>>,
    pub(crate) right: Vec<Range<usize>>,
}

pub(crate) fn cell_char_diff(left: &str, right: &str) -> CellCharDiff {
    if left == right {
        return CellCharDiff::default();
    }
    if left.len() > MAX_CELL_DIFF_BYTES || right.len() > MAX_CELL_DIFF_BYTES {
        return CellCharDiff {
            left: non_empty_range(left.len()),
            right: non_empty_range(right.len()),
        };
    }

    let left_characters = character_slices(left);
    let right_characters = character_slices(right);
    let mut input = InternedInput::default();
    input.update_before(left_characters.iter().copied());
    input.update_after(right_characters.iter().copied());
    let diff = Diff::compute(Algorithm::Histogram, &input);

    let left_offsets = character_offsets(left, &left_characters);
    let right_offsets = character_offsets(right, &right_characters);
    let mut result = CellCharDiff::default();
    for hunk in diff.hunks() {
        push_character_range(&mut result.left, &left_offsets, hunk.before);
        push_character_range(&mut result.right, &right_offsets, hunk.after);
    }
    result
}

fn non_empty_range(length: usize) -> Vec<Range<usize>> {
    if length == 0 {
        Vec::new()
    } else {
        vec![0..length]
    }
}

/// One `&str` per character, which is what the diff interns. Splitting on
/// characters rather than on bytes keeps every range it reports on a character
/// boundary, so the highlighted slices can be handed straight to the text
/// system.
fn character_slices(text: &str) -> Vec<&str> {
    let mut slices = Vec::new();
    for (start, character) in text.char_indices() {
        let end = start + character.len_utf8();
        if let Some(slice) = text.get(start..end) {
            slices.push(slice);
        }
    }
    slices
}

/// Byte offset of every character, plus the end of the string, so that a token
/// range `a..b` becomes the byte range `offsets[a]..offsets[b]`.
fn character_offsets(text: &str, slices: &[&str]) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(slices.len() + 1);
    let mut offset = 0;
    for slice in slices {
        offsets.push(offset);
        offset += slice.len();
    }
    offsets.push(text.len());
    offsets
}

fn push_character_range(ranges: &mut Vec<Range<usize>>, offsets: &[usize], tokens: Range<u32>) {
    if tokens.is_empty() {
        return;
    }
    let (Some(&start), Some(&end)) = (
        offsets.get(tokens.start as usize),
        offsets.get(tokens.end as usize),
    ) else {
        return;
    };
    match ranges.last_mut() {
        Some(last) if last.end == start => last.end = end,
        _ => ranges.push(start..end),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn side(text: &str) -> DiffSide {
        DiffSide::parse(text, ',', true)
    }

    fn side_without_header(text: &str) -> DiffSide {
        DiffSide::parse(text, ',', false)
    }

    /// The aligned rows as `left|right` with the kind, which is short enough to
    /// write the expected alignment out by hand.
    fn alignment(diff: &TableDiff) -> Vec<String> {
        diff.rows
            .iter()
            .map(|row| {
                let kind = match row.kind {
                    DiffRowKind::Unchanged => "=",
                    DiffRowKind::Added => "+",
                    DiffRowKind::Removed => "-",
                    DiffRowKind::Modified => "~",
                };
                let left = row
                    .left_row
                    .map(|row| row.to_string())
                    .unwrap_or_else(|| ".".to_owned());
                let right = row
                    .right_row
                    .map(|row| row.to_string())
                    .unwrap_or_else(|| ".".to_owned());
                format!("{kind}{left}|{right}")
            })
            .collect()
    }

    /// Neither pane's row numbers may walk backwards down the aligned list: a
    /// pane read on its own has to still be its own file, in its own order.
    fn assert_both_sides_stay_in_order(diff: &TableDiff) {
        let mut last_left: Option<usize> = None;
        let mut last_right: Option<usize> = None;
        for row in &diff.rows {
            if let Some(left) = row.left_row {
                assert!(
                    last_left.is_none_or(|last| last < left),
                    "the left pane went from row {last_left:?} back to row {left} in {:?}",
                    alignment(diff)
                );
                last_left = Some(left);
            }
            if let Some(right) = row.right_row {
                assert!(
                    last_right.is_none_or(|last| last < right),
                    "the right pane went from row {last_right:?} back to row {right} in {:?}",
                    alignment(diff)
                );
                last_right = Some(right);
            }
        }
    }

    #[test]
    fn an_unchanged_table_lines_every_row_up_with_itself() {
        let table = "name,value\na,1\nb,2\nc,3";
        let diff = TableDiff::compute(&side(table), &side(table));

        assert_eq!(alignment(&diff), ["=0|0", "=1|1", "=2|2"]);
        assert_eq!(diff.change_count(), 0);
        assert_eq!(diff.column_count, 2);
        assert!(diff.change_blocks().is_empty());
        assert_eq!(
            diff.header.as_ref().map(|header| header.kind),
            Some(DiffRowKind::Unchanged)
        );
    }

    #[test]
    fn an_edited_cell_becomes_one_changed_row() {
        let diff = TableDiff::compute(
            &side("name,value\na,1\nb,2"),
            &side("name,value\na,1\nb,99"),
        );

        assert_eq!(alignment(&diff), ["=0|0", "~1|1"]);
        assert_eq!(diff.modified_rows, 1);
        assert_eq!(diff.added_rows, 0);
        assert_eq!(diff.removed_rows, 0);
        let changed = diff
            .rows
            .last()
            .expect("the edited row should have been aligned");
        assert_eq!(changed.changed_columns, [1]);
    }

    #[test]
    fn an_inserted_row_leaves_the_other_side_blank() {
        let diff = TableDiff::compute(
            &side("name,value\na,1\nc,3"),
            &side("name,value\na,1\nb,2\nc,3"),
        );

        assert_eq!(alignment(&diff), ["=0|0", "+.|1", "=1|2"]);
        assert_eq!(diff.added_rows, 1);
        assert_eq!(diff.change_blocks(), [1]);
    }

    #[test]
    fn a_deleted_row_leaves_the_other_side_blank() {
        let diff = TableDiff::compute(
            &side("name,value\na,1\nb,2\nc,3"),
            &side("name,value\na,1\nc,3"),
        );

        assert_eq!(alignment(&diff), ["=0|0", "-1|.", "=2|1"]);
        assert_eq!(diff.removed_rows, 1);
    }

    /// Rows that only moved are two unrelated rows as far as a table
    /// comparison is concerned: one was taken out and another put in. What
    /// matters is that the rows around them still line up.
    #[test]
    fn reordered_rows_are_reported_as_a_removal_and_an_addition() {
        let diff = TableDiff::compute(
            &side("name,value\na,1\nb,2\nc,3"),
            &side("name,value\nc,3\na,1\nb,2"),
        );

        assert_eq!(alignment(&diff), ["+.|0", "=0|1", "=1|2", "-2|."]);
        assert_eq!(diff.added_rows, 1);
        assert_eq!(diff.removed_rows, 1);
        assert_both_sides_stay_in_order(&diff);
    }

    /// One change holding both rows that pair up and rows that do not: the ones
    /// that do not have to be written out where they sit rather than collected
    /// behind the pairs, or the row numbers in both panes jump backwards.
    #[test]
    fn a_change_that_only_partly_pairs_up_keeps_both_sides_in_order() {
        let diff = TableDiff::compute(
            &side("k,a,b,c\nx,1,2,3\ny,1,2,3\nz,1,2,3"),
            &side("k,a,b,c\nx,9,2,3\nqq,rr,ss,tt\nz,9,2,3"),
        );

        assert_eq!(alignment(&diff), ["~0|0", "-1|.", "+.|1", "~2|2"]);
        assert_both_sides_stay_in_order(&diff);
        assert_eq!(diff.modified_rows, 2);
        assert_eq!(diff.removed_rows, 1);
        assert_eq!(diff.added_rows, 1);
    }

    /// A change with more rows taken out than put in: the extra removals belong
    /// after the pairs on the left side, with nothing beside them on the right.
    #[test]
    fn a_change_with_more_removals_than_additions_keeps_the_leftovers() {
        let diff = TableDiff::compute(&side("h,v\na,1\nb,2\nc,3\nd,4"), &side("h,v\na,9\nd,4"));

        assert_eq!(alignment(&diff), ["~0|0", "-1|.", "-2|.", "=3|1"]);
        assert_both_sides_stay_in_order(&diff);
        assert_eq!(diff.removed_rows, 2);
        assert_eq!(diff.added_rows, 0);
    }

    /// A new column changes every row, and every one of them has to stay paired
    /// with the row it came from rather than turn into a delete and an insert.
    #[test]
    fn an_added_column_changes_every_row_in_place() {
        let diff = TableDiff::compute(&side("a,b\n1,2\n3,4"), &side("a,b,c\n1,2,x\n3,4,y"));

        assert_eq!(alignment(&diff), ["~0|0", "~1|1"]);
        assert_eq!(diff.column_count, 3);
        // The header changed too, and the counts are of everything that
        // changed rather than of body rows only.
        assert_eq!(diff.modified_rows, 3);
        for row in &diff.rows {
            assert_eq!(row.changed_columns, [2]);
        }
        assert_eq!(
            diff.header
                .as_ref()
                .map(|header| header.changed_columns.clone()),
            Some(vec![2])
        );
    }

    /// A row that lost its last cell, where the other side has nothing there at
    /// all, differs in that column: the value went away.
    #[test]
    fn a_dropped_trailing_cell_counts_as_a_changed_cell() {
        let diff = TableDiff::compute(&side("a,b\n1,2"), &side("a,b\n1"));

        assert_eq!(diff.column_count, 2);
        assert_eq!(alignment(&diff), ["~0|0"]);
        assert_eq!(
            diff.rows.first().map(|row| row.changed_columns.clone()),
            Some(vec![1])
        );
    }

    /// An empty trailing cell holds the same value as no cell at all, so a row
    /// that grew one must not read as changed.
    #[test]
    fn an_added_empty_trailing_cell_is_not_a_change() {
        let diff = TableDiff::compute(&side_without_header("1,2"), &side_without_header("1,2,"));

        assert_eq!(alignment(&diff), ["=0|0"]);
        assert_eq!(diff.change_count(), 0);
    }

    /// Two rows inside one change that share almost nothing are two rows, not
    /// one changed row, or the comparison would claim an edit between values
    /// that have nothing to do with each other.
    #[test]
    fn rows_that_share_too_little_are_not_paired() {
        let diff = TableDiff::compute(&side("a,b,c,d\n1,2,3,4"), &side("a,b,c,d\nw,x,y,z"));

        assert_eq!(alignment(&diff), ["-0|.", "+.|0"]);
        assert_eq!(diff.modified_rows, 0);
    }

    /// With a single column there is nothing to measure similarity against, so
    /// an edited value has to read as an edit.
    #[test]
    fn a_single_column_table_pairs_rows_by_position() {
        let diff = TableDiff::compute(&side("h\na\nb"), &side("h\na\nz"));

        assert_eq!(alignment(&diff), ["=0|0", "~1|1"]);
        assert_eq!(
            diff.rows.last().map(|row| row.changed_columns.clone()),
            Some(vec![0])
        );
    }

    /// Only the quoting differs, and the values behind it are the same, so the
    /// two files are the same table.
    #[test]
    fn quoting_that_does_not_change_a_value_is_not_a_difference() {
        let diff = TableDiff::compute(
            &side("name,note\na,\"plain\""),
            &side("name,note\n\"a\",plain"),
        );

        assert_eq!(diff.change_count(), 0);
        assert_eq!(alignment(&diff), ["=0|0"]);
    }

    /// A quoted line terminator belongs to the cell that holds it, so a row
    /// spanning three lines is still one row on each side.
    #[test]
    fn a_cell_holding_line_breaks_stays_one_row() {
        let left = side("name,note\na,\"first\nsecond\"");
        let right = side("name,note\na,\"first\nchanged\"");
        let diff = TableDiff::compute(&left, &right);

        assert_eq!(left.rows.len(), 1);
        assert_eq!(alignment(&diff), ["~0|0"]);
        assert_eq!(
            diff.rows.first().map(|row| row.changed_columns.clone()),
            Some(vec![1])
        );
    }

    #[test]
    fn comparing_against_an_empty_file_reports_every_row_as_added() {
        let diff = TableDiff::compute(&side(""), &side("name,value\na,1\nb,2"));

        assert_eq!(alignment(&diff), ["+.|0", "+.|1"]);
        assert_eq!(
            diff.header.as_ref().map(|header| header.kind),
            Some(DiffRowKind::Added)
        );
        assert_eq!(diff.added_rows, 3);
        assert_eq!(diff.change_blocks(), [0]);
    }

    #[test]
    fn two_empty_files_have_nothing_to_compare() {
        let diff = TableDiff::compute(&side(""), &side(""));

        assert!(diff.rows.is_empty());
        assert_eq!(diff.header, None);
        assert_eq!(diff.change_count(), 0);
        assert_eq!(diff.column_count, 0);
    }

    #[test]
    fn a_side_longer_than_the_limit_is_cut_short() {
        let mut text = String::from("value\n");
        for row in 0..MAX_DIFF_ROWS + 10 {
            text.push_str(&format!("{row}\n"));
        }
        let long = side(&text);

        assert_eq!(long.rows.len(), MAX_DIFF_ROWS);
        assert_eq!(long.dropped_rows, 10);
        assert!(TableDiff::compute(&long, &side("value\n0")).truncated);
        assert!(!TableDiff::compute(&side("value\n0"), &side("value\n0")).truncated);
    }

    #[test]
    fn consecutive_changes_are_one_block() {
        let diff = TableDiff::compute(&side("h\na\nb\nc\nd\ne"), &side("h\na\nB\nC\nd\nE"));

        assert_eq!(alignment(&diff), ["=0|0", "~1|1", "~2|2", "=3|3", "~4|4"]);
        assert_eq!(diff.change_blocks(), [1, 4]);
        assert_eq!(diff.changed_row_indices(), [1, 2, 4]);
    }

    #[test]
    fn the_header_is_compared_against_the_other_header() {
        let diff = TableDiff::compute(&side("a,b\n1,2"), &side("a,z\n1,2"));

        assert_eq!(alignment(&diff), ["=0|0"]);
        let header = diff.header.as_ref().expect("both sides have a header");
        assert_eq!(header.kind, DiffRowKind::Modified);
        assert_eq!(header.changed_columns, [1]);
        assert_eq!(diff.change_count(), 1);
    }

    /// With the header setting off the first line is data, and the comparison
    /// has to follow the grid rather than keep its own idea of the file.
    #[test]
    fn without_a_header_the_first_line_takes_part_in_the_alignment() {
        let diff = TableDiff::compute(
            &side_without_header("a,b\n1,2"),
            &side_without_header("1,2"),
        );

        assert_eq!(diff.header, None);
        assert_eq!(alignment(&diff), ["-0|.", "=1|0"]);
    }

    #[test]
    fn row_numbers_count_the_header_as_the_first_row() {
        assert_eq!(side("a,b\n1,2").row_number(0), 2);
        assert_eq!(side_without_header("a,b\n1,2").row_number(0), 1);
    }

    #[test]
    fn a_character_diff_marks_only_the_part_that_changed() {
        let diff = cell_char_diff("abcdef", "abcXef");

        assert_eq!(diff.left, [3..4]);
        assert_eq!(diff.right, [3..4]);
    }

    #[test]
    fn a_character_diff_of_equal_values_is_empty() {
        assert_eq!(cell_char_diff("same", "same"), CellCharDiff::default());
    }

    #[test]
    fn a_character_diff_reports_an_insertion_on_one_side_only() {
        let diff = cell_char_diff("ab", "axb");

        assert!(diff.left.is_empty());
        assert_eq!(diff.right, [1..2]);
    }

    /// The ranges are handed to the text system, which panics on a byte offset
    /// inside a character, so they have to fall on character boundaries.
    #[test]
    fn a_character_diff_of_multibyte_text_stays_on_character_boundaries() {
        let left = "東京都";
        let right = "東京府";
        let diff = cell_char_diff(left, right);

        assert_eq!(diff.left, [6..9]);
        assert_eq!(diff.right, [6..9]);
        for range in diff.left.iter().chain(diff.right.iter()) {
            assert!(left.is_char_boundary(range.start) && left.is_char_boundary(range.end));
        }
        assert_eq!(left.get(6..9), Some("都"));
        assert_eq!(right.get(6..9), Some("府"));
    }

    /// Past the limit the whole value is the difference, rather than the part of
    /// it that actually changed: the two values here share all but their last
    /// character, so a comparison that still ran would report only that one.
    #[test]
    fn a_cell_too_long_to_compare_is_reported_whole() {
        let long = "a".repeat(MAX_CELL_DIFF_BYTES);
        let longer = format!("{long}b");
        let diff = cell_char_diff(&long, &longer);

        assert_eq!(diff.left, [0..long.len()]);
        assert_eq!(diff.right, [0..longer.len()]);
    }

    /// Right up to the limit the comparison still runs, and picks out the one
    /// character that changed rather than giving up on the whole value.
    #[test]
    fn a_cell_right_at_the_limit_is_still_compared_character_by_character() {
        let left = format!("{}x", "a".repeat(MAX_CELL_DIFF_BYTES - 1));
        let right = format!("{}y", "a".repeat(MAX_CELL_DIFF_BYTES - 1));
        let last = MAX_CELL_DIFF_BYTES - 1;
        let diff = cell_char_diff(&left, &right);

        assert_eq!(left.len(), MAX_CELL_DIFF_BYTES);
        assert_eq!(diff.left, [last..MAX_CELL_DIFF_BYTES]);
        assert_eq!(diff.right, [last..MAX_CELL_DIFF_BYTES]);
    }

    #[test]
    fn a_character_diff_against_an_empty_value_has_nothing_on_that_side() {
        let diff = cell_char_diff("", "new");

        assert!(diff.left.is_empty());
        assert_eq!(diff.right, [0..3]);
    }
}
