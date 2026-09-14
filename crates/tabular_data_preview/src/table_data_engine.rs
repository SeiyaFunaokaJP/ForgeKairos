//! This module defines core operations and config of the tabular data view
//! It operates in 2 coordinate systems:
//! - `DataCellId` - indices of src data cells
//! - `DisplayCellId` - indices of data after applied transformations like sorting/filtering, which is used to render cell on the screen
//!
//! It's designed to contain core logic of operations without relying on `TabularDataPreviewPane`, context or window handles.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use ui::table_row::TableRow;

use crate::{
    table_data_engine::{
        filtering_by_column::{FilterEntry, FilterStack, calculate_available_filters, retain_rows},
        sorting_by_column::{AppliedSorting, sort_data_rows},
    },
    types::{AnyColumn, DataRow, DisplayRow, TableCell, TableLikeContent},
};

pub mod filtering_by_column;
pub mod sorting_by_column;

#[derive(Default)]
pub(crate) struct TableDataEngine {
    pub filter_stack: FilterStack,
    /// Pre-computed unique values per column, used to populate filter menus
    all_filters: HashMap<AnyColumn, Vec<FilterEntry>>,
    pub applied_sorting: Option<AppliedSorting>,
    d2d_mapping: DisplayToDataMapping,
    pub contents: Arc<TableLikeContent>,
}

impl TableDataEngine {
    pub(crate) fn d2d_mapping(&self) -> &DisplayToDataMapping {
        &self.d2d_mapping
    }

    pub(crate) fn set_d2d_mapping(&mut self, mapping: DisplayToDataMapping) {
        self.d2d_mapping = mapping;
    }

    /// Recomputes the unique filter entries for every column from the current table data.
    /// Must be called after content changes (e.g. after parsing).
    pub fn calculate_available_filters(&mut self) {
        self.all_filters =
            calculate_available_filters(&self.contents.rows, self.contents.number_of_cols);
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

    #[cfg(test)]
    fn holds_an_explicit_mapping(&self) -> bool {
        self.identity_len.is_none()
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

    #[test]
    fn an_untransformed_table_maps_every_display_row_to_itself() {
        let engine = engine_for("h\na\nb\nc");
        let mapping = DisplayToDataMapping::compute(&engine.contents, &engine.filter_stack, None);

        assert!(!mapping.holds_an_explicit_mapping());
        assert_eq!(mapping.visible_row_count(), 3);
        assert_eq!(mapping.get_data_row(DisplayRow(0)), Some(DataRow(0)));
        assert_eq!(mapping.get_data_row(DisplayRow(2)), Some(DataRow(2)));
        assert_eq!(mapping.get_data_row(DisplayRow(3)), None);
    }

    #[test]
    fn a_filtered_table_maps_through_the_retained_rows() {
        let mut engine = engine_for("h\na\nb\na");
        engine
            .toggle_filter(AnyColumn(0), Some("a".into()))
            .expect("the column has an `a` entry to filter on");
        let mapping = DisplayToDataMapping::compute(&engine.contents, &engine.filter_stack, None);

        assert!(mapping.holds_an_explicit_mapping());
        assert_eq!(mapping.visible_row_count(), 2);
        assert_eq!(mapping.get_data_row(DisplayRow(0)), Some(DataRow(0)));
        assert_eq!(mapping.get_data_row(DisplayRow(1)), Some(DataRow(2)));
        assert_eq!(mapping.get_data_row(DisplayRow(2)), None);
    }
}
