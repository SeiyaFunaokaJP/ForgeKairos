//! Ctrl+F and Ctrl+H over the grid.
//!
//! The grid joins Zed's ordinary buffer search bar by implementing
//! [`SearchableItem`], so the query field, the option toggles, the match
//! counter and the replace controls are the ones every other tab uses. What is
//! particular to a spreadsheet lives here: a hit is a byte range inside one
//! cell's value, and "the next hit" is resolved in the order the rows are
//! displayed rather than the order they sit in the file.

use std::{
    cmp::Reverse,
    collections::{BTreeMap, HashMap, HashSet},
    ops::Range,
    rc::Rc,
    sync::Arc,
};

use editor::EditorSettings;
use gpui::{App, Context, Hsla, Task, Window};
use project::search::SearchQuery;
use settings::{SeedQuerySetting, Settings as _};
use ui::prelude::*;
use workspace::searchable::{
    Direction, FilteredSearchRange, SearchEvent, SearchOptions, SearchToken, SearchableItem,
};

use crate::{
    TabularDataPreviewPane,
    edit_engine::RowRef,
    grid::{GridCell, GridRect, GridRow},
    types::{AnyColumn, DataRow, DisplayRow, TableLikeContent},
};

/// Largest number of hits the grid hands the search bar. A query that matches
/// more than this is answered with the first `MAX_SEARCH_MATCHES` of them: the
/// counter is meaningless at that size and the highlight set would cost more
/// than it is worth.
const MAX_SEARCH_MATCHES: usize = 100_000;

/// Longest cell value the search bar is seeded with. A whole paragraph pasted
/// into one cell is not a query anybody meant to type.
const MAX_QUERY_SUGGESTION_CHARS: usize = 128;

const TOO_MANY_MATCHES_MESSAGE: &str = "一致が多すぎるため先頭 100,000 件だけ対象です";

/// One hit: a byte range inside the value of one cell.
///
/// The row is held in data coordinates, so that a sort or filter applied while
/// the search bar is open cannot make a hit point at a different row than the
/// one it was found in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GridMatch {
    pub(crate) row: RowRef,
    pub(crate) col: AnyColumn,
    pub(crate) range: Range<usize>,
}

/// What the search bar last told the grid to show.
#[derive(Default)]
pub(crate) struct GridSearchState {
    /// Cells holding at least one hit, which the renderer tints.
    matched_cells: HashSet<(RowRef, AnyColumn)>,
    active_cell: Option<(RowRef, AnyColumn)>,
    /// The rectangle "search in selection" pinned the search to.
    filtered_range: Option<GridRect>,
    /// The rectangle the toggle pinned last, so that turning it off and back on
    /// comes back to it instead of to whatever the caret has since selected.
    previous_filtered_range: Option<GridRect>,
    /// Whether the last search ran into [`MAX_SEARCH_MATCHES`].
    truncated: bool,
    /// The row order built for one generation of the display mapping. Walking
    /// hits and moving the caret both need it, and rebuilding it per keystroke
    /// would cost one hash map entry per visible row.
    row_order: Option<(u64, Rc<RowOrder>)>,
}

/// The rows the grid is showing, in display order, in a form a background scan
/// can own.
enum BodyRows {
    /// Display row `n` is data row `n`, for this many rows.
    Identity(usize),
    /// The data row behind each visible display row, in display order.
    Mapped(Vec<DataRow>),
}

impl BodyRows {
    fn get(&self, display_row: usize) -> Option<DataRow> {
        match self {
            BodyRows::Identity(count) => (display_row < *count).then_some(DataRow(display_row)),
            BodyRows::Mapped(rows) => rows.get(display_row).copied(),
        }
    }
}

/// Everything one search needs to walk the visible cells off the main thread.
struct SearchScope {
    contents: Arc<TableLikeContent>,
    body: BodyRows,
    has_header_row: bool,
    /// Grid row indices to scan, in display order.
    rows: Range<usize>,
    cols: Range<usize>,
}

impl SearchScope {
    fn row_ref(&self, row_index: usize) -> Option<RowRef> {
        match row_index {
            0 => self.has_header_row.then_some(RowRef::Header),
            row_index => self.body.get(row_index - 1).map(RowRef::Data),
        }
    }

    /// Every hit in the scope, in display order: top to bottom, left to right,
    /// and within a cell from the start of its value. The flag says whether the
    /// scan stopped at [`MAX_SEARCH_MATCHES`] with rows still to go.
    fn collect_matches(&self, query: &SearchQuery) -> (Vec<GridMatch>, bool) {
        let mut matches = Vec::new();
        for row_index in self.rows.clone() {
            let Some(row_ref) = self.row_ref(row_index) else {
                continue;
            };
            let row = match row_ref {
                RowRef::Header => Some(&self.contents.headers),
                RowRef::Data(data_row) => self.contents.get_row(data_row),
            };
            let Some(row) = row else {
                continue;
            };
            for col in self.cols.clone().map(AnyColumn) {
                let Some(value) = row.get(col).and_then(|cell| cell.display_value()) else {
                    continue;
                };
                for range in query.search_str(value) {
                    matches.push(GridMatch {
                        row: row_ref,
                        col,
                        range,
                    });
                    if matches.len() >= MAX_SEARCH_MATCHES {
                        return (matches, true);
                    }
                }
            }
        }
        (matches, false)
    }
}

/// Where each visible row sits in the grid, which is how a hit found in data
/// coordinates is turned back into a place the caret can go.
enum RowOrder {
    Identity(usize),
    Mapped(HashMap<DataRow, usize>),
}

impl RowOrder {
    fn grid_row_index(&self, row: RowRef) -> Option<usize> {
        match row {
            RowRef::Header => Some(0),
            RowRef::Data(data_row) => match self {
                RowOrder::Identity(count) => (*data_row < *count).then_some(*data_row + 1),
                RowOrder::Mapped(rows) => rows.get(&data_row).copied(),
            },
        }
    }
}

impl TabularDataPreviewPane {
    /// The background the search bar gives a cell, if any. Drawn over the
    /// selection: a hit the user is being walked through has to stay visible
    /// inside a range they happen to have selected.
    pub(crate) fn search_match_background(&self, cell: GridCell, cx: &App) -> Option<Hsla> {
        if self.grid.search.matched_cells.is_empty() {
            return None;
        }
        let key = (self.row_ref(cell.row)?, cell.col);
        if !self.grid.search.matched_cells.contains(&key) {
            return None;
        }
        let colors = cx.theme().colors();
        Some(if self.grid.search.active_cell == Some(key) {
            colors.search_active_match_background
        } else {
            colors.search_match_background
        })
    }

    /// The faint outline that shows which rectangle "search in selection"
    /// pinned the search to, so that a query finding nothing outside it does
    /// not read as a query finding nothing.
    pub(crate) fn search_range_border(&self, cell: GridCell, cx: &App) -> Option<Hsla> {
        let range = self.grid.search.filtered_range?;
        range
            .contains(cell.row, cell.col)
            .then(|| cx.theme().colors().border_selected.opacity(0.5))
    }

    /// Tells the search bar that the hits it holds may no longer be where it
    /// found them, so that it looks for them again.
    ///
    /// A hit is a byte range inside a cell value plus a row in data
    /// coordinates, so a reparse (which rebuilds every value) and a filter or
    /// sort change (which moves the rows) both invalidate it. This is called
    /// where the two settle together - the contents are already swapped and the
    /// display mapping has just been recomputed - because a search run any
    /// earlier would hand the bar ranges into text the grid no longer holds,
    /// and a replacement driven from those would write at the wrong offset.
    pub(crate) fn invalidate_search_matches(&mut self, cx: &mut Context<Self>) {
        cx.emit(SearchEvent::MatchesInvalidated);
    }

    /// Tells the search bar the caret moved, so that the `n/m` counter follows
    /// the cell the user is standing on.
    pub(crate) fn notify_search_of_caret(&mut self, cx: &mut Context<Self>) {
        cx.emit(SearchEvent::ActiveMatchChanged);
    }

    /// The data row behind every visible display row.
    ///
    /// Asking the mapping row by row would cost a vector the size of the file
    /// on every keystroke in the query field, so the identity case - no sort,
    /// no filter - stays a pair of numbers.
    fn visible_body_rows(&self) -> BodyRows {
        let count = self.body_row_count();
        let mapping = self.engine.d2d_mapping();
        if mapping.is_identity() {
            return BodyRows::Identity(count);
        }
        BodyRows::Mapped(
            (0..count)
                .filter_map(|display_row| mapping.get_data_row(DisplayRow(display_row)))
                .collect(),
        )
    }

    fn row_order(&mut self) -> Rc<RowOrder> {
        let generation = self.engine.d2d_generation();
        if let Some((cached_generation, order)) = &self.grid.search.row_order
            && *cached_generation == generation
        {
            return order.clone();
        }
        let order = Rc::new(match self.visible_body_rows() {
            BodyRows::Identity(count) => RowOrder::Identity(count),
            BodyRows::Mapped(rows) => RowOrder::Mapped(
                rows.into_iter()
                    .enumerate()
                    .map(|(display_row, data_row)| (data_row, display_row + 1))
                    .collect(),
            ),
        });
        self.grid.search.row_order = Some((generation, order.clone()));
        order
    }

    fn search_scope(&self) -> SearchScope {
        let mut scope = SearchScope {
            contents: self.engine.contents.clone(),
            body: self.visible_body_rows(),
            has_header_row: self.row_ref(GridRow::Header).is_some(),
            rows: 0..0,
            cols: 0..0,
        };
        if self.grid_is_empty() {
            return scope;
        }
        let first_row = self.first_row_index();
        let last_row = self.last_row_index();
        let last_col = self.last_col_index();
        let rect = self.grid.search.filtered_range.unwrap_or(GridRect {
            first_row,
            last_row,
            first_col: 0,
            last_col,
        });
        scope.rows = rect.first_row.max(first_row)..rect.last_row.min(last_row) + 1;
        scope.cols = rect.first_col..rect.last_col.min(last_col) + 1;
        scope
    }

    /// Where a hit sits in the grid, for comparison against the caret.
    fn match_position(order: &RowOrder, found: &GridMatch) -> Option<(usize, usize, usize)> {
        Some((
            order.grid_row_index(found.row)?,
            found.col.get(),
            found.range.start,
        ))
    }

    /// Rewrites every cell the hits fall in as one transaction, so that the
    /// whole replacement is a single undo step.
    ///
    /// A hit is a byte range into the value the search saw. The bar can hand
    /// back a range the cell no longer holds - the file was edited in its text
    /// tab, or an earlier replacement in the same run changed the value - and
    /// writing at such an offset would corrupt the cell, so every range is
    /// checked against the value as it stands now and skipped when it no longer
    /// matches.
    fn replace_matches<'a>(
        &mut self,
        matches: impl Iterator<Item = &'a GridMatch>,
        query: &SearchQuery,
        cx: &mut Context<Self>,
    ) {
        let mut ranges_by_cell: BTreeMap<(RowRef, AnyColumn), Vec<Range<usize>>> = BTreeMap::new();
        for found in matches {
            ranges_by_cell
                .entry((found.row, found.col))
                .or_default()
                .push(found.range.clone());
        }
        if ranges_by_cell.is_empty() {
            return;
        }

        let mut cells = Vec::with_capacity(ranges_by_cell.len());
        let mut stale = 0usize;
        for ((row, col), mut ranges) in ranges_by_cell {
            let value = self.cell_value_at(row, col);
            let current = query.search_str(&value);
            let mut replaced = value.to_string();
            // Rewriting from the back leaves the byte ranges of the hits before
            // it pointing at the same text they were found in.
            ranges.sort_unstable_by_key(|range| Reverse(range.start));
            let mut rewritten_from = replaced.len();
            for range in ranges {
                if !current.contains(&range) {
                    stale += 1;
                    continue;
                }
                // Two hits of the same run can only overlap if the query found
                // them that way, which it does not, but a duplicate range in
                // the list would otherwise be applied twice.
                if range.end > rewritten_from {
                    continue;
                }
                // The hit is expanded against the cell as found, so lookaround
                // assertions see the same context the search did.
                let replacement = match query.replacement_for(&value, range.clone()) {
                    Some(replacement) => replacement.into_owned(),
                    None => continue,
                };
                rewritten_from = range.start;
                replaced.replace_range(range, &replacement);
            }
            if replaced.as_str() != value.as_ref() {
                cells.push((row, col, replaced));
            }
        }

        if !cells.is_empty() {
            let edits = self
                .with_edit_engine(cx, |engine| engine.set_cells(&cells))
                .unwrap_or_default();
            for (row, col, value) in &cells {
                self.write_cached_cell(*row, *col, value);
            }
            self.apply_buffer_edits(edits, cx);
        }
        if stale > 0 {
            self.set_status_message(
                format!("{stale} 件は内容が変わっていたため置換しませんでした"),
                cx,
            );
        } else {
            self.clear_status_message();
        }
        cx.notify();
    }
}

impl SearchableItem for TabularDataPreviewPane {
    type Match = GridMatch;

    fn supported_options(&self) -> SearchOptions {
        SearchOptions {
            case: true,
            word: true,
            regex: true,
            replacement: true,
            selection: true,
            select_all: true,
            find_in_results: false,
        }
    }

    fn search_bar_visibility_changed(
        &mut self,
        visible: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !visible {
            self.grid.search.filtered_range = None;
            self.grid.search.previous_filtered_range = None;
            self.grid.search.truncated = false;
            self.clear_matches(window, cx);
        }
    }

    fn has_filtered_search_ranges(&mut self) -> bool {
        self.grid.search.filtered_range.is_some()
    }

    /// Pins the search to the selected rectangle, the way the editor pins it to
    /// the selected text. A caret sitting on a single cell is no range at all -
    /// and activating a hit leaves it that way - so turning the toggle back on
    /// comes back to the rectangle the last one pinned. Only closing the bar
    /// forgets it.
    ///
    /// A search range is one rectangle, so a multiple selection is pinned by
    /// the rectangle around all of its blocks rather than by the blocks
    /// themselves; the corners between two blocks are therefore searched too.
    fn toggle_filtered_search_ranges(
        &mut self,
        enabled: Option<FilteredSearchRange>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(pinned) = self.grid.search.filtered_range.take() {
            self.grid.search.previous_filtered_range = Some(pinned);
        }
        if let Some(requested) = enabled {
            let rect = self.selection_bounding_rect();
            if rect.cell_count() > 1 {
                self.grid.search.filtered_range = Some(rect);
            } else if requested != FilteredSearchRange::Selection {
                self.grid.search.filtered_range = self.grid.search.previous_filtered_range;
            }
        }
        cx.notify();
    }

    fn clear_matches(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.grid.search.matched_cells.is_empty() && self.grid.search.active_cell.is_none() {
            return;
        }
        self.grid.search.matched_cells.clear();
        self.grid.search.active_cell = None;
        cx.notify();
    }

    fn update_matches(
        &mut self,
        matches: &[Self::Match],
        active_match_index: Option<usize>,
        _token: SearchToken,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.grid.search.matched_cells =
            matches.iter().map(|found| (found.row, found.col)).collect();
        self.grid.search.active_cell = active_match_index
            .and_then(|index| matches.get(index))
            .map(|found| (found.row, found.col));
        cx.notify();
    }

    fn query_suggestion(
        &mut self,
        seed_query_override: Option<SeedQuerySetting>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> String {
        let setting = seed_query_override
            .unwrap_or_else(|| EditorSettings::get_global(cx).seed_search_query_from_cursor);
        if setting == SeedQuerySetting::Never {
            return String::new();
        }
        // An open cell is an ordinary editor, so what it would seed a search
        // with is exactly what the editor itself says.
        if self.grid.editing.is_some() {
            let editor = self.grid.cell_editor.clone();
            let suggestion = editor.update(cx, |editor, cx| {
                SearchableItem::query_suggestion(editor, seed_query_override, window, cx)
            });
            if !suggestion.is_empty() {
                return suggestion;
            }
        }
        if setting != SeedQuerySetting::Always {
            return String::new();
        }
        let value = self.cell_value(self.grid.cursor.row, self.grid.cursor.col);
        let first_line = value.lines().next().unwrap_or_default();
        if first_line.trim().is_empty() || first_line.chars().count() > MAX_QUERY_SUGGESTION_CHARS {
            return String::new();
        }
        first_line.to_owned()
    }

    fn activate_match(
        &mut self,
        index: usize,
        matches: &[Self::Match],
        _token: SearchToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Moving the caret onto a hit must not carry a half typed value with
        // it, and the search never opens a cell of its own.
        self.commit_editing(window, cx);
        let order = self.row_order();
        // A hit whose row a filter has hidden since the search ran is skipped
        // rather than taking the caret nowhere.
        let found = matches
            .get(index..)
            .unwrap_or_default()
            .iter()
            .enumerate()
            .find_map(|(skipped, found)| Some((skipped, found, order.grid_row_index(found.row)?)));
        let Some((skipped, found, row_index)) = found else {
            return;
        };
        self.grid.search.active_cell = Some((found.row, found.col));
        self.select_cell(
            GridCell::new(GridRow::from_index(row_index), found.col),
            false,
            cx,
        );
        self.scroll_cursor_into_view(window, cx);
        if skipped > 0 {
            // The bar still believes `index` is active; the caret went somewhere
            // else, so it has to recount from where the caret actually landed.
            self.notify_search_of_caret(cx);
        }
        cx.notify();
    }

    /// The grid's selection is one rectangle, so selecting every hit is only
    /// possible when the hits fill one. Otherwise the caret goes to the first.
    fn select_matches(
        &mut self,
        matches: &[Self::Match],
        token: SearchToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let order = self.row_order();
        let mut cells: Vec<(usize, usize)> = matches
            .iter()
            .filter_map(|found| Self::match_position(&order, found))
            .map(|(row, col, _)| (row, col))
            .collect();
        cells.sort_unstable();
        cells.dedup();
        let (Some((first_row, _)), Some((last_row, _))) = (cells.first(), cells.last()) else {
            return;
        };
        let (first_row, last_row) = (*first_row, *last_row);
        let first_col = cells.iter().map(|(_, col)| *col).min().unwrap_or_default();
        let last_col = cells.iter().map(|(_, col)| *col).max().unwrap_or_default();
        let covered = (last_row - first_row + 1).saturating_mul(last_col - first_col + 1);
        if covered != cells.len() {
            self.activate_match(0, matches, token, window, cx);
            return;
        }

        self.commit_editing(window, cx);
        self.select_cell(
            GridCell::new(GridRow::from_index(first_row), AnyColumn(first_col)),
            false,
            cx,
        );
        self.select_cell(
            GridCell::new(GridRow::from_index(last_row), AnyColumn(last_col)),
            true,
            cx,
        );
        self.scroll_cursor_into_view(window, cx);
        cx.notify();
    }

    fn replace(
        &mut self,
        found: &Self::Match,
        query: &SearchQuery,
        _token: SearchToken,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_matches(std::iter::once(found), query, cx);
    }

    fn replace_all(
        &mut self,
        matches: &mut dyn Iterator<Item = &Self::Match>,
        query: &SearchQuery,
        _token: SearchToken,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_matches(matches, query, cx);
    }

    fn find_matches(
        &mut self,
        query: Arc<SearchQuery>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Vec<Self::Match>> {
        let scope = self.search_scope();
        let scan = cx.background_spawn(async move { scope.collect_matches(&query) });
        cx.spawn(async move |this, cx| {
            let (matches, truncated) = scan.await;
            this.update(cx, |this, cx| {
                if truncated != this.grid.search.truncated {
                    this.grid.search.truncated = truncated;
                    if truncated {
                        this.set_sticky_status_message(TOO_MANY_MATCHES_MESSAGE, cx);
                    } else {
                        this.clear_status_message();
                        cx.notify();
                    }
                }
            })
            .ok();
            matches
        })
    }

    fn active_match_index(
        &mut self,
        direction: Direction,
        matches: &[Self::Match],
        _token: SearchToken,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        if matches.is_empty() {
            return None;
        }
        let order = self.row_order();
        let cursor = self.grid.cursor;
        let (row, col) = (cursor.row.index(), cursor.col.get());
        // The hits in the caret's own cell count as the active one whichever
        // way the bar is about to step, which is what the editor does with a
        // caret sitting inside a match.
        Some(match direction {
            Direction::Next => matches
                .iter()
                .position(|found| {
                    Self::match_position(&order, found).is_some_and(|at| at >= (row, col, 0))
                })
                .unwrap_or(0),
            Direction::Prev => matches
                .iter()
                .rposition(|found| {
                    Self::match_position(&order, found)
                        .is_some_and(|at| at <= (row, col, usize::MAX))
                })
                .unwrap_or(matches.len() - 1),
        })
    }
}

#[cfg(test)]
mod tests {
    use editor::Editor;
    use fs::FakeFs;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use project::Project;
    use search::{
        BufferSearchBar, ReplaceAll, ReplaceNext, SearchOptions as BarOptions, SelectAllMatches,
        SelectNextMatch, SelectPreviousMatch, ToggleCaseSensitive,
    };
    use serde_json::json;
    use util::{path, paths::PathMatcher};
    use workspace::{AppState, MultiWorkspace};
    use zed_actions::buffer_search::{Deploy, Dismiss};

    use super::*;
    use crate::{
        table_data_engine::sorting_by_column::{AppliedSorting, SortDirection},
        types::DisplayRow,
    };

    fn text_query(query: &str, whole_word: bool, case_sensitive: bool) -> Arc<SearchQuery> {
        Arc::new(
            SearchQuery::text(
                query,
                whole_word,
                case_sensitive,
                false,
                PathMatcher::default(),
                PathMatcher::default(),
                false,
                None,
            )
            .expect("the test query should build"),
        )
    }

    fn regex_query(query: &str) -> Arc<SearchQuery> {
        Arc::new(
            SearchQuery::regex(
                query,
                false,
                false,
                false,
                false,
                PathMatcher::default(),
                PathMatcher::default(),
                false,
                None,
            )
            .expect("the test query should build"),
        )
    }

    fn found(row: RowRef, col: usize, range: Range<usize>) -> GridMatch {
        GridMatch {
            row,
            col: AnyColumn(col),
            range,
        }
    }

    /// Opens `contents` as a CSV grid that is the active item of a real
    /// workspace, with the buffer search bar in its pane's toolbar.
    async fn open_grid_with_search(
        contents: &str,
        cx: &mut TestAppContext,
    ) -> (
        Entity<TabularDataPreviewPane>,
        Entity<Editor>,
        Entity<BufferSearchBar>,
        VisualTestContext,
    ) {
        cx.update(|cx| {
            AppState::test(cx);
            editor::init(cx);
            search::buffer_search::init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ "data.csv": contents }))
            .await;
        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("the test workspace should be readable");
        let mut cx = VisualTestContext::from_window(window.into(), cx);

        let buffer = project
            .update(&mut cx, |project, cx| {
                project.open_local_buffer(path!("/project/data.csv"), cx)
            })
            .await
            .expect("the fixture buffer should open");
        let editor = cx.update(|window, cx| {
            cx.new(|cx| Editor::for_buffer(buffer, Some(project.clone()), window, cx))
        });
        let pane = workspace.update_in(&mut cx, |_workspace, window, cx| {
            TabularDataPreviewPane::new(&editor, window, cx)
        });
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(pane.clone()), None, true, window, cx);
        });
        let search_bar = workspace.update_in(&mut cx, |workspace, window, cx| {
            let search_bar = cx.new(|cx| BufferSearchBar::new(None, window, cx));
            workspace.active_pane().update(cx, |pane, cx| {
                pane.toolbar().update(cx, |toolbar, cx| {
                    toolbar.add_item(search_bar.clone(), window, cx);
                });
            });
            search_bar
        });
        cx.run_until_parked();
        (pane, editor, search_bar, cx)
    }

    fn deploy_search(cx: &mut VisualTestContext, action: Deploy) {
        cx.dispatch_action(action);
        cx.run_until_parked();
    }

    fn search_for(
        search_bar: &Entity<BufferSearchBar>,
        query: &str,
        options: BarOptions,
        cx: &mut VisualTestContext,
    ) {
        search_bar.update_in(cx, |search_bar, window, cx| {
            drop(search_bar.search(query, Some(options), true, window, cx));
        });
        cx.run_until_parked();
        // Typing in the query field activates the first hit once the search
        // lands; driving the bar from code has to do the same by hand.
        search_bar.update_in(cx, |search_bar, window, cx| {
            search_bar.activate_current_match(window, cx);
        });
        cx.run_until_parked();
    }

    fn cursor_cell(pane: &Entity<TabularDataPreviewPane>, cx: &VisualTestContext) -> GridCell {
        pane.read_with(cx, |pane, _| pane.grid.cursor)
    }

    fn highlighted_cells(
        pane: &Entity<TabularDataPreviewPane>,
        cx: &VisualTestContext,
    ) -> Vec<(RowRef, AnyColumn)> {
        pane.read_with(cx, |pane, _| {
            let mut cells: Vec<_> = pane.grid.search.matched_cells.iter().copied().collect();
            cells.sort_unstable();
            cells
        })
    }

    fn buffer_text(editor: &Entity<Editor>, cx: &VisualTestContext) -> String {
        editor.read_with(cx, |editor, cx| editor.text(cx))
    }

    async fn matches_for(
        pane: &Entity<TabularDataPreviewPane>,
        query: Arc<SearchQuery>,
        cx: &mut VisualTestContext,
    ) -> Vec<GridMatch> {
        pane.update_in(cx, |pane, window, cx| pane.find_matches(query, window, cx))
            .await
    }

    #[gpui::test]
    async fn test_matches_come_back_in_display_order(cx: &mut TestAppContext) {
        let (pane, _editor, _search_bar, mut cx) =
            open_grid_with_search("b,v\nbob,1\nx,b", cx).await;

        let matches = matches_for(&pane, text_query("b", false, false), &mut cx).await;
        assert_eq!(
            matches,
            vec![
                found(RowRef::Header, 0, 0..1),
                found(RowRef::Data(DataRow(0)), 0, 0..1),
                found(RowRef::Data(DataRow(0)), 0, 2..3),
                found(RowRef::Data(DataRow(1)), 1, 0..1),
            ]
        );
    }

    #[gpui::test]
    async fn test_case_and_whole_word_narrow_the_hits(cx: &mut TestAppContext) {
        let (pane, _editor, _search_bar, mut cx) =
            open_grid_with_search("a,b\nBob,bobby", cx).await;

        let insensitive = matches_for(&pane, text_query("bob", false, false), &mut cx).await;
        assert_eq!(
            insensitive,
            vec![
                found(RowRef::Data(DataRow(0)), 0, 0..3),
                found(RowRef::Data(DataRow(0)), 1, 0..3),
            ]
        );

        let sensitive = matches_for(&pane, text_query("bob", false, true), &mut cx).await;
        assert_eq!(sensitive, vec![found(RowRef::Data(DataRow(0)), 1, 0..3)]);

        let whole_word = matches_for(&pane, text_query("bob", true, false), &mut cx).await;
        assert_eq!(whole_word, vec![found(RowRef::Data(DataRow(0)), 0, 0..3)]);
    }

    #[gpui::test]
    async fn test_a_regular_expression_matches_inside_a_cell(cx: &mut TestAppContext) {
        let (pane, _editor, _search_bar, mut cx) = open_grid_with_search("a,b\nx1,y22", cx).await;

        let matches = matches_for(&pane, regex_query(r"\d+"), &mut cx).await;
        assert_eq!(
            matches,
            vec![
                found(RowRef::Data(DataRow(0)), 0, 1..2),
                found(RowRef::Data(DataRow(0)), 1, 1..3),
            ]
        );
    }

    #[gpui::test]
    async fn test_a_filtered_out_row_is_not_searched(cx: &mut TestAppContext) {
        let (pane, _editor, _search_bar, mut cx) =
            open_grid_with_search("k,v\na,1\nb,2\na,3", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.toggle_filter(AnyColumn(0), Some("a".into()), cx);
        });
        cx.run_until_parked();

        assert!(
            matches_for(&pane, text_query("2", false, false), &mut cx)
                .await
                .is_empty(),
            "a row the filter hides holds no hits"
        );
        assert_eq!(
            matches_for(&pane, text_query("3", false, false), &mut cx).await,
            vec![found(RowRef::Data(DataRow(2)), 1, 0..1)]
        );
    }

    #[gpui::test]
    async fn test_the_search_bar_walks_the_caret_through_the_hits(cx: &mut TestAppContext) {
        let (pane, _editor, search_bar, mut cx) =
            open_grid_with_search("k,v\nalpha,1\nbeta,2\nalpha,3", cx).await;

        deploy_search(&mut cx, Deploy::find());
        search_for(&search_bar, "alpha", BarOptions::NONE, &mut cx);

        assert_eq!(
            highlighted_cells(&pane, &cx),
            vec![
                (RowRef::Data(DataRow(0)), AnyColumn(0)),
                (RowRef::Data(DataRow(2)), AnyColumn(0)),
            ]
        );
        assert_eq!(
            cursor_cell(&pane, &cx),
            GridCell::new(GridRow::Body(DisplayRow(0)), AnyColumn(0))
        );

        cx.dispatch_action(SelectNextMatch);
        cx.run_until_parked();
        assert_eq!(
            cursor_cell(&pane, &cx),
            GridCell::new(GridRow::Body(DisplayRow(2)), AnyColumn(0))
        );
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.grid.search.active_cell,
                Some((RowRef::Data(DataRow(2)), AnyColumn(0)))
            );
        });

        cx.dispatch_action(SelectPreviousMatch);
        cx.run_until_parked();
        assert_eq!(
            cursor_cell(&pane, &cx),
            GridCell::new(GridRow::Body(DisplayRow(0)), AnyColumn(0))
        );
    }

    /// The cell editor holds focus for input methods, so the whole `"Editor"`
    /// keymap is live over the grid; the binding the grid's own context adds
    /// has to reach the bar all the same, and an open cell has to be written
    /// out before the caret starts walking hits.
    #[gpui::test]
    async fn test_the_grid_binding_opens_the_bar_and_commits_the_open_cell(
        cx: &mut TestAppContext,
    ) {
        let (pane, editor, search_bar, mut cx) = open_grid_with_search("k,v\nalpha,1", cx).await;
        cx.update(|_window, cx| {
            cx.bind_keys([gpui::KeyBinding::new(
                "ctrl-f",
                Deploy::find(),
                Some("TabularDataGrid"),
            )]);
        });

        pane.update(&mut cx, |pane, cx| {
            pane.select_cell(
                GridCell::new(GridRow::Body(DisplayRow(0)), AnyColumn(0)),
                false,
                cx,
            );
        });
        cx.simulate_input("z");
        cx.simulate_keystrokes("ctrl-f");
        cx.run_until_parked();

        assert!(search_bar.read_with(&cx, |search_bar, _| !search_bar.is_dismissed()));
        assert_eq!(buffer_text(&editor, &cx), "k,v\nz,1");
    }

    #[gpui::test]
    async fn test_the_case_toggle_narrows_what_is_highlighted(cx: &mut TestAppContext) {
        let (pane, _editor, search_bar, mut cx) = open_grid_with_search("k,v\nBob,bobby", cx).await;

        deploy_search(&mut cx, Deploy::find());
        search_for(&search_bar, "bob", BarOptions::NONE, &mut cx);
        assert_eq!(
            highlighted_cells(&pane, &cx),
            vec![
                (RowRef::Data(DataRow(0)), AnyColumn(0)),
                (RowRef::Data(DataRow(0)), AnyColumn(1)),
            ]
        );

        cx.dispatch_action(ToggleCaseSensitive);
        cx.run_until_parked();
        assert_eq!(
            highlighted_cells(&pane, &cx),
            vec![(RowRef::Data(DataRow(0)), AnyColumn(1))]
        );
    }

    #[gpui::test]
    async fn test_a_hit_in_the_header_row_is_reachable(cx: &mut TestAppContext) {
        let (pane, _editor, search_bar, mut cx) =
            open_grid_with_search("k,alpha\nbeta,1", cx).await;

        deploy_search(&mut cx, Deploy::find());
        search_for(&search_bar, "alpha", BarOptions::NONE, &mut cx);

        assert_eq!(
            highlighted_cells(&pane, &cx),
            vec![(RowRef::Header, AnyColumn(1))]
        );
        assert_eq!(
            cursor_cell(&pane, &cx),
            GridCell::new(GridRow::Header, AnyColumn(1))
        );
    }

    #[gpui::test]
    async fn test_a_sorted_table_is_walked_in_the_order_it_is_shown(cx: &mut TestAppContext) {
        let (pane, _editor, search_bar, mut cx) =
            open_grid_with_search("k,v\na,1\nb,2\nc,3", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.engine.applied_sorting = Some(AppliedSorting {
                col_idx: AnyColumn(0),
                direction: SortDirection::Desc,
            });
            pane.apply_sort(cx);
        });
        cx.run_until_parked();

        // Sorted descending the rows read c, b, a, so the first hit for a digit
        // is the `3` of row `c` and the caret lands on the topmost display row.
        let matches = matches_for(&pane, regex_query(r"\d"), &mut cx).await;
        assert_eq!(
            matches,
            vec![
                found(RowRef::Data(DataRow(2)), 1, 0..1),
                found(RowRef::Data(DataRow(1)), 1, 0..1),
                found(RowRef::Data(DataRow(0)), 1, 0..1),
            ]
        );

        deploy_search(&mut cx, Deploy::find());
        search_for(&search_bar, "1", BarOptions::NONE, &mut cx);
        assert_eq!(
            cursor_cell(&pane, &cx),
            GridCell::new(GridRow::Body(DisplayRow(2)), AnyColumn(1)),
            "the hit in data row 0 is drawn last once the table is sorted"
        );
    }

    /// Before the hits were invalidated by the reparse rather than by the raw
    /// buffer edit, sorting left the bar holding the file order while the grid
    /// showed another, and "next" walked back up the screen.
    #[gpui::test]
    async fn test_next_keeps_going_down_the_screen_after_a_sort(cx: &mut TestAppContext) {
        let (pane, _editor, search_bar, mut cx) =
            open_grid_with_search("k,v\nx1,-\nx2,-\nx3,-", cx).await;

        deploy_search(&mut cx, Deploy::find());
        search_for(&search_bar, "x", BarOptions::NONE, &mut cx);
        cx.dispatch_action(SelectNextMatch);
        cx.dispatch_action(SelectNextMatch);
        cx.run_until_parked();
        assert_eq!(
            cursor_cell(&pane, &cx),
            GridCell::new(GridRow::Body(DisplayRow(2)), AnyColumn(0))
        );

        pane.update(&mut cx, |pane, cx| {
            pane.engine.applied_sorting = Some(AppliedSorting {
                col_idx: AnyColumn(0),
                direction: SortDirection::Desc,
            });
            pane.apply_sort(cx);
        });
        cx.run_until_parked();

        // The bottom display row now holds `x1`, the last hit in display order,
        // so the next one wraps to the top instead of jumping to a row the file
        // order happened to put next.
        cx.dispatch_action(SelectNextMatch);
        cx.run_until_parked();
        assert_eq!(
            cursor_cell(&pane, &cx),
            GridCell::new(GridRow::Body(DisplayRow(0)), AnyColumn(0))
        );
    }

    #[gpui::test]
    async fn test_moving_the_caret_moves_the_active_hit(cx: &mut TestAppContext) {
        let (pane, _editor, search_bar, mut cx) =
            open_grid_with_search("k,v\na,-\n-,-\na,-", cx).await;

        deploy_search(&mut cx, Deploy::find());
        search_for(&search_bar, "a", BarOptions::NONE, &mut cx);
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.grid.search.active_cell,
                Some((RowRef::Data(DataRow(0)), AnyColumn(0)))
            );
        });

        pane.update(&mut cx, |pane, cx| {
            pane.select_cell(
                GridCell::new(GridRow::Body(DisplayRow(1)), AnyColumn(0)),
                false,
                cx,
            );
        });
        cx.run_until_parked();

        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.grid.search.active_cell,
                Some((RowRef::Data(DataRow(2)), AnyColumn(0))),
                "the counter should follow the caret to the next hit below it"
            );
        });
    }

    #[gpui::test]
    async fn test_replacing_after_the_header_row_was_turned_off(cx: &mut TestAppContext) {
        let (pane, editor, search_bar, mut cx) = open_grid_with_search("foo,v\nfoo,1", cx).await;

        deploy_search(&mut cx, Deploy::replace());
        search_for(&search_bar, "foo", BarOptions::NONE, &mut cx);
        search_bar.update(&mut cx, |search_bar, cx| {
            search_bar.set_replacement(Some("bar"), cx);
        });

        // Line 1 stops being the header, so every hit that was addressed
        // against the header row has to be found again as a data row.
        pane.update(&mut cx, |pane, cx| pane.toggle_header_row(cx));
        cx.run_until_parked();

        search_bar.update_in(&mut cx, |search_bar, window, cx| {
            search_bar.replace_all(&ReplaceAll, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &cx), "bar,v\nbar,1");
    }

    #[gpui::test]
    async fn test_replacing_everything_leaves_the_filtered_out_rows_alone(cx: &mut TestAppContext) {
        let (pane, editor, search_bar, mut cx) =
            open_grid_with_search("k,v\na,x\nb,x\na,x", cx).await;

        deploy_search(&mut cx, Deploy::replace());
        search_for(&search_bar, "x", BarOptions::NONE, &mut cx);
        search_bar.update(&mut cx, |search_bar, cx| {
            search_bar.set_replacement(Some("y"), cx);
        });
        assert_eq!(highlighted_cells(&pane, &cx).len(), 3);

        pane.update(&mut cx, |pane, cx| {
            pane.toggle_filter(AnyColumn(0), Some("a".into()), cx);
        });
        cx.run_until_parked();
        assert_eq!(
            highlighted_cells(&pane, &cx).len(),
            2,
            "changing the filter should have sent the bar looking again"
        );

        search_bar.update_in(&mut cx, |search_bar, window, cx| {
            search_bar.replace_all(&ReplaceAll, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &cx), "k,v\na,y\nb,x\na,y");
    }

    #[gpui::test]
    async fn test_an_edit_from_the_text_tab_moves_the_hits_before_a_replacement(
        cx: &mut TestAppContext,
    ) {
        let (_pane, editor, search_bar, mut cx) = open_grid_with_search("k,v\nfoo,1", cx).await;

        deploy_search(&mut cx, Deploy::replace());
        search_for(&search_bar, "foo", BarOptions::NONE, &mut cx);
        search_bar.update(&mut cx, |search_bar, cx| {
            search_bar.set_replacement(Some("bar"), cx);
        });

        // The same file, edited in its text tab: the hit that was at byte 0 of
        // the cell is now at byte 2.
        let buffer = editor
            .read_with(&cx, |editor, cx| editor.buffer().read(cx).as_singleton())
            .expect("the grid's editor is backed by one buffer");
        buffer.update(&mut cx, |buffer, cx| {
            buffer.edit([(4..4, "XY")], None, cx);
        });
        cx.executor()
            .advance_clock(crate::parser::REPARSE_DEBOUNCE * 2);
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &cx), "k,v\nXYfoo,1");

        cx.dispatch_action(ReplaceNext);
        cx.run_until_parked();
        assert_eq!(
            buffer_text(&editor, &cx),
            "k,v\nXYbar,1",
            "the replacement must land on the text the query matches, not on the old offset"
        );
    }

    /// The guard behind the invalidation: a range that no longer matches is
    /// dropped rather than written at, however it went stale.
    #[gpui::test]
    async fn test_a_hit_that_no_longer_matches_is_not_replaced(cx: &mut TestAppContext) {
        let (pane, editor, _search_bar, mut cx) = open_grid_with_search("k,v\nXYfoo,1", cx).await;

        let query = text_query("foo", false, false)
            .as_ref()
            .clone()
            .with_replacement("bar".to_owned());
        let stale = found(RowRef::Data(DataRow(0)), 0, 0..3);
        pane.update_in(&mut cx, |pane, window, cx| {
            SearchableItem::replace(pane, &stale, &query, SearchToken::default(), window, cx);
        });
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &cx), "k,v\nXYfoo,1");
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.grid.status_message.as_deref(),
                Some("1 件は内容が変わっていたため置換しませんでした")
            );
        });
    }

    #[gpui::test]
    async fn test_two_hits_in_one_cell_are_replaced_one_after_the_other(cx: &mut TestAppContext) {
        let (_pane, editor, search_bar, mut cx) = open_grid_with_search("k,v\nfoofoo,1", cx).await;

        deploy_search(&mut cx, Deploy::replace());
        search_for(&search_bar, "foo", BarOptions::NONE, &mut cx);
        search_bar.update(&mut cx, |search_bar, cx| {
            search_bar.set_replacement(Some("bar"), cx);
        });

        cx.dispatch_action(ReplaceNext);
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &cx), "k,v\nbarfoo,1");

        cx.dispatch_action(ReplaceNext);
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &cx), "k,v\nbarbar,1");
    }

    #[gpui::test]
    async fn test_quoted_multiline_and_japanese_cells(cx: &mut TestAppContext) {
        let (pane, editor, search_bar, mut cx) =
            open_grid_with_search("名前,メモ\n田中,\"1 行目\n2 行目\"", cx).await;
        pane.update(&mut cx, |pane, _| {
            pane.settings.multiline_cells_enabled = true;
        });

        // A quoted cell is searched by its value, so the hit's byte range is
        // into the text the user sees and not into the quoting around it.
        assert_eq!(
            matches_for(&pane, text_query("2 行目", false, false), &mut cx).await,
            vec![found(
                RowRef::Data(DataRow(0)),
                1,
                "1 行目\n".len().."1 行目\n2 行目".len()
            )]
        );

        deploy_search(&mut cx, Deploy::replace());
        search_for(&search_bar, "田中", BarOptions::NONE, &mut cx);
        search_bar.update(&mut cx, |search_bar, cx| {
            search_bar.set_replacement(Some("佐藤"), cx);
        });
        search_bar.update_in(&mut cx, |search_bar, window, cx| {
            search_bar.replace_all(&ReplaceAll, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            buffer_text(&editor, &cx),
            "名前,メモ\n佐藤,\"1 行目\n2 行目\""
        );
    }

    #[gpui::test]
    async fn test_a_replacement_holding_the_delimiter_is_quoted(cx: &mut TestAppContext) {
        let (_pane, editor, search_bar, mut cx) = open_grid_with_search("k,v\nab,1", cx).await;

        deploy_search(&mut cx, Deploy::replace());
        search_for(&search_bar, "b", BarOptions::NONE, &mut cx);
        search_bar.update(&mut cx, |search_bar, cx| {
            search_bar.set_replacement(Some("x,y"), cx);
        });
        search_bar.update_in(&mut cx, |search_bar, window, cx| {
            search_bar.replace_all(&ReplaceAll, window, cx);
        });
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &cx), "k,v\n\"ax,y\",1");
    }

    #[gpui::test]
    async fn test_a_regular_expression_replacement_uses_its_captures(cx: &mut TestAppContext) {
        let (_pane, editor, search_bar, mut cx) = open_grid_with_search("k,v\nab,1", cx).await;

        deploy_search(&mut cx, Deploy::replace());
        search_for(&search_bar, "(a)(b)", BarOptions::REGEX, &mut cx);
        search_bar.update(&mut cx, |search_bar, cx| {
            search_bar.set_replacement(Some("$2$1"), cx);
        });
        search_bar.update_in(&mut cx, |search_bar, window, cx| {
            search_bar.replace_all(&ReplaceAll, window, cx);
        });
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &cx), "k,v\nba,1");
    }

    /// A search range is one rectangle, so two blocks are pinned by the
    /// rectangle around both - the cells between them included, which is what
    /// the grid tells the user by drawing that rectangle's border.
    #[gpui::test]
    async fn test_two_blocks_pin_the_rectangle_around_them(cx: &mut TestAppContext) {
        let (pane, _editor, search_bar, mut cx) =
            open_grid_with_search("k,v,w\na,b,a\nc,a,d\na,b,a", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.change_cell_selection(
                GridCell::new(GridRow::Body(DisplayRow(0)), AnyColumn(0)),
                crate::grid::SelectionChange::Replace,
                cx,
            );
            pane.change_cell_selection(
                GridCell::new(GridRow::Body(DisplayRow(2)), AnyColumn(0)),
                crate::grid::SelectionChange::Add,
                cx,
            );
            pane.change_cell_selection(
                GridCell::new(GridRow::Body(DisplayRow(2)), AnyColumn(2)),
                crate::grid::SelectionChange::Extend,
                cx,
            );
        });
        deploy_search(
            &mut cx,
            Deploy {
                focus: true,
                replace_enabled: false,
                selection_search_enabled: true,
            },
        );
        search_for(&search_bar, "a", BarOptions::NONE, &mut cx);

        // Every `a` of the three body rows. The last block alone would be the
        // bottom row, and would leave three of them out.
        assert_eq!(highlighted_cells(&pane, &cx).len(), 5);
    }

    #[gpui::test]
    async fn test_the_pinned_range_comes_back_when_the_toggle_does(cx: &mut TestAppContext) {
        let (pane, _editor, search_bar, mut cx) = open_grid_with_search("k,v\na,a\na,a", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.select_cell(
                GridCell::new(GridRow::Body(DisplayRow(0)), AnyColumn(0)),
                false,
                cx,
            );
            pane.select_cell(
                GridCell::new(GridRow::Body(DisplayRow(1)), AnyColumn(0)),
                true,
                cx,
            );
        });
        deploy_search(
            &mut cx,
            Deploy {
                focus: true,
                replace_enabled: false,
                selection_search_enabled: true,
            },
        );
        search_for(&search_bar, "a", BarOptions::NONE, &mut cx);
        assert_eq!(highlighted_cells(&pane, &cx).len(), 2);

        // Activating a hit collapsed the selection to one cell, so turning the
        // toggle off and on again has nothing left to pin but the rectangle it
        // pinned before.
        search_bar.update_in(&mut cx, |search_bar, window, cx| {
            search_bar.set_search_within_selection(None, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(highlighted_cells(&pane, &cx).len(), 4);

        search_bar.update_in(&mut cx, |search_bar, window, cx| {
            search_bar.set_search_within_selection(Some(FilteredSearchRange::Default), window, cx);
        });
        cx.run_until_parked();
        assert_eq!(highlighted_cells(&pane, &cx).len(), 2);
    }

    #[gpui::test]
    async fn test_replacing_the_active_hit_rewrites_one_cell(cx: &mut TestAppContext) {
        let (_pane, editor, search_bar, mut cx) =
            open_grid_with_search("k,v\nfoo,1\nfoo,2", cx).await;

        deploy_search(&mut cx, Deploy::replace());
        search_for(&search_bar, "foo", BarOptions::NONE, &mut cx);
        search_bar.update(&mut cx, |search_bar, cx| {
            search_bar.set_replacement(Some("bar"), cx);
        });

        cx.dispatch_action(ReplaceNext);
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &cx), "k,v\nbar,1\nfoo,2");
    }

    #[gpui::test]
    async fn test_replacing_everything_is_a_single_undo_step(cx: &mut TestAppContext) {
        let (pane, editor, search_bar, mut cx) =
            open_grid_with_search("k,v\nfoofoo,foo\nx,foo", cx).await;

        deploy_search(&mut cx, Deploy::replace());
        search_for(&search_bar, "foo", BarOptions::NONE, &mut cx);
        search_bar.update(&mut cx, |search_bar, cx| {
            search_bar.set_replacement(Some("bar"), cx);
        });

        search_bar.update_in(&mut cx, |search_bar, window, cx| {
            search_bar.replace_all(&ReplaceAll, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &cx), "k,v\nbarbar,bar\nx,bar");
        assert!(
            highlighted_cells(&pane, &cx).is_empty(),
            "the bar should have searched again and found nothing left"
        );

        // Undo belongs to the grid, so the caret has to be back in it first.
        cx.dispatch_action(Dismiss);
        cx.run_until_parked();
        cx.dispatch_action(editor::actions::Undo);
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &cx), "k,v\nfoofoo,foo\nx,foo");
    }

    #[gpui::test]
    async fn test_a_pinned_selection_narrows_the_search(cx: &mut TestAppContext) {
        let (pane, _editor, search_bar, mut cx) = open_grid_with_search("k,v\na,a\na,a", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.select_cell(
                GridCell::new(GridRow::Body(DisplayRow(0)), AnyColumn(0)),
                false,
                cx,
            );
            pane.select_cell(
                GridCell::new(GridRow::Body(DisplayRow(1)), AnyColumn(0)),
                true,
                cx,
            );
        });

        deploy_search(
            &mut cx,
            Deploy {
                focus: true,
                replace_enabled: false,
                selection_search_enabled: true,
            },
        );
        search_for(&search_bar, "a", BarOptions::NONE, &mut cx);

        assert_eq!(
            highlighted_cells(&pane, &cx),
            vec![
                (RowRef::Data(DataRow(0)), AnyColumn(0)),
                (RowRef::Data(DataRow(1)), AnyColumn(0)),
            ],
            "only the pinned column should be searched"
        );
    }

    #[gpui::test]
    async fn test_selecting_every_hit_covers_the_rectangle_they_fill(cx: &mut TestAppContext) {
        let (pane, _editor, search_bar, mut cx) = open_grid_with_search("k,v\na,a\na,a", cx).await;

        deploy_search(&mut cx, Deploy::find());
        search_for(&search_bar, "a", BarOptions::NONE, &mut cx);
        cx.dispatch_action(SelectAllMatches);
        cx.run_until_parked();

        let rect = pane.read_with(&cx, |pane, _| pane.selection_rect());
        assert_eq!(
            (rect.first_row, rect.last_row, rect.first_col, rect.last_col),
            (1, 2, 0, 1)
        );
    }

    #[gpui::test]
    async fn test_dismissing_the_bar_clears_the_highlight_and_returns_focus(
        cx: &mut TestAppContext,
    ) {
        let (pane, _editor, search_bar, mut cx) = open_grid_with_search("k,v\nalpha,1", cx).await;

        deploy_search(&mut cx, Deploy::find());
        search_for(&search_bar, "alpha", BarOptions::NONE, &mut cx);
        assert!(!highlighted_cells(&pane, &cx).is_empty());

        cx.dispatch_action(Dismiss);
        cx.run_until_parked();

        assert!(highlighted_cells(&pane, &cx).is_empty());
        pane.read_with(&cx, |pane, _| {
            assert!(pane.grid.search.filtered_range.is_none());
        });
        let focused = cx.update(|window, cx| {
            gpui::Focusable::focus_handle(pane.read(cx), cx).is_focused(window)
        });
        assert!(
            focused,
            "closing the bar should put the caret back in the grid"
        );
    }

    /// The caret walking onto a hit far down a large file has to take the view
    /// with it, and the list only knows the height of the rows it has drawn.
    #[gpui::test]
    async fn test_a_hit_far_down_the_file_is_scrolled_to(cx: &mut TestAppContext) {
        const HIT_ROW: usize = 4_000;
        let mut contents = String::from("a,b\n");
        for row in 0..5_000 {
            let value = if row == HIT_ROW { "needle" } else { "hay" };
            contents.push_str(&format!("{row},{value}\n"));
        }
        let (pane, _editor, search_bar, mut cx) = open_grid_with_search(&contents, cx).await;
        // A width the list has not measured its rows at is what drops the height
        // hints it would otherwise scroll by.
        cx.simulate_resize(gpui::size(px(1600.), px(1053.)));
        cx.run_until_parked();

        deploy_search(&mut cx, Deploy::find());
        search_for(&search_bar, "needle", BarOptions::NONE, &mut cx);

        assert_eq!(
            cursor_cell(&pane, &cx),
            GridCell::new(GridRow::Body(DisplayRow(HIT_ROW)), AnyColumn(1))
        );
        pane.read_with(&cx, |pane, _| {
            assert!(
                crate::grid_editing::tests::body_row_is_visible(pane, HIT_ROW),
                "the hit's row is off screen; the view sits at {:?}",
                pane.list_state.logical_scroll_top()
            );
        });
    }

    #[test]
    fn test_the_grid_keymap_deploys_the_search_bar() {
        let keymap = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../assets/keymaps/default-windows.json"
        ));
        let block = keymap
            .split(r#""context": "TabularDataGrid""#)
            .nth(1)
            .expect("the default keymap should carry a TabularDataGrid block");
        for binding in [
            r#""ctrl-f": "buffer_search::Deploy""#,
            r#""ctrl-h": "buffer_search::DeployReplace""#,
        ] {
            assert!(
                block.contains(binding),
                "the TabularDataGrid block should bind {binding}; \
                 the editor's own binding is scoped to `mode == full`, \
                 which the grid's single line cell editor never is"
            );
        }
    }
}
