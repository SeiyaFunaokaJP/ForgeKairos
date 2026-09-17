use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use ui::{SharedString, table_row::TableRow};

use crate::{
    table_data_engine::TableDataEngine,
    types::{AnyColumn, DataRow, TableCell},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FilterEntryState {
    Available {
        is_applied: bool,
    },
    /// Selecting this value would leave zero rows given every other active column's
    /// filter. Still carries `is_applied`: a value can be both currently checked
    /// *and* blocked (another column's filter was applied afterward), and the UI
    /// needs to keep showing it checked so the user can uncheck it.
    Unavailable {
        blocked_by: AnyColumn,
        is_applied: bool,
    },
}

#[derive(Debug, Clone)]
pub struct FilterEntry {
    /// Content to display. None if cell is virtual
    pub content: Option<SharedString>,
    /// List of rows in which this value occurs
    pub rows: Vec<DataRow>,
}

impl FilterEntry {
    pub(crate) fn occurred_times(&self) -> usize {
        self.rows.len()
    }
}

/// One of the text conditions Excel offers beside the value checklist, for when
/// the user wants "everything containing x" rather than a list of values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextFilterOp {
    Contains,
    NotContains,
    Equals,
    StartsWith,
    EndsWith,
    Blank,
    NotBlank,
}

impl TextFilterOp {
    /// Every operator, in the order the popover offers them.
    pub(crate) const ALL: [TextFilterOp; 7] = [
        TextFilterOp::Contains,
        TextFilterOp::NotContains,
        TextFilterOp::Equals,
        TextFilterOp::StartsWith,
        TextFilterOp::EndsWith,
        TextFilterOp::Blank,
        TextFilterOp::NotBlank,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            TextFilterOp::Contains => "含む",
            TextFilterOp::NotContains => "含まない",
            TextFilterOp::Equals => "等しい",
            TextFilterOp::StartsWith => "前方一致",
            TextFilterOp::EndsWith => "後方一致",
            TextFilterOp::Blank => "空白",
            TextFilterOp::NotBlank => "空白以外",
        }
    }

    /// Whether the operator reads the text the user typed at all.
    pub(crate) fn needs_needle(self) -> bool {
        !matches!(self, TextFilterOp::Blank | TextFilterOp::NotBlank)
    }

    /// How the condition reads as a phrase, e.g. `「abc」を含む`. Written out per
    /// operator rather than as "label + needle" because the particle differs.
    pub(crate) fn describe(self, needle: &str) -> String {
        match self {
            TextFilterOp::Contains => format!("「{needle}」を含む"),
            TextFilterOp::NotContains => format!("「{needle}」を含まない"),
            TextFilterOp::Equals => format!("「{needle}」と等しい"),
            TextFilterOp::StartsWith => format!("「{needle}」で始まる"),
            TextFilterOp::EndsWith => format!("「{needle}」で終わる"),
            TextFilterOp::Blank => "空白".to_owned(),
            TextFilterOp::NotBlank => "空白以外".to_owned(),
        }
    }
}

/// A text condition on one column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TextFilter {
    pub(crate) op: TextFilterOp,
    /// What the user typed, kept as typed so the popover can show it back.
    pub(crate) needle: SharedString,
    /// `needle` case folded once, so that testing a row does not fold it again
    /// for every cell.
    folded_needle: String,
}

impl TextFilter {
    pub(crate) fn new(op: TextFilterOp, needle: SharedString) -> Self {
        let folded_needle = needle.to_lowercase();
        Self {
            op,
            needle,
            folded_needle,
        }
    }

    /// Whether a cell's value passes this condition. Case is ignored, the way
    /// Excel's text filters ignore it.
    pub(crate) fn matches(&self, value: Option<&SharedString>) -> bool {
        let value = value.map(SharedString::as_ref).unwrap_or_default();
        match self.op {
            // A cell holding nothing but spaces reads as empty in the grid, so
            // it counts as blank here too.
            TextFilterOp::Blank => value.trim().is_empty(),
            TextFilterOp::NotBlank => !value.trim().is_empty(),
            TextFilterOp::Contains => value.to_lowercase().contains(&self.folded_needle),
            TextFilterOp::NotContains => !value.to_lowercase().contains(&self.folded_needle),
            TextFilterOp::Equals => value.to_lowercase() == self.folded_needle,
            TextFilterOp::StartsWith => value.to_lowercase().starts_with(&self.folded_needle),
            TextFilterOp::EndsWith => value.to_lowercase().ends_with(&self.folded_needle),
        }
    }

    /// How the condition reads where the UI reports what a column is filtered by.
    pub(crate) fn description(&self) -> String {
        self.op.describe(&self.needle)
    }
}

/// What one column is filtered by.
///
/// Excel keeps a value checklist and a text condition per column and combines
/// them; here the two replace each other, so that a checkmark in the list means
/// exactly one thing - that value is in the set of values being kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ColumnFilter {
    /// A row is kept when its cell in this column holds one of these values.
    Values(HashSet<Option<SharedString>>),
    Text(TextFilter),
}

impl ColumnFilter {
    fn keeps(&self, value: Option<&SharedString>) -> bool {
        match self {
            ColumnFilter::Values(values) => values.contains(&value.cloned()),
            ColumnFilter::Text(filter) => filter.matches(value),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub(crate) struct FilterStack {
    /// How each filtered column is filtered. A column with no entry here is not
    /// filtered at all.
    column_filters: HashMap<AnyColumn, ColumnFilter>,
}

impl FilterStack {
    pub(crate) fn is_empty(&self) -> bool {
        self.column_filters.is_empty()
    }

    /// Drops the filters of columns a table this wide does not have.
    ///
    /// Filters outlive a reparse, which is what lets one survive an edit to the
    /// cells it is filtering; a column *delete* would otherwise leave one
    /// behind on a column index that is now past the end of every row, and
    /// since a missing cell is not one of the values it keeps, the whole table
    /// would go blank with no heading left to clear the filter from.
    pub(crate) fn forget_columns_beyond(&mut self, number_of_cols: usize) {
        self.column_filters
            .retain(|col, _| col.get() < number_of_cols);
    }
}

impl TableDataEngine {
    pub(crate) fn has_active_filters(&self, col: AnyColumn) -> bool {
        self.filter_stack.column_filters.contains_key(&col)
    }

    /// Whether any column at all is filtered, which is what the blue row
    /// numbers and the toolbar's "clear every filter" button key off.
    pub(crate) fn has_any_filters(&self) -> bool {
        !self.filter_stack.is_empty()
    }

    /// The text condition on `col`, when that is what it is filtered by.
    pub(crate) fn text_filter(&self, col: AnyColumn) -> Option<&TextFilter> {
        match self.filter_stack.column_filters.get(&col) {
            Some(ColumnFilter::Text(filter)) => Some(filter),
            _ => None,
        }
    }

    /// Marks an entry unavailable if choosing it would leave zero rows, so users
    /// aren't offered filter values that lead to an empty table. Every other
    /// active column's filter counts toward this, regardless of activation
    /// order: `column`'s own filter is excluded (so its own current selection
    /// doesn't block its other values), but a column filtered after `column`
    /// blocks exactly as much as one filtered before it.
    pub(crate) fn get_filters_for_column(
        &self,
        column: AnyColumn,
    ) -> anyhow::Result<Arc<Vec<(FilterEntry, FilterEntryState)>>> {
        let all_column_entries = self
            .all_filters
            .get(&column)
            .ok_or_else(|| anyhow::anyhow!("Expected {column:?} to have filter entries"))?;

        // A text condition is not a set of values, so while one is in force none
        // of the checklist's entries is checked; the popover reports the
        // condition itself instead.
        let applied_values = match self.filter_stack.column_filters.get(&column) {
            Some(ColumnFilter::Values(values)) => Some(values),
            _ => None,
        };

        // Rows that survive every *other* active column's filter. `column`'s own
        // filter is excluded so its entries reflect what selecting them would
        // add rather than being constrained by the current selection.
        let rows_passing_other_filters =
            retain_rows(&self.contents.rows, &self.filter_stack, Some(column));

        // Only used to populate `Unavailable::blocked_by`, which the UI doesn't
        // display by name; any other currently active column is a valid choice.
        let blocking_column = self
            .filter_stack
            .column_filters
            .keys()
            .find(|&&col| col != column)
            .copied();

        all_column_entries
            .iter()
            .map(|entry| {
                let adjusted_rows: Vec<DataRow> = entry
                    .rows
                    .iter()
                    .filter(|row| rows_passing_other_filters.contains(row))
                    .copied()
                    .collect();

                let is_applied =
                    applied_values.is_some_and(|values| values.contains(&entry.content));
                let state = if adjusted_rows.is_empty() {
                    let blocked_by = blocking_column.ok_or_else(|| {
                        anyhow::anyhow!(
                            "Expected an active column other than {column:?} to block \
                             {:?} when it has no rows passing other filters",
                            entry.content
                        )
                    })?;
                    FilterEntryState::Unavailable {
                        blocked_by,
                        is_applied,
                    }
                } else {
                    FilterEntryState::Available { is_applied }
                };

                Ok((
                    FilterEntry {
                        content: entry.content.clone(),
                        rows: adjusted_rows,
                    },
                    state,
                ))
            })
            .collect::<anyhow::Result<Vec<_>>>()
            .map(Arc::new)
    }

    pub(crate) fn clear_filters_for_col(&mut self, col: AnyColumn) {
        self.filter_stack.column_filters.remove(&col);
    }

    /// Drops every column's filter at once.
    pub(crate) fn clear_all_filters(&mut self) {
        self.filter_stack.column_filters.clear();
    }

    /// Checks or unchecks one value of a column's checklist.
    ///
    /// Unchecking the last checked value leaves the column unfiltered rather
    /// than filtered down to nothing, which is the only reading of an empty
    /// checklist that leaves the user a way back.
    pub(crate) fn toggle_filter(&mut self, column: AnyColumn, value: Option<SharedString>) {
        match self.filter_stack.column_filters.get_mut(&column) {
            Some(ColumnFilter::Values(values)) => {
                if !values.contains(&value) {
                    values.insert(value);
                } else if values.len() == 1 {
                    self.filter_stack.column_filters.remove(&column);
                } else {
                    values.remove(&value);
                }
            }
            // Checking a value while a text condition is in force replaces the
            // condition: the two are alternatives, and the last one set wins.
            Some(filter) => *filter = ColumnFilter::Values(HashSet::from_iter([value])),
            None => {
                self.filter_stack
                    .column_filters
                    .insert(column, ColumnFilter::Values(HashSet::from_iter([value])));
            }
        }
    }

    /// Replaces whatever `column` was filtered by with this set of values. An
    /// empty set would leave no rows at all, so it clears the column instead.
    pub(crate) fn set_value_filter(
        &mut self,
        column: AnyColumn,
        values: HashSet<Option<SharedString>>,
    ) {
        if values.is_empty() {
            self.filter_stack.column_filters.remove(&column);
            return;
        }
        self.filter_stack
            .column_filters
            .insert(column, ColumnFilter::Values(values));
    }

    /// Replaces whatever `column` was filtered by with a text condition. A
    /// condition that reads the typed text but was given none keeps every row,
    /// so it clears the column instead.
    pub(crate) fn set_text_filter(
        &mut self,
        column: AnyColumn,
        op: TextFilterOp,
        needle: SharedString,
    ) {
        if op.needs_needle() && needle.is_empty() {
            self.filter_stack.column_filters.remove(&column);
            return;
        }
        self.filter_stack
            .column_filters
            .insert(column, ColumnFilter::Text(TextFilter::new(op, needle)));
    }
}

/// Calculate available filter entries for each column from the table data.
pub fn calculate_available_filters(
    content_rows: &[TableRow<TableCell>],
    number_of_cols: usize,
) -> HashMap<AnyColumn, Vec<FilterEntry>> {
    let mut available_filters = HashMap::new();

    for col_idx in 0..number_of_cols {
        let column = AnyColumn::new(col_idx);
        let mut value_to_rows: HashMap<Option<SharedString>, Vec<DataRow>> = HashMap::new();

        for (row_index, row) in content_rows.iter().enumerate() {
            let cell_value = row
                .get(column)
                .and_then(|cell| cell.display_value().cloned());
            value_to_rows
                .entry(cell_value)
                .or_default()
                .push(DataRow(row_index));
        }

        let filter_entries: Vec<FilterEntry> = value_to_rows
            .into_iter()
            .map(|(content, rows)| FilterEntry { content, rows })
            .collect();

        available_filters.insert(column, filter_entries);
    }

    available_filters
}

/// Returns the set of data rows that survive all active filters in the stack,
/// optionally ignoring the filter on `exclude` without cloning the stack.
pub fn retain_rows(
    content_rows: &[TableRow<TableCell>],
    filter_stack: &FilterStack,
    exclude: Option<AnyColumn>,
) -> HashSet<DataRow> {
    let config = &filter_stack.column_filters;
    if config.is_empty() {
        return (0..content_rows.len()).map(DataRow).collect();
    }

    content_rows
        .iter()
        .enumerate()
        .filter(|(_, row)| {
            config.iter().all(|(col, filter)| {
                if Some(*col) == exclude {
                    return true;
                }
                filter.keeps(row.get(*col).and_then(|cell| cell.display_value()))
            })
        })
        .map(|(index, _)| DataRow(index))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TableLikeContent;
    use proptest::prelude::*;

    fn small_value() -> impl Strategy<Value = &'static str> {
        prop::sample::select(&["a", "b", "c"])
    }

    /// Generates a small rectangular table (header + data rows) built
    /// from a tiny value alphabet, so duplicate values across rows/columns
    /// are frequent enough to exercise cross-column blocking. Returns the
    /// column count alongside the table text since callers need it to bound
    /// generated column indices.
    fn table_text_strategy() -> impl Strategy<Value = (usize, String)> {
        (2usize..=6, 2usize..=3).prop_flat_map(|(rows, cols)| {
            prop::collection::vec(prop::collection::vec(small_value(), cols), rows).prop_map(
                move |grid| {
                    let header = (0..cols)
                        .map(|c| format!("c{c}"))
                        .collect::<Vec<_>>()
                        .join(",");
                    let mut lines = vec![header];
                    lines.extend(grid.into_iter().map(|row| row.join(",")));
                    (cols, lines.join("\n"))
                },
            )
        })
    }

    fn build_engine(table_text: &str) -> TableDataEngine {
        let mut engine = TableDataEngine::default();
        engine.contents = Arc::new(TableLikeContent::from_str(table_text.to_string(), true));
        engine.calculate_available_filters();
        engine
    }

    /// Resolves raw `(column_index, value_index)` pairs (generated modulo
    /// nothing in particular, so out of range in general) into concrete,
    /// deduplicated `(AnyColumn, value)` toggles by reading each column's
    /// entries from an untouched engine and wrapping indices to valid ranges.
    fn resolve_toggles(
        engine: &TableDataEngine,
        cols: usize,
        raw: &[(usize, usize)],
    ) -> Vec<(AnyColumn, Option<SharedString>)> {
        let mut toggles = Vec::new();
        for &(col_idx, value_idx) in raw {
            let column = AnyColumn::new(col_idx % cols);
            let entries = engine.get_filters_for_column(column).unwrap();
            if entries.is_empty() {
                continue;
            }
            let value = entries[value_idx % entries.len()].0.content.clone();
            let pair = (column, value);
            if !toggles.contains(&pair) {
                toggles.push(pair);
            }
        }
        toggles
    }

    /// Per-column `(value, is_available, is_applied)` triples, flattening
    /// `FilterEntryState` for comparison while dropping `blocked_by`: any
    /// other currently active column is an equally valid blocker, so its
    /// *identity* legitimately differs across activation orders, but
    /// availability and applied-ness must not.
    type StateSnapshot = Vec<(AnyColumn, Vec<(Option<SharedString>, bool, bool)>)>;

    fn apply_and_snapshot(
        engine: &mut TableDataEngine,
        cols: usize,
        toggles: &[(AnyColumn, Option<SharedString>)],
    ) -> StateSnapshot {
        for (column, value) in toggles {
            engine.toggle_filter(*column, value.clone());
        }

        (0..cols)
            .map(|col_idx| {
                let column = AnyColumn::new(col_idx);
                let entries = engine.get_filters_for_column(column).unwrap();
                let mut column_snapshot: Vec<(Option<SharedString>, bool, bool)> = entries
                    .iter()
                    .map(|(entry, state)| match *state {
                        FilterEntryState::Available { is_applied } => {
                            (entry.content.clone(), true, is_applied)
                        }
                        FilterEntryState::Unavailable { is_applied, .. } => {
                            (entry.content.clone(), false, is_applied)
                        }
                    })
                    .collect();
                // `all_column_entries` comes from a `HashMap`, so its iteration
                // order isn't stable across engine instances built from the
                // same data; sort so that's not mistaken for a real divergence.
                column_snapshot.sort();
                (column, column_snapshot)
            })
            .collect()
    }

    /// The values of column 0 that survive the engine's filters, in row order,
    /// with a virtual (missing) cell written as `<なし>`.
    fn kept_values(engine: &TableDataEngine) -> Vec<String> {
        let mut rows: Vec<DataRow> = retain_rows(&engine.contents.rows, &engine.filter_stack, None)
            .into_iter()
            .collect();
        rows.sort();
        rows.iter()
            .map(|row| {
                engine
                    .contents
                    .rows
                    .get(**row)
                    .and_then(|row| row.get(AnyColumn(0)))
                    .and_then(|cell| cell.display_value())
                    .map(|value| value.as_ref().to_owned())
                    .unwrap_or_else(|| "<なし>".to_owned())
            })
            .collect()
    }

    /// `matches` reads a missing cell, which no fixture table can produce in its
    /// first column, so the two blank operators are checked against it directly.
    #[test]
    fn blank_covers_a_missing_cell() {
        assert!(TextFilter::new(TextFilterOp::Blank, "".into()).matches(None));
        assert!(!TextFilter::new(TextFilterOp::NotBlank, "".into()).matches(None));
        // A missing cell has no text to contain anything either.
        assert!(!TextFilter::new(TextFilterOp::Contains, "a".into()).matches(None));
        assert!(TextFilter::new(TextFilterOp::NotContains, "a".into()).matches(None));
    }

    fn kept_with_text_filter(table_text: &str, op: TextFilterOp, needle: &str) -> Vec<String> {
        let mut engine = build_engine(table_text);
        engine.set_text_filter(AnyColumn(0), op, needle.into());
        kept_values(&engine)
    }

    /// Case is ignored on both sides, and multibyte values are compared whole
    /// rather than by byte, which a naive slice of the haystack would break on.
    #[test]
    fn text_conditions_ignore_case_and_survive_multibyte_values() {
        const TABLE: &str = "c0\nApfel\napfelsaft\nBirne\n日本語\nに日本";

        assert_eq!(
            kept_with_text_filter(TABLE, TextFilterOp::Contains, "APFEL"),
            vec!["Apfel", "apfelsaft"]
        );
        assert_eq!(
            kept_with_text_filter(TABLE, TextFilterOp::NotContains, "apfel"),
            vec!["Birne", "日本語", "に日本"]
        );
        assert_eq!(
            kept_with_text_filter(TABLE, TextFilterOp::Equals, "apfel"),
            vec!["Apfel"]
        );
        assert_eq!(
            kept_with_text_filter(TABLE, TextFilterOp::StartsWith, "日本"),
            vec!["日本語"]
        );
        assert_eq!(
            kept_with_text_filter(TABLE, TextFilterOp::EndsWith, "日本"),
            vec!["に日本"]
        );
    }

    /// An empty cell and a cell holding only spaces both read as empty in the
    /// grid, so "blank" has to cover them both.
    #[test]
    fn blank_covers_empty_and_whitespace_cells() {
        const TABLE: &str = "c0,c1\na,1\n,2\n   ,3\nb,4";

        assert_eq!(
            kept_with_text_filter(TABLE, TextFilterOp::Blank, ""),
            vec!["", "   "]
        );
        assert_eq!(
            kept_with_text_filter(TABLE, TextFilterOp::NotBlank, ""),
            vec!["a", "b"]
        );
    }

    /// The needle-reading operators filter nothing without a needle, and a
    /// column left in that state has to come out unfiltered rather than empty.
    #[test]
    fn a_condition_without_a_needle_leaves_the_column_unfiltered() {
        let mut engine = build_engine("c0\na\nb");
        engine.set_text_filter(AnyColumn(0), TextFilterOp::Contains, "".into());

        assert!(!engine.has_active_filters(AnyColumn(0)));
        assert!(!engine.has_any_filters());
        assert_eq!(kept_values(&engine), vec!["a", "b"]);
    }

    /// The checklist and the text condition are alternatives: setting either one
    /// replaces the other, and neither leaves the column half filtered by both.
    #[test]
    fn a_text_condition_and_the_value_checklist_replace_each_other() {
        let mut engine = build_engine("c0\nabc\nabd\nxyz");
        let column = AnyColumn(0);

        engine.toggle_filter(column, Some("xyz".into()));
        assert_eq!(kept_values(&engine), vec!["xyz"]);

        engine.set_text_filter(column, TextFilterOp::StartsWith, "ab".into());
        assert_eq!(kept_values(&engine), vec!["abc", "abd"]);
        assert!(engine.text_filter(column).is_some());
        // Nothing in the checklist reads as checked while the condition stands.
        let entries = engine
            .get_filters_for_column(column)
            .expect("the column has filter entries");
        assert!(
            entries.iter().all(|(_, state)| matches!(
                state,
                FilterEntryState::Available { is_applied: false }
            )),
            "a text condition should leave every checklist entry unchecked: {entries:?}"
        );

        // And back: checking a value drops the condition instead of narrowing it.
        engine.toggle_filter(column, Some("xyz".into()));
        assert!(engine.text_filter(column).is_none());
        assert_eq!(kept_values(&engine), vec!["xyz"]);
    }

    /// A text condition on one column has to compose with another column's
    /// checklist, and clearing everything has to bring every row back.
    #[test]
    fn filters_on_several_columns_compose_and_clear() {
        let mut engine = build_engine("c0,c1\nabc,keep\nabd,drop\nxyz,keep");

        engine.set_text_filter(AnyColumn(0), TextFilterOp::StartsWith, "ab".into());
        engine.toggle_filter(AnyColumn(1), Some("keep".into()));
        assert_eq!(kept_values(&engine), vec!["abc"]);

        engine.clear_filters_for_col(AnyColumn(0));
        assert_eq!(kept_values(&engine), vec!["abc", "xyz"]);

        engine.clear_all_filters();
        assert!(!engine.has_any_filters());
        assert_eq!(kept_values(&engine), vec!["abc", "abd", "xyz"]);
    }

    /// Unchecking the last checked value has to leave the column unfiltered, not
    /// filtered down to no rows at all.
    #[test]
    fn unchecking_the_last_value_leaves_the_column_unfiltered() {
        let mut engine = build_engine("c0\na\nb");
        let column = AnyColumn(0);

        engine.toggle_filter(column, Some("a".into()));
        assert_eq!(kept_values(&engine), vec!["a"]);

        engine.toggle_filter(column, Some("a".into()));
        assert!(!engine.has_active_filters(column));
        assert_eq!(kept_values(&engine), vec!["a", "b"]);
    }

    /// The "select everything the search narrowed the list to" button hands the
    /// engine a set of values; an empty one has to read as "no filter".
    #[test]
    fn a_value_set_replaces_the_column_filter_and_an_empty_one_clears_it() {
        let mut engine = build_engine("c0\na\nb\nc");
        let column = AnyColumn(0);

        engine.set_text_filter(column, TextFilterOp::Equals, "a".into());
        engine.set_value_filter(column, HashSet::from_iter([Some("b".into())]));
        assert!(engine.text_filter(column).is_none());
        assert_eq!(kept_values(&engine), vec!["b"]);

        engine.set_value_filter(column, HashSet::new());
        assert!(!engine.has_active_filters(column));
        assert_eq!(kept_values(&engine), vec!["a", "b", "c"]);
    }

    /// Deleting a filtered column reparses the buffer into a table that has no
    /// such column; the filter has to go with it rather than hide every row for
    /// want of a cell to read.
    #[test]
    fn a_reparse_forgets_the_filter_of_a_column_that_is_gone() {
        let mut engine = build_engine("c0,c1\na,1\nb,2");
        engine.toggle_filter(AnyColumn(1), Some("1".into()));
        assert_eq!(kept_values(&engine), vec!["a"]);

        // What deleting the second column leaves the grid to reparse.
        engine.contents = Arc::new(TableLikeContent::from_str("c0\na\nb".to_owned(), true));
        engine.calculate_available_filters();

        assert!(!engine.has_any_filters());
        assert_eq!(kept_values(&engine), vec!["a", "b"]);
    }

    /// The filter of a column that is still there survives the reparse that an
    /// edit to another column triggers.
    #[test]
    fn a_reparse_keeps_the_filter_of_a_column_that_remains() {
        let mut engine = build_engine("c0,c1\na,1\nb,2");
        engine.toggle_filter(AnyColumn(0), Some("a".into()));

        engine.contents = Arc::new(TableLikeContent::from_str(
            "c0,c1\na,9\nb,2".to_owned(),
            true,
        ));
        engine.calculate_available_filters();

        assert!(engine.has_active_filters(AnyColumn(0)));
        assert_eq!(kept_values(&engine), vec!["a"]);
    }

    proptest! {
        #[test]
        fn filter_states_are_order_independent(
            (cols, table_text) in table_text_strategy(),
            raw_toggles in prop::collection::vec((0usize..3, 0usize..3), 0..8),
        ) {
            let toggles = resolve_toggles(&build_engine(&table_text), cols, &raw_toggles);

            let mut forward_engine = build_engine(&table_text);
            let forward = apply_and_snapshot(&mut forward_engine, cols, &toggles);

            let mut reversed = toggles.clone();
            reversed.reverse();
            let mut backward_engine = build_engine(&table_text);
            let backward = apply_and_snapshot(&mut backward_engine, cols, &reversed);

            prop_assert_eq!(forward, backward);
        }
    }
}
