//! This module defines core operations and config of the tabular data view
//! It operates in 2 coordinate systems:
//! - `DataCellId` - indices of src data cells
//! - `DisplayCellId` - indices of data after applied transformations like sorting/filtering, which is used to render cell on the screen
//!
//! It's designed to contain core logic of operations without relying on `TabularDataPreviewPane`, context or window handles.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, atomic::AtomicUsize},
};

use serde::{Deserialize, Serialize};
use text::TransactionId;
use ui::{SharedString, table_row::TableRow};

use crate::{
    grid_styles::{after_deletion, before_deletion, sorted_unique},
    parser::MAX_SYNC_PARSE_BYTES,
    table_data_engine::{
        filtering_by_column::{
            ColumnFilter, FilterEntry, FilterJoin, FilterStack, RemovedColumnFilters,
            TextCondition, TextFilter, TextFilterOp, calculate_available_filters, retain_rows,
        },
        sorting_by_column::{AppliedSorting, SortDirection, sort_data_rows},
    },
    types::{AnyColumn, DataRow, DisplayRow, TableCell, TableLikeContent},
};

pub mod filtering_by_column;
pub mod sorting_by_column;

/// How many column inserts and deletes can have their filters taken back. Past
/// this the oldest is forgotten and undoing it leaves the filters where they
/// are, exactly as the cell colors do.
const MAX_RECORDED_COLUMN_SHIFTS: usize = 64;

#[derive(Default)]
pub(crate) struct TableDataEngine {
    pub filter_stack: FilterStack,
    /// Pre-computed unique values per column, used to populate filter menus
    all_filters: HashMap<AnyColumn, Vec<FilterEntry>>,
    pub applied_sorting: Option<AppliedSorting>,
    d2d_mapping: DisplayToDataMapping,
    d2d_generation: u64,
    pub contents: Arc<TableLikeContent>,
    /// Where this file's filters and sorting are kept between sessions.
    pub(crate) stored_view: StoredViewState,
    column_shift_undo_stack: Vec<(TransactionId, ColumnShift)>,
    column_shift_redo_stack: Vec<(TransactionId, ColumnShift)>,
}

impl TableDataEngine {
    pub(crate) fn d2d_mapping(&self) -> &DisplayToDataMapping {
        &self.d2d_mapping
    }

    /// Bumped whenever the mapping is replaced, which is what anything holding
    /// a view derived from it compares against to tell that view is stale.
    pub(crate) fn d2d_generation(&self) -> u64 {
        self.d2d_generation
    }

    pub(crate) fn set_d2d_mapping(&mut self, mapping: DisplayToDataMapping) {
        self.d2d_mapping = mapping;
        self.d2d_generation = self.d2d_generation.wrapping_add(1);
    }

    /// Recomputes the unique filter entries for every column from the current table data.
    /// Must be called after content changes (e.g. after parsing).
    pub fn calculate_available_filters(&mut self) {
        // A stored view read back before the first parse has to wait for it:
        // the table has no columns until then, and `forget_columns_beyond`
        // would drop every filter the stored view holds.
        if self.contents.number_of_cols > 0
            && let Some(pending) = self.stored_view.pending.take()
        {
            self.apply_stored_view(pending);
        }
        self.filter_stack
            .forget_columns_beyond(self.contents.number_of_cols);
        self.all_filters =
            calculate_available_filters(&self.contents.rows, self.contents.number_of_cols);
    }

    /// How many of the table's rows the active filters are hiding, or `None`
    /// when the table is too big to count them here.
    ///
    /// The count is a full pass over every row - the very work that
    /// [`MAX_SYNC_PARSE_BYTES`] exists to keep off the foreground thread, which
    /// hands the filtering of a table this size to the background. Counting it
    /// anyway would stall the grid for a number nobody asked for.
    pub(crate) fn hidden_row_count(&self) -> Option<usize> {
        if !self.has_any_filters() {
            return Some(0);
        }
        if self.contents.source_byte_len > MAX_SYNC_PARSE_BYTES {
            return None;
        }
        let kept = retain_rows(&self.contents.rows, &self.filter_stack, None).len();
        Some(self.contents.rows.len().saturating_sub(kept))
    }

    ///// The stored view /////

    /// Puts a stored filter and sort set back, or keeps it until the first
    /// parse has given the table its columns.
    pub(crate) fn restore_stored_view(&mut self, stored: StoredTableView) {
        if self.contents.number_of_cols == 0 {
            self.stored_view.pending = Some(stored);
            return;
        }
        self.apply_stored_view(stored);
    }

    fn apply_stored_view(&mut self, stored: StoredTableView) {
        // Anything the user narrowed or sorted while the read was in flight is
        // the newer of the two and wins, so a stored view is only ever put back
        // onto a table nobody has touched yet.
        if !self.filter_stack.is_empty() || self.applied_sorting.is_some() {
            return;
        }
        let (mut filter_stack, sorting) = stored.into_view();
        // The file may have lost columns since the view was stored. A filter on
        // one that is gone keeps no row at all - a missing cell is not one of
        // its values - and there is no heading left to clear it from; a sorting
        // on one has no pass of its own to prune it and would leave the grid
        // claiming to be sorted by a column nobody can see.
        filter_stack.forget_columns_beyond(self.contents.number_of_cols);
        let sorting =
            sorting.filter(|sorting| sorting.col_idx.get() < self.contents.number_of_cols);
        if filter_stack.is_empty() && sorting.is_none() {
            return;
        }
        self.filter_stack = filter_stack;
        self.applied_sorting = sorting;
        self.stored_view.restored = true;
    }

    /// Reports - once - that a stored view was really put back, so that the
    /// user is told why rows are missing from a table they just opened.
    pub(crate) fn take_restored_notice(&mut self) -> bool {
        std::mem::take(&mut self.stored_view.restored)
    }

    pub(crate) fn current_stored_view(&self) -> StoredTableView {
        StoredTableView::of(&self.filter_stack, self.applied_sorting)
    }

    /// Records what the store now holds, so that the same view is not written
    /// to it twice.
    pub(crate) fn mark_stored_view_saved(&mut self, view: &StoredTableView) {
        let (filter_stack, sorting) = view.clone().into_view();
        self.stored_view.saved = Some((filter_stack, sorting));
    }

    /// The view to write back, or `None` when the store already holds it.
    ///
    /// Compared against the live model rather than against a freshly built
    /// payload: every keystroke reparses and asks this question, and building
    /// the payload copies out every value of every checklist.
    pub(crate) fn take_unsaved_view(&mut self) -> Option<StoredTableView> {
        // A stored view still waiting for the first parse is not in the model
        // yet, and writing the empty model back would throw it away.
        if self.stored_view.pending.is_some() {
            return None;
        }
        let (saved_filters, saved_sorting) = self.stored_view.saved.as_ref()?;
        let unchanged = *saved_filters == self.filter_stack
            && match (*saved_sorting, self.applied_sorting) {
                (None, None) => true,
                (Some(saved), Some(current)) => {
                    saved.col_idx == current.col_idx && saved.direction == current.direction
                }
                _ => false,
            };
        if unchanged && !std::mem::take(&mut self.stored_view.write_even_if_unchanged) {
            return None;
        }

        let view = self.current_stored_view();
        self.mark_stored_view_saved(&view);
        Some(view)
    }

    /// Nothing is known about the row the view is about to be written to, so
    /// the next write goes out whether or not the view itself has changed.
    pub(crate) fn forget_where_the_view_was_saved(&mut self) {
        self.stored_view.write_even_if_unchanged = true;
    }

    ///// Column inserts and deletes /////

    /// Moves the filters and the sorting along with a column operation.
    ///
    /// Applied *before* the buffer edit, because the reparse that edit triggers
    /// runs `forget_columns_beyond` against the new width: a filter still
    /// sitting on its old index would be dropped there instead of moved.
    pub(crate) fn apply_column_shift(&mut self, shift: &mut ColumnShift) {
        match shift {
            ColumnShift::Insert { at, count } => {
                self.filter_stack.insert_columns(*at, *count);
                if let Some(sorting) = self.applied_sorting.as_mut()
                    && sorting.col_idx.get() >= *at
                {
                    sorting.col_idx = AnyColumn::new(sorting.col_idx.get() + *count);
                }
            }
            ColumnShift::Delete { cols, removed } => {
                *removed = self.delete_columns_from_view(cols);
            }
        }
    }

    fn delete_columns_from_view(&mut self, cols: &[usize]) -> RemovedColumns {
        let deleted = sorted_unique(cols);
        let filters = self.filter_stack.delete_columns(&deleted);
        let sorting = self.applied_sorting;
        self.applied_sorting = self.applied_sorting.and_then(|sorting| {
            after_deletion(sorting.col_idx.get(), &deleted).map(|shifted| AppliedSorting {
                col_idx: AnyColumn::new(shifted),
                direction: sorting.direction,
            })
        });
        RemovedColumns {
            filters,
            // Only worth putting back when the sorted column itself is gone;
            // otherwise the shifted one above is already the right answer.
            sorting: sorting.filter(|sorting| deleted.contains(&sorting.col_idx.get())),
        }
    }

    /// The inverse, applied to the filters as they are now so that a column
    /// narrowed after the operation keeps what the user chose.
    pub(crate) fn revert_column_shift(&mut self, shift: &ColumnShift) {
        match shift {
            ColumnShift::Insert { at, count } => {
                self.delete_columns_from_view(&(*at..*at + *count).collect::<Vec<_>>());
            }
            ColumnShift::Delete { cols, removed } => {
                let deleted = sorted_unique(cols);
                self.filter_stack
                    .restore_columns(&deleted, &removed.filters);
                // A sort applied after the delete is the newer of the two and
                // keeps the column it was applied to; only a table left
                // unsorted gets the deleted column's own sorting back.
                self.applied_sorting = match self.applied_sorting {
                    Some(sorting) => Some(AppliedSorting {
                        col_idx: AnyColumn::new(before_deletion(sorting.col_idx.get(), &deleted)),
                        direction: sorting.direction,
                    }),
                    None => removed.sorting,
                };
            }
        }
    }

    /// Records an already-applied shift against the buffer transaction it
    /// belongs to, so that undoing that transaction takes the filters back.
    ///
    /// No transaction means nothing reached the buffer - the edit engine
    /// refused the operation, or there was no buffer to edit - so the columns
    /// never moved and the shift is taken straight back off. Leaving it on
    /// would point every filter past it at its neighbour's values, silently and
    /// with no undo step to take it back.
    pub(crate) fn record_column_shift(
        &mut self,
        transaction_id: Option<TransactionId>,
        shift: ColumnShift,
    ) {
        let Some(transaction_id) = transaction_id else {
            self.revert_column_shift(&shift);
            return;
        };
        self.column_shift_redo_stack.clear();
        self.column_shift_undo_stack.push((transaction_id, shift));
        if self.column_shift_undo_stack.len() > MAX_RECORDED_COLUMN_SHIFTS {
            self.column_shift_undo_stack.remove(0);
        }
    }

    /// Takes the filters back when the column operation `transaction_id`
    /// belongs to is undone. Anything else - an edit made on the text side, or
    /// an operation whose record has been forgotten - leaves them alone.
    pub(crate) fn undo_column_shift(&mut self, transaction_id: TransactionId) {
        if self.column_shift_undo_stack.last().map(|(id, _)| *id) != Some(transaction_id) {
            return;
        }
        let Some((id, shift)) = self.column_shift_undo_stack.pop() else {
            return;
        };
        self.revert_column_shift(&shift);
        self.column_shift_redo_stack.push((id, shift));
    }

    pub(crate) fn redo_column_shift(&mut self, transaction_id: TransactionId) {
        if self.column_shift_redo_stack.last().map(|(id, _)| *id) != Some(transaction_id) {
            return;
        }
        let Some((id, mut shift)) = self.column_shift_redo_stack.pop() else {
            return;
        };
        self.apply_column_shift(&mut shift);
        self.column_shift_undo_stack.push((id, shift));
    }
}

/// A column insert or delete, remembered so that undoing the buffer edit takes
/// the filters and the sorting back with it.
#[derive(Debug, Clone)]
pub(crate) enum ColumnShift {
    Insert {
        at: usize,
        count: usize,
    },
    Delete {
        cols: Vec<usize>,
        removed: RemovedColumns,
    },
}

impl ColumnShift {
    pub(crate) fn delete(cols: Vec<usize>) -> Self {
        ColumnShift::Delete {
            cols,
            removed: RemovedColumns::default(),
        }
    }
}

/// What a column delete took out of the view.
#[derive(Debug, Clone, Default)]
pub(crate) struct RemovedColumns {
    filters: RemovedColumnFilters,
    sorting: Option<AppliedSorting>,
}

/// Where a file's filters and sorting live between sessions, and what has
/// already been written there.
#[derive(Default)]
pub(crate) struct StoredViewState {
    /// Set once the first read has been kicked off, so that it happens exactly
    /// once per grid.
    pub(crate) load_started: bool,
    /// The key value store row this file's view lives in, or `None` for a
    /// buffer with no local file, whose view lasts only as long as the tab.
    pub(crate) key: Option<String>,
    /// What the store holds, or `None` while the read is still in flight: a
    /// write before then would replace the stored view with an empty one.
    saved: Option<(FilterStack, Option<AppliedSorting>)>,
    /// Bumped on every change. A write only goes out while it still holds the
    /// newest one, which is what debounces the writes.
    pub(crate) save_generation: Arc<AtomicUsize>,
    /// A stored view waiting for the first parse.
    pending: Option<StoredTableView>,
    /// Set when a stored view was really put back.
    restored: bool,
    /// Set when the row being written to is not the one `saved` describes.
    write_even_if_unchanged: bool,
}

/// One file's filters and sorting, in the shape they are stored in.
///
/// Kept apart from the live model so that the payload is stable: a `HashMap`
/// and a `HashSet` have no order to write out, and the payload is compared
/// against the last one written to decide whether a write is needed at all.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct StoredTableView {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    filters: Vec<StoredColumnFilter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sorting: Option<StoredSorting>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredColumnFilter {
    col: usize,
    #[serde(flatten)]
    filter: StoredFilter,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredFilter {
    Values {
        /// `null` stands for a row whose cell in this column is missing
        /// altogether, which keeps different rows than the empty string does.
        values: Vec<Option<String>>,
    },
    Text {
        first: StoredTextFilter,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        second: Option<StoredSecondFilter>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredTextFilter {
    op: TextFilterOp,
    #[serde(default)]
    needle: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredSecondFilter {
    join: FilterJoin,
    #[serde(flatten)]
    filter: StoredTextFilter,
}

/// Mirrors [`SortDirection`], which is not itself serializable: the stored
/// spelling is part of the payload's format and should not change with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredDirection {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct StoredSorting {
    col: usize,
    direction: StoredDirection,
}

impl StoredTableView {
    fn of(filter_stack: &FilterStack, sorting: Option<AppliedSorting>) -> Self {
        let mut filters: Vec<StoredColumnFilter> = filter_stack
            .entries()
            .map(|(col, filter)| StoredColumnFilter {
                col: col.get(),
                filter: StoredFilter::of(filter),
            })
            .collect();
        filters.sort_by_key(|filter| filter.col);

        Self {
            filters,
            sorting: sorting.map(|sorting| StoredSorting {
                col: sorting.col_idx.get(),
                direction: match sorting.direction {
                    SortDirection::Asc => StoredDirection::Asc,
                    SortDirection::Desc => StoredDirection::Desc,
                },
            }),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.filters.is_empty() && self.sorting.is_none()
    }

    fn into_view(self) -> (FilterStack, Option<AppliedSorting>) {
        let mut filter_stack = FilterStack::default();
        for stored in self.filters {
            if let Some(filter) = stored.filter.into_filter() {
                filter_stack.set_column_filter(AnyColumn::new(stored.col), filter);
            }
        }
        let sorting = self.sorting.map(|sorting| AppliedSorting {
            col_idx: AnyColumn::new(sorting.col),
            direction: match sorting.direction {
                StoredDirection::Asc => SortDirection::Asc,
                StoredDirection::Desc => SortDirection::Desc,
            },
        });
        (filter_stack, sorting)
    }
}

impl StoredFilter {
    fn of(filter: &ColumnFilter) -> Self {
        match filter {
            ColumnFilter::Values(values) => {
                let mut values: Vec<Option<String>> = values
                    .iter()
                    .map(|value| value.as_ref().map(|value| value.as_ref().to_owned()))
                    .collect();
                values.sort();
                StoredFilter::Values { values }
            }
            ColumnFilter::Text(condition) => StoredFilter::Text {
                first: StoredTextFilter::of(&condition.first),
                second: condition
                    .rest
                    .as_ref()
                    .map(|(join, second)| StoredSecondFilter {
                        join: *join,
                        filter: StoredTextFilter::of(second),
                    }),
            },
        }
    }

    /// `None` for a payload the model itself could never hold, which a row
    /// edited by hand or written by a later version can carry: an empty
    /// checklist keeps no row at all, and a condition that reads the typed text
    /// but was stored without any keeps every row while the blue row numbers
    /// claim the grid is hiding some.
    fn into_filter(self) -> Option<ColumnFilter> {
        match self {
            StoredFilter::Values { values } => {
                if values.is_empty() {
                    return None;
                }
                Some(ColumnFilter::Values(
                    values
                        .into_iter()
                        .map(|value| value.map(SharedString::from))
                        .collect(),
                ))
            }
            StoredFilter::Text { first, second } => Some(ColumnFilter::Text(TextCondition {
                first: first.into_filter()?,
                rest: second.and_then(|second| Some((second.join, second.filter.into_filter()?))),
            })),
        }
    }
}

impl StoredTextFilter {
    fn of(filter: &TextFilter) -> Self {
        Self {
            op: filter.op,
            needle: filter.needle.as_ref().to_owned(),
        }
    }

    fn into_filter(self) -> Option<TextFilter> {
        if self.op.needs_needle() && self.needle.is_empty() {
            return None;
        }
        Some(TextFilter::new(self.op, SharedString::from(self.needle)))
    }
}

/// Relation of Display (rendered) rows to Data (src) rows with applied transformations
/// Transformations applied:
/// - sorting by column
/// - filtering by column values
#[derive(Debug, Default)]
pub struct DisplayToDataMapping {
    /// All rows sorted, regardless of applied filtering. Recomputed every time sorting changes
    sorted_rows: Vec<DataRow>,
    /// Rows that survive the active filters. Recomputed every time filters change
    retained_rows: HashSet<DataRow>,
    /// Merged result: sorted rows intersected with retained rows. Empty while `identity_len` is
    /// set, because building it would cost one hash map entry per row on every edit.
    mapping: Arc<HashMap<DisplayRow, DataRow>>,
    /// Row count when every display row is its own data row, which is the case whenever neither
    /// sorting nor filtering is active.
    identity_len: Option<usize>,
}

impl DisplayToDataMapping {
    /// Computes the full display-to-data mapping from owned inputs.
    /// Intended to be called from a background thread.
    pub(crate) fn compute(
        contents: &Arc<TableLikeContent>,
        filter_stack: &FilterStack,
        sorting: Option<AppliedSorting>,
    ) -> Self {
        // Untransformed tables are the common case and are re-mapped after every single cell edit,
        // so they skip the per-row bookkeeping entirely.
        if sorting.is_none() && filter_stack.is_empty() {
            return Self {
                identity_len: Some(contents.rows.len()),
                ..Self::default()
            };
        }

        let mut mapping = Self::default();
        mapping.apply_sorting(sorting, &contents.rows);
        mapping.apply_filtering(filter_stack, &contents.rows);
        mapping.merge_mappings();
        mapping
    }

    /// Get the data row for a given display row
    pub fn get_data_row(&self, display_row: DisplayRow) -> Option<DataRow> {
        if let Some(identity_len) = self.identity_len {
            return (*display_row < identity_len).then_some(DataRow(*display_row));
        }
        self.mapping.get(&display_row).copied()
    }

    /// Whether every display row is simply its own data row, which it is while
    /// neither sorting nor filtering is active.
    pub(crate) fn is_identity(&self) -> bool {
        self.identity_len.is_some()
    }

    /// Get the number of filtered rows
    pub fn visible_row_count(&self) -> usize {
        self.identity_len.unwrap_or_else(|| self.mapping.len())
    }

    /// Computes sorting
    fn apply_sorting(&mut self, sorting: Option<AppliedSorting>, rows: &[TableRow<TableCell>]) {
        let data_rows: Vec<DataRow> = (0..rows.len()).map(DataRow).collect();

        let sorted_rows = if let Some(sorting) = sorting {
            sort_data_rows(&rows, data_rows, sorting)
        } else {
            data_rows
        };

        self.sorted_rows = sorted_rows;
    }

    fn apply_filtering(&mut self, filter_stack: &FilterStack, rows: &[TableRow<TableCell>]) {
        self.retained_rows = retain_rows(rows, filter_stack, None);
    }

    /// Merges pre-computed sorting and filtering into the final display mapping
    fn merge_mappings(&mut self) {
        self.mapping = Arc::new(
            self.sorted_rows
                .iter()
                .filter(|data_row| self.retained_rows.contains(data_row))
                .enumerate()
                .map(|(display, data)| (DisplayRow(display), *data))
                .collect(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DisplayRow;

    fn engine_for(table_text: &str) -> TableDataEngine {
        let mut engine = TableDataEngine::default();
        engine.contents = Arc::new(TableLikeContent::from_str(table_text.to_string(), true));
        engine.calculate_available_filters();
        engine
    }

    fn round_trip(view: &StoredTableView) -> StoredTableView {
        let payload = serde_json::to_string(view).expect("the view should serialize");
        serde_json::from_str(&payload).expect("the payload should parse")
    }

    /// A cell the row does not have at all keeps different rows than one holding
    /// the empty string, so the two have to survive the round trip apart.
    #[test]
    fn a_missing_cell_and_an_empty_one_stay_apart_in_the_stored_payload() {
        let mut engine = engine_for("c0,c1\na,1\nb,2");
        engine.set_value_filter(
            AnyColumn(0),
            HashSet::from_iter([None, Some(SharedString::from("")), Some("a".into())]),
        );

        let stored = engine.current_stored_view();
        assert_eq!(round_trip(&stored), stored);

        let (filter_stack, _) = round_trip(&stored).into_view();
        let ColumnFilter::Values(values) = filter_stack
            .entries()
            .find(|(col, _)| *col == AnyColumn(0))
            .map(|(_, filter)| filter.clone())
            .expect("column 0 should be filtered")
        else {
            panic!("the stored filter should still be a value checklist");
        };
        assert!(values.contains(&None), "the missing cell should come back");
        assert!(
            values.contains(&Some(SharedString::from(""))),
            "the empty string should come back, and not as the missing cell"
        );
        assert_eq!(values.len(), 3);
    }

    #[test]
    fn two_conditions_and_the_sorting_survive_the_round_trip() {
        let mut engine = engine_for("c0,c1\na,1\nb,2");
        engine.set_text_filter(AnyColumn(1), TextFilterOp::Contains, "x".into());
        engine.set_second_text_filter(
            AnyColumn(1),
            FilterJoin::Or,
            TextFilterOp::NotBlank,
            "".into(),
        );
        engine.applied_sorting = Some(AppliedSorting {
            col_idx: AnyColumn(1),
            direction: SortDirection::Desc,
        });

        let stored = engine.current_stored_view();
        let (filter_stack, sorting) = round_trip(&stored).into_view();

        let mut restored = engine_for("c0,c1\na,1\nb,2");
        restored.filter_stack = filter_stack;
        restored.applied_sorting = sorting;
        let condition = restored
            .text_filter(AnyColumn(1))
            .expect("the text condition should come back");
        assert_eq!(condition.description(), "contains \"x\" or is not blank");
        assert_eq!(
            restored
                .applied_sorting
                .map(|sorting| (sorting.col_idx, sorting.direction == SortDirection::Desc)),
            Some((AnyColumn(1), true))
        );
    }

    /// Counting the hidden rows walks every row, and a table past
    /// `MAX_SYNC_PARSE_BYTES` is exactly the one whose filtering the pane hands
    /// to the background instead of running it in step. The count has to stay
    /// off the foreground thread with it, so the notice goes out without one.
    #[test]
    fn a_table_too_big_to_filter_in_step_is_not_counted_either() {
        let mut table = String::from(
            "c0,c1
",
        );
        let mut row = 0;
        while table.len() <= MAX_SYNC_PARSE_BYTES {
            table.push_str(&format!(
                "{row:0>64},drop
"
            ));
            row += 1;
        }
        let mut engine = engine_for(&table);
        engine.toggle_filter(AnyColumn(1), Some("drop".into()));

        assert!(engine.has_any_filters());
        assert_eq!(
            engine.hidden_row_count(),
            None,
            "a table this size is filtered in the background for the same reason"
        );

        // The same filter on a table under the threshold is still counted, so
        // it is the size holding the number back and not the filter itself.
        let mut small = engine_for(
            "c0,c1
keep,drop
keep,keep",
        );
        small.toggle_filter(AnyColumn(1), Some("drop".into()));
        assert_eq!(small.hidden_row_count(), Some(1));
    }

    /// The column count is zero until the first parse, and `forget_columns_beyond`
    /// would drop every filter a stored view holds.
    #[test]
    fn a_stored_view_read_before_the_first_parse_waits_for_it() {
        let mut source = engine_for("c0,c1\na,1\nb,2");
        source.toggle_filter(AnyColumn(1), Some("1".into()));
        let stored = source.current_stored_view();

        let mut engine = TableDataEngine::default();
        engine.restore_stored_view(stored);
        assert!(
            !engine.has_any_filters(),
            "nothing may be restored onto a table with no columns yet"
        );
        engine.contents = Arc::new(TableLikeContent::from_str(
            "c0,c1\na,1\nb,2".to_owned(),
            true,
        ));
        engine.calculate_available_filters();

        assert!(engine.has_active_filters(AnyColumn(1)));
        assert!(engine.take_restored_notice());
        assert_eq!(engine.hidden_row_count(), Some(1));
    }

    /// A narrowing the user made while the read was still in flight is the
    /// newer of the two.
    #[test]
    fn a_stored_view_never_overwrites_what_the_user_already_narrowed() {
        let mut source = engine_for("c0,c1\na,1\nb,2");
        source.toggle_filter(AnyColumn(1), Some("1".into()));
        let stored = source.current_stored_view();

        let mut engine = engine_for("c0,c1\na,1\nb,2");
        engine.toggle_filter(AnyColumn(0), Some("b".into()));
        engine.restore_stored_view(stored);

        assert!(engine.has_active_filters(AnyColumn(0)));
        assert!(!engine.has_active_filters(AnyColumn(1)));
        assert!(!engine.take_restored_notice());
    }

    /// The file the view was stored for may have lost columns since. A filter
    /// on one that is gone keeps no row at all - a missing cell is not one of
    /// its values - and a sorting on one leaves the grid claiming to be sorted
    /// by a column nobody can see.
    #[test]
    fn a_stored_view_drops_the_columns_the_file_no_longer_has() {
        let mut source = engine_for("c0,c1,c2\na,1,x\nb,2,y");
        source.toggle_filter(AnyColumn(2), Some("x".into()));
        source.applied_sorting = Some(AppliedSorting {
            col_idx: AnyColumn(2),
            direction: SortDirection::Asc,
        });
        let stored = source.current_stored_view();

        let mut engine = engine_for("c0,c1\na,1\nb,2");
        engine.restore_stored_view(stored.clone());
        engine.calculate_available_filters();
        assert!(
            !engine.has_any_filters(),
            "a filter on a column that is gone must not survive the restore"
        );
        assert!(engine.applied_sorting.is_none(), "nor a sorting on it");
        assert!(
            !engine.take_restored_notice(),
            "and nothing was restored to tell the user about"
        );

        // The same by the other road in: a view read back before the first
        // parse, which waits for the table to have columns at all.
        let mut waited = TableDataEngine::default();
        waited.restore_stored_view(stored);
        waited.contents = Arc::new(TableLikeContent::from_str(
            "c0,c1\na,1\nb,2".to_owned(),
            true,
        ));
        waited.calculate_available_filters();
        assert!(!waited.has_any_filters());
        assert!(waited.applied_sorting.is_none());
        assert!(!waited.take_restored_notice());
    }

    /// A row edited by hand - or written by a later version - can name a filter
    /// the popover could never build. An empty checklist would hide every row
    /// and a needle-reading condition with no needle would hide none, both with
    /// the blue row numbers on and nothing to explain them.
    #[test]
    fn a_stored_filter_the_model_could_never_hold_is_ignored() {
        let payload = r#"{
            "filters": [
                { "col": 0, "kind": "values", "values": [] },
                { "col": 1, "kind": "text", "first": { "op": "contains", "needle": "" } },
                { "col": 2, "kind": "text",
                  "first": { "op": "contains", "needle": "a" },
                  "second": { "join": "and", "op": "ends_with", "needle": "" } }
            ]
        }"#;
        let view: StoredTableView =
            serde_json::from_str(payload).expect("the payload should parse");
        let (filter_stack, _) = view.into_view();

        assert!(
            filter_stack
                .entries()
                .all(|(col, _)| col != AnyColumn(0) && col != AnyColumn(1)),
            "neither an empty checklist nor a needleless condition may come back"
        );
        let Some((_, ColumnFilter::Text(condition))) = filter_stack
            .entries()
            .find(|(col, _)| *col == AnyColumn(2))
            .map(|(col, filter)| (col, filter.clone()))
        else {
            panic!("the condition that does have a needle should come back");
        };
        assert!(
            condition.rest.is_none(),
            "and its needleless second condition should be dropped on its own"
        );
    }

    /// Nothing the popover cannot write is worth a panic either: a payload that
    /// names an operator this version does not know is refused whole rather
    /// than half applied.
    #[test]
    fn an_unparsable_stored_payload_is_refused_rather_than_guessed_at() {
        for payload in [
            "{oops",
            r#"{"filters":[{"col":0,"kind":"text","first":{"op":"sounds_like","needle":"a"}}]}"#,
            r#"{"filters":[{"kind":"values","values":["a"]}]}"#,
            r#"{"filters":[{"col":0,"kind":"regex","pattern":"a"}]}"#,
        ] {
            assert!(
                serde_json::from_str::<StoredTableView>(payload).is_err(),
                "{payload} should not parse into a view"
            );
        }
    }

    ///// Columns moving under the filters and the sorting /////

    fn three_columns_filtered_on_b() -> TableDataEngine {
        let mut engine = engine_for("c0,c1,c2\na,keep,1\nb,drop,2\nc,keep,3");
        engine.toggle_filter(AnyColumn(1), Some("keep".into()));
        engine.applied_sorting = Some(AppliedSorting {
            col_idx: AnyColumn(1),
            direction: SortDirection::Asc,
        });
        engine
    }

    fn sorted_column(engine: &TableDataEngine) -> Option<AnyColumn> {
        engine.applied_sorting.map(|sorting| sorting.col_idx)
    }

    #[test]
    fn inserting_a_column_carries_the_filter_and_the_sorting_right() {
        let mut engine = three_columns_filtered_on_b();
        let mut shift = ColumnShift::Insert { at: 0, count: 1 };
        engine.apply_column_shift(&mut shift);

        assert!(engine.has_active_filters(AnyColumn(2)));
        assert_eq!(sorted_column(&engine), Some(AnyColumn(2)));

        engine.revert_column_shift(&shift);
        assert!(engine.has_active_filters(AnyColumn(1)));
        assert_eq!(sorted_column(&engine), Some(AnyColumn(1)));
    }

    /// The column *at* the insertion point is the one the new columns push
    /// aside, and it moves by as many as were inserted rather than by one.
    #[test]
    fn inserting_several_columns_at_the_filtered_one_moves_it_by_that_many() {
        let mut engine = three_columns_filtered_on_b();
        let mut shift = ColumnShift::Insert { at: 1, count: 2 };
        engine.apply_column_shift(&mut shift);

        assert!(!engine.has_active_filters(AnyColumn(1)));
        assert!(!engine.has_active_filters(AnyColumn(2)));
        assert!(engine.has_active_filters(AnyColumn(3)));
        assert_eq!(sorted_column(&engine), Some(AnyColumn(3)));

        engine.revert_column_shift(&shift);
        assert!(engine.has_active_filters(AnyColumn(1)));
        assert_eq!(sorted_column(&engine), Some(AnyColumn(1)));
    }

    /// An operation whose edits never reached the buffer left the columns where
    /// they were, so the shift applied ahead of it has to come straight back
    /// off: there is no transaction to undo it with later.
    #[test]
    fn a_shift_with_no_transaction_behind_it_is_taken_back_off() {
        let mut engine = three_columns_filtered_on_b();
        let before = engine.current_stored_view();

        let mut insert = ColumnShift::Insert { at: 0, count: 2 };
        engine.apply_column_shift(&mut insert);
        engine.record_column_shift(None, insert);
        assert_eq!(engine.current_stored_view(), before);

        let mut delete = ColumnShift::delete(vec![1]);
        engine.apply_column_shift(&mut delete);
        engine.record_column_shift(None, delete);
        assert_eq!(engine.current_stored_view(), before);
    }

    #[test]
    fn deleting_the_filtered_column_gives_its_filter_back_on_undo() {
        let mut engine = three_columns_filtered_on_b();
        let mut shift = ColumnShift::delete(vec![1]);
        engine.apply_column_shift(&mut shift);

        assert!(!engine.has_any_filters());
        assert_eq!(sorted_column(&engine), None);

        engine.revert_column_shift(&shift);
        assert!(engine.has_active_filters(AnyColumn(1)));
        assert_eq!(sorted_column(&engine), Some(AnyColumn(1)));
    }

    /// Deleting a column to the *left* of the filtered one moves the filter
    /// left with it rather than dropping it.
    #[test]
    fn deleting_a_column_to_the_left_moves_the_filter_left() {
        let mut engine = three_columns_filtered_on_b();
        let mut shift = ColumnShift::delete(vec![0]);
        engine.apply_column_shift(&mut shift);

        assert!(engine.has_active_filters(AnyColumn(0)));
        assert_eq!(sorted_column(&engine), Some(AnyColumn(0)));

        engine.revert_column_shift(&shift);
        assert!(engine.has_active_filters(AnyColumn(1)));
        assert_eq!(sorted_column(&engine), Some(AnyColumn(1)));
    }

    #[test]
    fn an_untransformed_table_maps_every_display_row_to_itself() {
        let engine = engine_for("h\na\nb\nc");
        let mapping = DisplayToDataMapping::compute(&engine.contents, &engine.filter_stack, None);

        assert!(mapping.is_identity());
        assert_eq!(mapping.visible_row_count(), 3);
        assert_eq!(mapping.get_data_row(DisplayRow(0)), Some(DataRow(0)));
        assert_eq!(mapping.get_data_row(DisplayRow(2)), Some(DataRow(2)));
        assert_eq!(mapping.get_data_row(DisplayRow(3)), None);
    }

    #[test]
    fn a_filtered_table_maps_through_the_retained_rows() {
        let mut engine = engine_for("h\na\nb\na");
        engine.toggle_filter(AnyColumn(0), Some("a".into()));
        let mapping = DisplayToDataMapping::compute(&engine.contents, &engine.filter_stack, None);

        assert!(!mapping.is_identity());
        assert_eq!(mapping.visible_row_count(), 2);
        assert_eq!(mapping.get_data_row(DisplayRow(0)), Some(DataRow(0)));
        assert_eq!(mapping.get_data_row(DisplayRow(1)), Some(DataRow(2)));
        assert_eq!(mapping.get_data_row(DisplayRow(2)), None);
    }
}
