use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    BackgroundExecutor, ElementId, Focusable, ForegroundExecutor, MouseButton, MouseDownEvent,
    MouseMoveEvent, Task, WeakEntity, actions, transparent_black,
};
use picker::{Picker, PickerDelegate};
use ui::{
    Color, GradientFade, HighlightedLabel, Icon, IconButton, IconName, IconSize, Label, LabelSize,
    ListItem, ListItemSpacing, PopoverMenu, Tooltip, prelude::*, utils::replace_control_characters,
};

use crate::{
    TabularDataPreviewPane,
    grid::{CellEditMode, GridCell, GridRow, SelectionChange, column_label},
    renderer::table_cell::CELL_FRAME_DEBUG_SELECTOR,
    settings::FilterSortOrder,
    table_data_engine::{
        filtering_by_column::{
            FilterEntry, FilterEntryState, FilterJoin, TextCondition, TextFilterOp,
        },
        sorting_by_column::{AppliedSorting, SortDirection},
    },
    types::AnyColumn,
};

actions!(
    tabular_data,
    [
        /// 表のすべての列の絞り込みを解除します。
        ClearAllFilters,
        /// 選択中のセルの列の絞り込みメニューを開きます。
        ToggleFilterMenu,
    ]
);

/// Stands in for a cell with no value at all in the filter list.
const NULL_VALUE_LABEL: &str = "(空白)";

/// Marks a heading's filter button, which - unlike the sort button beside it -
/// has to be drawn whether or not the pointer is over the column. Only the first
/// column carries it, so that a test reads one known button rather than whichever
/// column was painted last.
pub(crate) const FILTER_BUTTON_DEBUG_SELECTOR: &str = "tabular-grid-filter-button";

/// The sort button's counterpart of [`FILTER_BUTTON_DEBUG_SELECTOR`]. It is only
/// recorded while the button is actually painted, which is what tells the two
/// visibility rules apart.
pub(crate) const SORT_BUTTON_DEBUG_SELECTOR: &str = "tabular-grid-sort-button";

struct ColumnFilterRow {
    entry: FilterEntry,
    is_applied: bool,
    /// Set when this value is hidden because another column's active filter
    /// excludes every row containing it.
    hidden_by: Option<AnyColumn>,
}

enum ColumnFilterListEntry {
    Header(SharedString),
    Row {
        row_index: usize,
        positions: Vec<usize>,
    },
}

pub(crate) struct ColumnFilterDelegate {
    col: AnyColumn,
    /// Weak, because the pane holds a [`ui::PopoverMenuHandle`] that keeps the
    /// deployed picker alive: a strong handle back would close the loop and the
    /// grid would outlive its own tab.
    view: WeakEntity<TabularDataPreviewPane>,
    /// Row order frozen at open time (available entries sorted per the
    /// column's `FilterSortOrder`, then entries hidden by other columns'
    /// filters). Kept stable so toggling a value doesn't reshuffle the list
    /// under the user's cursor; only `is_applied`/`hidden_by`/counts are
    /// refreshed in place after a toggle.
    rows: Vec<ColumnFilterRow>,
    /// Number of available (non-hidden) rows, shown in the search placeholder.
    available_count: usize,
    string_candidates: Arc<Vec<StringMatchCandidate>>,
    filtered: Vec<ColumnFilterListEntry>,
    selected_index: usize,
    query: String,
    /// Set while the seven condition buttons apply to the *second* of the
    /// column's two text conditions, joined this way. `None` while they replace
    /// whatever the column is filtered by, which is what they have always done.
    pending_join: Option<FilterJoin>,
    foreground: ForegroundExecutor,
    background: BackgroundExecutor,
    cancel: Option<Arc<AtomicBool>>,
}

impl ColumnFilterDelegate {
    fn new(
        col: AnyColumn,
        view: WeakEntity<TabularDataPreviewPane>,
        sort_order: FilterSortOrder,
        column_filters: Arc<Vec<(FilterEntry, FilterEntryState)>>,
        cx: &mut Context<Picker<Self>>,
    ) -> Self {
        let mut available: Vec<(FilterEntry, bool)> = Vec::new();
        let mut hidden: Vec<(FilterEntry, AnyColumn, bool)> = Vec::new();
        for (entry, state) in column_filters.iter() {
            match state {
                FilterEntryState::Available { is_applied } => {
                    available.push((entry.clone(), *is_applied))
                }
                FilterEntryState::Unavailable {
                    blocked_by,
                    is_applied,
                } => hidden.push((entry.clone(), *blocked_by, *is_applied)),
            }
        }

        match sort_order {
            FilterSortOrder::AlphaThenCount => available.sort_by(|(a, a_app), (b, b_app)| {
                b_app
                    .cmp(a_app)
                    .then_with(|| a.content.cmp(&b.content))
                    .then_with(|| b.occurred_times().cmp(&a.occurred_times()))
            }),
            FilterSortOrder::CountThenAlpha => available.sort_by(|(a, a_app), (b, b_app)| {
                b_app
                    .cmp(a_app)
                    .then_with(|| b.occurred_times().cmp(&a.occurred_times()))
                    .then_with(|| a.content.cmp(&b.content))
            }),
        }

        let available_count = available.len();

        let rows: Vec<ColumnFilterRow> = available
            .into_iter()
            .map(|(entry, is_applied)| ColumnFilterRow {
                entry,
                is_applied,
                hidden_by: None,
            })
            .chain(
                hidden
                    .into_iter()
                    .map(|(entry, blocked_by, is_applied)| ColumnFilterRow {
                        entry,
                        is_applied,
                        hidden_by: Some(blocked_by),
                    }),
            )
            .collect();

        let string_candidates = Arc::new(
            rows.iter()
                .enumerate()
                .map(|(index, row)| {
                    StringMatchCandidate::new(index, &Self::display_text(&row.entry))
                })
                .collect::<Vec<_>>(),
        );

        let filtered = Self::build_entries(
            &rows,
            rows.iter()
                .enumerate()
                .map(|(index, _)| (index, Vec::new()))
                .collect(),
        );

        // A column that already carries two conditions opens with the buttons
        // pointed at the second one, so that the pair the user built is what
        // they go on editing.
        let pending_join = view
            .read_with(cx, |view, _| {
                view.engine.text_filter(col).and_then(TextCondition::join)
            })
            .ok()
            .flatten();

        Self {
            col,
            view,
            rows,
            available_count,
            string_candidates,
            filtered,
            selected_index: 0,
            query: String::new(),
            pending_join,
            foreground: cx.foreground_executor().clone(),
            background: cx.background_executor().clone(),
            cancel: None,
        }
    }

    fn display_text(entry: &FilterEntry) -> String {
        match &entry.content {
            Some(s) => s.as_ref().to_owned(),
            None => NULL_VALUE_LABEL.to_owned(),
        }
    }

    /// Assembles the flat list shown to the user from matched
    /// `(row_index, positions)` pairs (which must be sorted ascending by
    /// `row_index`, matching `rows`' available-then-hidden order), inserting a
    /// "hidden by other filters" header before the first hidden row.
    fn build_entries(
        rows: &[ColumnFilterRow],
        matches: Vec<(usize, Vec<usize>)>,
    ) -> Vec<ColumnFilterListEntry> {
        let mut entries = Vec::with_capacity(matches.len() + 1);
        let mut header_inserted = false;
        for (row_index, positions) in matches {
            if rows[row_index].hidden_by.is_some() && !header_inserted {
                entries.push(ColumnFilterListEntry::Header("他の絞り込みで非表示".into()));
                header_inserted = true;
            }
            entries.push(ColumnFilterListEntry::Row {
                row_index,
                positions,
            });
        }
        entries
    }

    fn first_selectable_index(&self) -> usize {
        self.filtered
            .iter()
            .position(|entry| {
                matches!(entry, ColumnFilterListEntry::Row { row_index, .. }
                    if self.rows[*row_index].hidden_by.is_none())
            })
            .unwrap_or(0)
    }

    fn matches_for_query(&self, query: &str) -> Vec<(usize, Vec<usize>)> {
        if query.is_empty() {
            return self
                .rows
                .iter()
                .enumerate()
                .map(|(index, _)| (index, Vec::new()))
                .collect();
        }

        let cancel_flag = AtomicBool::new(false);
        let mut matches: Vec<StringMatch> = self.foreground.block_on(match_strings(
            self.string_candidates.as_ref(),
            query,
            false,
            true,
            usize::MAX,
            &cancel_flag,
            self.background.clone(),
        ));
        matches.sort_by_key(|m| m.candidate_id);
        matches
            .into_iter()
            .map(|m| (m.candidate_id, m.positions))
            .collect()
    }

    /// Re-fetches this column's filter entries from the engine (e.g. after a
    /// toggle changes counts/availability across the cascade) while keeping
    /// `rows`' frozen order, then re-applies the current search query.
    fn refresh_rows(&mut self, cx: &mut Context<Picker<Self>>) {
        let column_filters = match self
            .view
            .read_with(cx, |view, _| view.engine.get_filters_for_column(self.col))
            .and_then(|filters| filters)
        {
            Ok(filters) => filters,
            Err(err) => {
                log::error!("Failed to get filters for column: {err}");
                return;
            }
        };

        let mut lookup: HashMap<Option<SharedString>, (FilterEntry, FilterEntryState)> =
            column_filters
                .iter()
                .map(|(entry, state)| (entry.content.clone(), (entry.clone(), *state)))
                .collect();

        for row in &mut self.rows {
            let Some((entry, state)) = lookup.remove(&row.entry.content) else {
                continue;
            };
            row.entry = entry;
            match state {
                FilterEntryState::Available { is_applied } => {
                    row.is_applied = is_applied;
                    row.hidden_by = None;
                }
                FilterEntryState::Unavailable {
                    blocked_by,
                    is_applied,
                } => {
                    row.is_applied = is_applied;
                    row.hidden_by = Some(blocked_by);
                }
            }
        }

        // Checking a value replaces the text conditions, and a join armed for a
        // second condition that has no first one left would make the seven
        // buttons do nothing at all.
        if self
            .view
            .read_with(cx, |view, _| view.engine.text_filter(self.col).is_none())
            .unwrap_or(true)
        {
            self.pending_join = None;
        }

        self.filtered = Self::build_entries(&self.rows, self.matches_for_query(&self.query));
        self.selected_index = self
            .selected_index
            .min(self.filtered.len().saturating_sub(1));
    }

    /// How many of this column's values the user can actually choose, which is
    /// what "all of them" means for [`Self::select_listed_values`].
    fn available_rows(&self) -> impl Iterator<Item = &ColumnFilterRow> {
        self.rows.iter().filter(|row| row.hidden_by.is_none())
    }

    /// Excel's "select everything the search narrowed the list down to": the
    /// values the list is showing become the column's filter. With no search in
    /// place that is every value, which is the same as no filter at all.
    fn select_listed_values(&mut self, cx: &mut Context<Picker<Self>>) {
        let listed = listed_value_filter(&self.rows, &self.filtered);
        let col = self.col;
        // The pane is only gone when the grid's tab closed under the open
        // popover, which the popover is about to follow.
        self.view
            .update(cx, |view, cx| match listed {
                Some(values) => view.set_value_filter(col, values, cx),
                None => view.clear_filters(col, cx),
            })
            .ok();
        self.refresh_rows(cx);
    }

    /// Applies a text condition built from what is typed in the search box, or
    /// drops it again when it is the condition already in force.
    ///
    /// Which of the column's two conditions that is depends on whether a join
    /// is armed; without one the condition replaces whatever the column was
    /// filtered by, as a single condition always has.
    fn toggle_text_filter(&mut self, op: TextFilterOp, cx: &mut Context<Picker<Self>>) {
        let col = self.col;
        let needle = SharedString::from(self.query.clone());

        let Some(join) = self.pending_join else {
            let already_applied = self
                .view
                .read_with(cx, |view, _| {
                    view.engine.text_filter(col).is_some_and(|condition| {
                        condition.rest.is_none()
                            && condition.first.op == op
                            && condition.first.needle == needle
                    })
                })
                .unwrap_or(false);

            self.view
                .update(cx, |view, cx| {
                    if already_applied {
                        view.clear_filters(col, cx);
                    } else {
                        view.set_text_filter(col, op, needle, cx);
                    }
                })
                .ok();
            self.refresh_rows(cx);
            return;
        };

        let already_applied = self
            .view
            .read_with(cx, |view, _| {
                view.engine
                    .text_filter(col)
                    .and_then(|condition| condition.rest.as_ref())
                    .is_some_and(|(_, second)| second.op == op && second.needle == needle)
            })
            .unwrap_or(false);

        self.view
            .update(cx, |view, cx| {
                if already_applied {
                    view.clear_second_text_filter(col, cx);
                } else {
                    view.set_second_text_filter(col, join, op, needle, cx);
                }
            })
            .ok();
        self.refresh_rows(cx);
    }

    /// The "かつ" / "または" toggle. While a second condition already stands
    /// this switches how the two are joined; while none does it only arms the
    /// next press of a condition button, and pressing it again disarms it.
    fn set_join(&mut self, join: FilterJoin, cx: &mut Context<Picker<Self>>) {
        let col = self.col;
        let has_second = self
            .view
            .read_with(cx, |view, _| {
                view.engine
                    .text_filter(col)
                    .is_some_and(|condition| condition.rest.is_some())
            })
            .unwrap_or(false);

        if !has_second {
            self.pending_join = (self.pending_join != Some(join)).then_some(join);
            return;
        }

        self.pending_join = Some(join);
        self.view
            .update(cx, |view, cx| view.set_filter_join(col, join, cx))
            .ok();
        self.refresh_rows(cx);
    }

    /// Takes the second condition away again, leaving the first in force.
    fn clear_second_condition(&mut self, cx: &mut Context<Picker<Self>>) {
        let col = self.col;
        self.view
            .update(cx, |view, cx| view.clear_second_text_filter(col, cx))
            .ok();
        self.pending_join = None;
        self.refresh_rows(cx);
    }

    /// One of the text condition buttons under the value list.
    fn render_text_filter_button(
        &self,
        op: TextFilterOp,
        active: Option<TextFilterOp>,
        armed_join: Option<FilterJoin>,
        cx: &mut Context<Picker<Self>>,
    ) -> Button {
        // A condition that reads the typed text has nothing to work with while
        // the search box is empty, so it is offered but not armed.
        let needs_query = op.needs_needle() && self.query.is_empty();
        let is_active = active == Some(op);
        let tooltip: SharedString = if needs_query {
            format!("検索欄に文字を入れてから「{}」を押します", op.label()).into()
        } else if is_active && armed_join.is_some() {
            "2 つ目の条件に使用中。クリックで解除".into()
        } else if is_active {
            "この条件で絞り込み中。クリックで解除".into()
        } else {
            let kept = if op.needs_needle() {
                format!("{}行", op.describe(&self.query))
            } else {
                format!("{}のセルの行", op.label())
            };
            match armed_join {
                Some(join) => format!(
                    "1 つ目の条件と「{}」でつないで、{}を表示します",
                    join.label(),
                    kept
                )
                .into(),
                None => format!("{kept}だけを表示します").into(),
            }
        };

        Button::new(ElementId::Name(op.label().into()), op.label())
            .size(ButtonSize::Compact)
            .label_size(LabelSize::Small)
            .style(if is_active {
                ButtonStyle::Filled
            } else {
                ButtonStyle::Subtle
            })
            .disabled(needs_query)
            .tooltip(Tooltip::text(tooltip))
            .on_click(cx.listener(move |picker, _, _window, cx| {
                picker.delegate.toggle_text_filter(op, cx);
                cx.notify();
            }))
    }

    fn render_join_button(
        &self,
        join: FilterJoin,
        armed_join: Option<FilterJoin>,
        has_second: bool,
        cx: &mut Context<Picker<Self>>,
    ) -> Button {
        let is_active = armed_join == Some(join);
        let tooltip: SharedString = if has_second {
            format!("2 つの条件を「{}」でつなぎます", join.label()).into()
        } else if is_active {
            format!(
                "次に押す条件が「{}」でつないだ 2 つ目になります。もう一度押すと取り消します",
                join.label()
            )
            .into()
        } else {
            format!(
                "次に押す条件を「{}」でつないだ 2 つ目にします",
                join.label()
            )
            .into()
        };

        Button::new(
            ElementId::Name(format!("table-filter-join-{}", join.label()).into()),
            join.label(),
        )
        .size(ButtonSize::Compact)
        .label_size(LabelSize::Small)
        .style(if is_active {
            ButtonStyle::Filled
        } else {
            ButtonStyle::Subtle
        })
        .tooltip(Tooltip::text(tooltip))
        .on_click(cx.listener(move |picker, _, _window, cx| {
            picker.delegate.set_join(join, cx);
            cx.notify();
        }))
    }
}

/// Which values "select everything the list is showing" narrows a column to, or
/// `None` when that is every value the column has: a filter that keeps every row
/// is not a filter, and leaving one in place would go on excluding rows that a
/// later edit gives a new value.
///
/// Entries hidden by another column's filter are left out, matching what the
/// list lets the user check by hand.
fn listed_value_filter(
    rows: &[ColumnFilterRow],
    filtered: &[ColumnFilterListEntry],
) -> Option<HashSet<Option<SharedString>>> {
    let listed: HashSet<Option<SharedString>> = filtered
        .iter()
        .filter_map(|entry| match entry {
            ColumnFilterListEntry::Row { row_index, .. } => rows.get(*row_index),
            ColumnFilterListEntry::Header(_) => None,
        })
        .filter(|row| row.hidden_by.is_none())
        .map(|row| row.entry.content.clone())
        .collect();
    let available = rows.iter().filter(|row| row.hidden_by.is_none()).count();
    (listed.len() != available).then_some(listed)
}

impl PickerDelegate for ColumnFilterDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "table column filter"
    }

    fn match_count(&self) -> usize {
        self.filtered.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix.min(self.filtered.len().saturating_sub(1));
        cx.notify();
    }

    /// A hidden (blocked-by-another-filter) row can still be selected if it's currently
    /// applied, so the user can uncheck it from the popover; a hidden-and-unapplied row
    /// can't be newly selected, since doing so would only add a filter guaranteed to
    /// leave zero rows.
    fn can_select(&self, ix: usize, _window: &mut Window, _cx: &mut Context<Picker<Self>>) -> bool {
        match self.filtered.get(ix) {
            Some(ColumnFilterListEntry::Row { row_index, .. }) => {
                let row = &self.rows[*row_index];
                row.hidden_by.is_none() || row.is_applied
            }
            _ => false,
        }
    }

    /// Names what the box does for both of its jobs: it narrows the value list,
    /// and it is the text the conditions in the footer are applied to.
    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        format!("{} 件の値を検索 / 条件の文字", self.available_count).into()
    }

    /// Text nothing in the column matches is still worth having typed, since the
    /// footer's conditions are applied to it.
    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some("一致する値はありません。下の条件では使えます".into())
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.query = query.clone();

        if query.is_empty() {
            self.filtered = Self::build_entries(&self.rows, self.matches_for_query(&query));
            self.selected_index = self.first_selectable_index();
            cx.notify();
            return Task::ready(());
        }

        if let Some(prev) = &self.cancel {
            prev.store(true, Ordering::Relaxed);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());

        let string_candidates = self.string_candidates.clone();
        let background = self.background.clone();

        cx.spawn_in(window, async move |this, cx| {
            let mut matches = match_strings(
                string_candidates.as_ref(),
                &query,
                false,
                true,
                usize::MAX,
                cancel.as_ref(),
                background,
            )
            .await;
            matches.sort_by_key(|m| m.candidate_id);

            this.update_in(cx, |this, _, cx| {
                if this.delegate.query != query {
                    return;
                }
                let rows = &this.delegate.rows;
                let matches = matches
                    .into_iter()
                    .map(|m| (m.candidate_id, m.positions))
                    .collect();
                this.delegate.filtered = ColumnFilterDelegate::build_entries(rows, matches);
                this.delegate.selected_index = this.delegate.first_selectable_index();
                cx.notify();
            })
            .ok();
        })
    }

    fn confirm(&mut self, _secondary: bool, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(ColumnFilterListEntry::Row { row_index, .. }) =
            self.filtered.get(self.selected_index)
        else {
            return;
        };
        let row_index = *row_index;
        if self.rows[row_index].hidden_by.is_some() && !self.rows[row_index].is_applied {
            return;
        }
        let col = self.col;
        let value = self.rows[row_index].entry.content.clone();
        self.view
            .update(cx, |view, cx| view.toggle_filter(col, value, cx))
            .ok();
        self.refresh_rows(cx);
        cx.notify();
    }

    fn dismissed(&mut self, _window: &mut Window, _cx: &mut Context<Picker<Self>>) {}

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        match self.filtered.get(ix)? {
            ColumnFilterListEntry::Header(label) => Some(
                div()
                    .px_2()
                    .pt_2()
                    .pb_1()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        Label::new(label.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .into_any_element(),
            ),
            ColumnFilterListEntry::Row {
                row_index,
                positions,
            } => {
                let row = &self.rows[*row_index];
                let value_text: SharedString = match &row.entry.content {
                    Some(s) => s.clone(),
                    None => NULL_VALUE_LABEL.into(),
                };
                let count_text = SharedString::from(row.entry.occurred_times().to_string());
                let label = HighlightedLabel::new(value_text.clone(), positions.clone())
                    .size(LabelSize::Small)
                    .single_line()
                    .truncate();

                // Hidden-and-unapplied rows are disabled (can't newly select a value that
                // would zero out the table); hidden-and-applied rows stay interactive so the
                // user can uncheck them, same as a normal available row, just muted to signal
                // it's currently blocked by another filter.
                let is_hidden = row.hidden_by.is_some();
                let color = if is_hidden {
                    Color::Disabled
                } else {
                    Color::Muted
                };

                Some(
                    ListItem::new(("table-filter-value", ix))
                        .disabled(is_hidden && !row.is_applied)
                        .inset(true)
                        .spacing(ListItemSpacing::Sparse)
                        .toggle_state(selected)
                        .start_slot(
                            h_flex()
                                .flex_none()
                                .when(!row.is_applied, |el| el.invisible())
                                .child(Icon::new(IconName::Check).size(IconSize::Small).color(
                                    if is_hidden {
                                        Color::Disabled
                                    } else {
                                        Color::Accent
                                    },
                                )),
                        )
                        .child(label.color(color))
                        .end_slot(Label::new(count_text).size(LabelSize::Small).color(color))
                        .tooltip(Tooltip::text(value_text))
                        .into_any_element(),
                )
            }
        }
    }

    /// What the column is filtered by, how to drop it, and the text conditions.
    ///
    /// All of it lives in the footer rather than the header because the picker
    /// only draws a header while the value list has matches, and a text
    /// condition is exactly what the user reaches for when it has none.
    fn render_footer(
        &self,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        let col = self.col;
        let (has_active_filters, active_text) = self
            .view
            .read_with(cx, |view, _| {
                (
                    view.engine.has_active_filters(col),
                    view.engine.text_filter(col).cloned(),
                )
            })
            .unwrap_or((false, None));

        let status: SharedString = match &active_text {
            Some(filter) => format!("条件: {}", filter.description()).into(),
            None if has_active_filters => {
                let selected_rows: usize = self
                    .available_rows()
                    .filter(|row| row.is_applied)
                    .map(|row| row.entry.occurred_times())
                    .sum();
                let total_rows: usize = self
                    .available_rows()
                    .map(|row| row.entry.occurred_times())
                    .sum();
                format!("{total_rows} 行中 {selected_rows} 行を選択中").into()
            }
            None => "値を選ぶか、下の条件で絞り込みます".into(),
        };
        // A join can only be armed while there is a first condition to hang the
        // second one off.
        let armed_join = active_text
            .as_ref()
            .and(self.pending_join.as_ref())
            .copied();
        let active_op = match (&active_text, armed_join) {
            (Some(condition), Some(_)) => condition.rest.as_ref().map(|(_, second)| second.op),
            (Some(condition), None) => Some(condition.first.op),
            (None, _) => None,
        };

        // Two rows of four and three: the popover is too narrow for all seven
        // labels side by side once the buffer font is enlarged.
        let (first_ops, second_ops) = TextFilterOp::ALL.split_at(4);

        // Excel's "かつ / または" from its custom filter dialog. Offered only
        // once a first condition stands, since it joins that one to a second.
        let join_row = active_text.as_ref().map(|condition| {
            let has_second = condition.rest.is_some();
            h_flex()
                .w_full()
                .gap_1()
                .items_center()
                .child(
                    Label::new("2 つ目の条件")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(self.render_join_button(FilterJoin::And, armed_join, has_second, cx))
                .child(self.render_join_button(FilterJoin::Or, armed_join, has_second, cx))
                .when(has_second, |this| {
                    this.child(
                        Button::new(
                            ElementId::Name("table-filter-clear-second".into()),
                            "2 つ目を解除",
                        )
                        .size(ButtonSize::Compact)
                        .label_size(LabelSize::Small)
                        .style(ButtonStyle::Subtle)
                        .tooltip(Tooltip::text(
                            "2 つ目の条件を外して、1 つ目だけで絞り込みます",
                        ))
                        .on_click(cx.listener(
                            move |picker, _, _window, cx| {
                                picker.delegate.clear_second_condition(cx);
                                cx.notify();
                            },
                        )),
                    )
                })
        });

        Some(
            v_flex()
                .w_full()
                .gap_1()
                .px_2()
                .py_1()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    h_flex()
                        .w_full()
                        .gap_1()
                        .justify_between()
                        .items_center()
                        .child(
                            Label::new(status)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .single_line()
                                .truncate(),
                        )
                        .child(
                            Button::new(ElementId::Name("table-filter-clear".into()), "すべて解除")
                                .size(ButtonSize::Compact)
                                .label_size(LabelSize::Small)
                                .style(ButtonStyle::Subtle)
                                .disabled(!has_active_filters)
                                .tooltip(Tooltip::text("この列の絞り込みを解除します"))
                                .on_click(cx.listener(move |picker, _, _window, cx| {
                                    picker
                                        .delegate
                                        .view
                                        .update(cx, |view, cx| view.clear_filters(col, cx))
                                        .ok();
                                    picker.delegate.refresh_rows(cx);
                                    cx.notify();
                                })),
                        ),
                )
                .child(
                    h_flex()
                        .w_full()
                        .gap_1()
                        .items_center()
                        .child(
                            Button::new(
                                ElementId::Name("table-filter-select-listed".into()),
                                "すべて選択",
                            )
                            .size(ButtonSize::Compact)
                            .label_size(LabelSize::Small)
                            .style(ButtonStyle::Subtle)
                            .tooltip(Tooltip::text(
                                "いま一覧に出ている値だけに絞り込みます (検索と組み合わせて使います)",
                            ))
                            .on_click(cx.listener(move |picker, _, _window, cx| {
                                picker.delegate.select_listed_values(cx);
                                cx.notify();
                            })),
                        )
                        .children(first_ops.iter().map(|op| {
                            self.render_text_filter_button(*op, active_op, armed_join, cx)
                        })),
                )
                .child(
                    h_flex().w_full().gap_1().items_center().children(
                        second_ops.iter().map(|op| {
                            self.render_text_filter_button(*op, active_op, armed_join, cx)
                        }),
                    ),
                )
                .children(join_row)
                .into_any(),
        )
    }
}

impl TabularDataPreviewPane {
    /// The column heading: its spreadsheet letter, the value of the header row
    /// when there is one, and the sort and filter buttons.
    pub(crate) fn create_header_element_with_sort_button(
        &self,
        cx: &mut Context<'_, TabularDataPreviewPane>,
        col_idx: AnyColumn,
    ) -> AnyElement {
        let has_active_sort = self
            .engine
            .applied_sorting
            .is_some_and(|o| o.col_idx == col_idx);
        let group_name = SharedString::from(format!("table-col-header-{}", col_idx.get()));

        let header_cell = GridCell::new(GridRow::Header, col_idx);
        let is_active_cell = self.grid.cursor == header_cell && self.grid.context_menu.is_none();
        let needs_probe = self.overlay_cell() == header_cell;
        let is_selected = self.selection_contains_col(col_idx);
        let search_range_border = self.search_range_border(header_cell, cx);
        let header_text = if self.settings.first_row_is_header {
            self.cell_value(GridRow::Header, col_idx)
        } else {
            SharedString::default()
        };
        // Column names come from the file being previewed, so they may hold
        // control characters. Show stand-ins for those, but keep the real name
        // for the cell's value, which is what editing and copying start from.
        let displayed_header = match replace_control_characters(&header_text) {
            Cow::Borrowed(_) => header_text.clone(),
            Cow::Owned(replaced) => SharedString::from(replaced),
        };

        let colors = cx.theme().colors();
        let paint = self.cell_paint(header_cell, is_selected, is_active_cell, cx);
        // The fade at the right edge hides the heading's text behind the sort
        // and filter buttons, so it has to be the same color the cell is.
        let base_bg = paint.background.unwrap_or(colors.editor_background);
        let grad_width_hovered = px(100.);
        // The filter button stands in the heading whether or not the pointer is
        // there, so the text has to fade out behind it at rest too; hovering
        // brings out the sort button beside it, which needs more room.
        let grad_width = if has_active_sort {
            grad_width_hovered
        } else {
            px(44.)
        };
        let insert_handle = self.insert_column_handle(col_idx, group_name.clone(), cx);
        let heading = h_flex()
            .group(group_name.clone())
            .relative()
            .overflow_hidden()
            .w_full()
            .items_center()
            .font_buffer(cx)
            .text_buffer(cx)
            .border_2()
            .border_color(if is_active_cell {
                colors.border_focused
            } else {
                search_range_border.unwrap_or_else(transparent_black)
            })
            .when(needs_probe, |this| {
                this.debug_selector(|| CELL_FRAME_DEBUG_SELECTOR.to_owned())
            })
            .when_some(paint.background, |this, background| this.bg(background))
            .when_some(paint.text, |this, color| this.text_color(color))
            .child(
                div()
                    .id(ElementId::NamedInteger(
                        "table-col-header-text".into(),
                        col_idx.get() as u64,
                    ))
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .flex()
                    .items_center()
                    .gap_1()
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            if event.click_count > 1 && this.has_header_row() {
                                this.select_cell(header_cell, false, cx);
                                this.start_editing(CellEditMode::Edit, window, cx);
                                return;
                            }
                            this.on_column_mouse_down(
                                col_idx,
                                MouseButton::Left,
                                event.modifiers,
                                event.position,
                                window,
                                cx,
                            );
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            this.on_column_mouse_down(
                                col_idx,
                                MouseButton::Right,
                                event.modifiers,
                                event.position,
                                window,
                                cx,
                            );
                        }),
                    )
                    .on_mouse_move(
                        cx.listener(move |this, event: &MouseMoveEvent, _window, cx| {
                            if event.pressed_button == Some(MouseButton::Left) {
                                this.on_column_drag(col_idx, cx);
                            }
                        }),
                    )
                    .child(
                        Label::new(column_label(col_idx.get()))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(displayed_header),
            )
            .child(
                GradientFade::new(base_bg, base_bg, base_bg)
                    .width(grad_width)
                    .width_hovered(grad_width_hovered)
                    .right(px(0.))
                    .gradient_stop(0.8)
                    .group_name(group_name.clone()),
            )
            .child(
                h_flex()
                    .absolute()
                    .right_0()
                    .top_0()
                    .h_full()
                    .items_center()
                    .gap_1()
                    // Sorting stays a hover affordance, but the filter arrow is
                    // Excel's autofilter: hidden until hovered, nobody finds it.
                    // The debug selector hangs off a child of the hover gate, so
                    // that it is only recorded while the button is really drawn.
                    .child(
                        div()
                            .when(!has_active_sort, |this| {
                                this.visible_on_hover(group_name.clone())
                            })
                            .child(
                                div()
                                    .when(col_idx.get() == 0, |this| {
                                        this.debug_selector(|| {
                                            SORT_BUTTON_DEBUG_SELECTOR.to_owned()
                                        })
                                    })
                                    .child(self.create_sort_button(cx, col_idx)),
                            ),
                    )
                    .child(
                        div()
                            .when(col_idx.get() == 0, |this| {
                                this.debug_selector(|| FILTER_BUTTON_DEBUG_SELECTOR.to_owned())
                            })
                            .child(self.create_filter_button(cx, col_idx)),
                    ),
            )
            // Last, so that it is painted over the heading's own text: the "+"
            // sits on the boundary a column inserted here would land on, and
            // that boundary is the heading's left edge, the one end of the
            // heading the sort and filter buttons never reach.
            .child(insert_handle);

        // As in the body cells, the probe has to hang off an undecorated
        // wrapper: inside the heading it would be laid out against the padding
        // box and report a rectangle two pixels inside the one the overlay
        // editor has to cover.
        div()
            .relative()
            .flex()
            .w_full()
            .child(heading)
            .when(needs_probe, |this| this.child(self.active_cell_probe(cx)))
            .into_any_element()
    }

    fn create_sort_button(
        &self,
        cx: &mut Context<'_, TabularDataPreviewPane>,
        col_idx: AnyColumn,
    ) -> Button {
        Button::new(
            ElementId::NamedInteger("sort-button".into(), col_idx.get() as u64),
            match self.engine.applied_sorting {
                Some(ordering) if ordering.col_idx == col_idx => match ordering.direction {
                    SortDirection::Asc => "↓",
                    SortDirection::Desc => "↑",
                },
                _ => "↕",
            },
        )
        .size(ButtonSize::Compact)
        .style(
            if self
                .engine
                .applied_sorting
                .is_some_and(|o| o.col_idx == col_idx)
            {
                ButtonStyle::Filled
            } else {
                ButtonStyle::Subtle
            },
        )
        .tooltip(Tooltip::text(match self.engine.applied_sorting {
            Some(ordering) if ordering.col_idx == col_idx => match ordering.direction {
                SortDirection::Asc => "昇順で並べ替え中。クリックで降順",
                SortDirection::Desc => "降順で並べ替え中。クリックで解除",
            },
            _ => "並べ替えなし。クリックで昇順",
        }))
        .on_click(cx.listener(move |this, _event, _window, cx| {
            let new_sorting = match this.engine.applied_sorting {
                Some(ordering) if ordering.col_idx == col_idx => match ordering.direction {
                    SortDirection::Asc => Some(AppliedSorting {
                        col_idx,
                        direction: SortDirection::Desc,
                    }),
                    SortDirection::Desc => None,
                },
                _ => Some(AppliedSorting {
                    col_idx,
                    direction: SortDirection::Asc,
                }),
            };
            this.engine.applied_sorting = new_sorting;
            this.apply_sort(cx);
            cx.notify();
        }))
    }

    fn create_filter_button(
        &self,
        cx: &mut Context<'_, TabularDataPreviewPane>,
        col: AnyColumn,
    ) -> PopoverMenu<Picker<ColumnFilterDelegate>> {
        let has_active_filters = self.engine.has_active_filters(col);
        let sort_order = self.settings.filter_sort_order;
        // Only the caret's own column carries the handle, so that the keyboard
        // shortcut always deploys the menu of the column the user is standing in.
        let is_caret_column = self.grid.cursor.col == col;
        let tooltip: SharedString = match self.engine.text_filter(col) {
            Some(filter) => format!("絞り込み中: {}。クリックで変更", filter.description()).into(),
            None if has_active_filters => "選んだ値で絞り込み中。クリックで変更".into(),
            None => "絞り込み・並べ替えのメニュー (Alt+↓)".into(),
        };

        PopoverMenu::new(ElementId::NamedInteger(
            "filter-menu".into(),
            col.get() as u64,
        ))
        .when(is_caret_column, |menu| {
            menu.with_handle(self.filter_menu_handle.clone())
        })
        .trigger_with_tooltip(
            IconButton::new(
                ElementId::NamedInteger("filter-button".into(), col.get() as u64),
                // The funnel is Excel's "this column is filtered"; an unfiltered
                // column shows the bare arrow that opens the menu.
                if has_active_filters {
                    IconName::FilterFunnel
                } else {
                    IconName::ChevronDown
                },
            )
            .icon_size(IconSize::Small)
            .icon_color(if has_active_filters {
                Color::Accent
            } else {
                Color::Muted
            })
            .style(if has_active_filters {
                ButtonStyle::Filled
            } else {
                ButtonStyle::Subtle
            })
            .toggle_state(has_active_filters),
            Tooltip::text(tooltip),
        )
        .menu({
            // Weak, because the caret column's menu is registered with a handle
            // the pane itself owns: a builder holding the pane strongly would
            // close a cycle and keep the grid alive after its tab was closed.
            let view_entity = cx.entity().downgrade();
            move |window, cx| {
                let view = view_entity.upgrade()?;
                let column_filters = match view.read(cx).engine.get_filters_for_column(col) {
                    Ok(filters) => filters,
                    Err(err) => {
                        log::error!("Failed to get filters for column: {err}");
                        return None;
                    }
                };
                let view_entity = view_entity.clone();

                let picker = cx.new(|cx| {
                    let delegate =
                        ColumnFilterDelegate::new(col, view_entity, sort_order, column_filters, cx);
                    Picker::list(delegate, window, cx)
                        .popover()
                        .initial_width(rems(18.75))
                        .show_scrollbar(true)
                });
                picker.focus_handle(cx).focus(window, cx);
                Some(picker)
            }
        })
    }

    ///// Filtering from the grid itself /////

    /// Narrows `col` to this set of values, replacing whatever it was filtered
    /// by before.
    pub(crate) fn set_value_filter(
        &mut self,
        col: AnyColumn,
        values: HashSet<Option<SharedString>>,
        cx: &mut Context<Self>,
    ) {
        self.engine.set_value_filter(col, values);
        self.apply_filter_sort(cx);
    }

    /// Narrows `col` to the rows whose value satisfies a text condition,
    /// replacing whatever it was filtered by before.
    pub(crate) fn set_text_filter(
        &mut self,
        col: AnyColumn,
        op: TextFilterOp,
        needle: SharedString,
        cx: &mut Context<Self>,
    ) {
        self.engine.set_text_filter(col, op, needle);
        self.apply_filter_sort(cx);
    }

    /// Joins a second text condition onto the one `col` already carries.
    pub(crate) fn set_second_text_filter(
        &mut self,
        col: AnyColumn,
        join: FilterJoin,
        op: TextFilterOp,
        needle: SharedString,
        cx: &mut Context<Self>,
    ) {
        self.engine.set_second_text_filter(col, join, op, needle);
        self.apply_filter_sort(cx);
    }

    /// Switches `col`'s two conditions between "かつ" and "または".
    pub(crate) fn set_filter_join(
        &mut self,
        col: AnyColumn,
        join: FilterJoin,
        cx: &mut Context<Self>,
    ) {
        self.engine.set_filter_join(col, join);
        self.apply_filter_sort(cx);
    }

    /// Drops `col`'s second condition, leaving the first one filtering.
    pub(crate) fn clear_second_text_filter(&mut self, col: AnyColumn, cx: &mut Context<Self>) {
        self.engine.clear_second_text_filter(col);
        self.apply_filter_sort(cx);
    }

    /// The way back from a table narrowed on several columns at once, which
    /// otherwise takes one visit to each column's menu.
    pub(crate) fn clear_all_filters(&mut self, cx: &mut Context<Self>) {
        if !self.engine.has_any_filters() {
            return;
        }
        self.engine.clear_all_filters();
        self.apply_filter_sort(cx);
    }

    /// The grid's own filter actions. Registered on the root element beside
    /// [`Self::register_grid_actions`]; these are actions of the grid's own
    /// namespace, so the overlay editor never claims them and they need no
    /// `capture_action`.
    pub(crate) fn register_filter_actions(&self, element: Div, cx: &mut Context<Self>) -> Div {
        element
            .on_action(cx.listener(|this, _: &ClearAllFilters, _window, cx| {
                this.clear_all_filters(cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleFilterMenu, window, cx| {
                // The popover takes the focus, and the caret's cell is committed
                // on losing it anyway; doing it here keeps the order fixed.
                this.commit_editing(window, cx);
                // A column scrolled out of the view has no button on screen to
                // hang the popover off, so it has to be brought back first.
                this.scroll_column_into_view(window, cx);
                // The menu is built by a closure that reads this pane, so
                // deploying it from inside an update of the pane would be a
                // re-entrant read.
                let handle = this.filter_menu_handle.clone();
                window.defer(cx, move |window, cx| handle.toggle(window, cx));
            }))
    }

    /// Opens `col`'s filter popover, which is what the column heading's context
    /// menu offers.
    ///
    /// The handle the popover is opened through is registered by the caret
    /// column's own button and by no other, so a column that is not the caret's
    /// has to become it first - and the button carrying the handle is only
    /// rebuilt on the next frame, which is why the menu is opened from one.
    pub(crate) fn open_filter_menu_for_column(
        &mut self,
        col: AnyColumn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit_editing(window, cx);
        let caret_has_to_move = self.grid.cursor.col != col;
        if caret_has_to_move {
            self.change_column_selection(col, SelectionChange::Replace, cx);
        }
        self.scroll_column_into_view(window, cx);

        let handle = self.filter_menu_handle.clone();
        if caret_has_to_move {
            window.on_next_frame(move |window, cx| handle.show(window, cx));
            return;
        }
        window.defer(cx, move |window, cx| handle.show(window, cx));
    }
}

#[cfg(test)]
mod tests {
    use db::kvp::KeyValueStore;
    use gpui::{Entity, TestAppContext, VisualTestContext, px};

    use super::*;
    use crate::{
        grid_editing::tests::{open_grid, open_grid_in_workspace},
        table_data_engine::{StoredTableView, filtering_by_column::FilterEntry},
        types::{DataRow, DisplayRow},
        workspace_item::{FILTER_SAVE_DEBOUNCE, filters_key_for_editor},
    };
    use workspace::SaveIntent;

    fn row(value: &str, hidden_by: Option<AnyColumn>) -> ColumnFilterRow {
        ColumnFilterRow {
            entry: FilterEntry {
                content: Some(value.to_owned().into()),
                rows: vec![DataRow(0)],
            },
            is_applied: false,
            hidden_by,
        }
    }

    fn listed(indices: impl IntoIterator<Item = usize>) -> Vec<ColumnFilterListEntry> {
        indices
            .into_iter()
            .map(|row_index| ColumnFilterListEntry::Row {
                row_index,
                positions: Vec::new(),
            })
            .collect()
    }

    /// Narrowing to the whole list has to come out as "no filter" rather than as
    /// a filter naming every current value, which would keep excluding rows that
    /// a later edit gives a value the set never heard of.
    #[test]
    fn selecting_the_whole_list_clears_the_column_instead_of_naming_every_value() {
        let rows = vec![row("a", None), row("b", None)];
        assert_eq!(listed_value_filter(&rows, &listed([0, 1])), None);
    }

    #[test]
    fn selecting_part_of_the_list_narrows_the_column_to_it() {
        let rows = vec![row("a", None), row("b", None), row("c", None)];
        let values = listed_value_filter(&rows, &listed([0, 2]))
            .expect("two of three values is a narrower filter");
        assert_eq!(
            values,
            HashSet::from_iter([Some("a".into()), Some("c".into())])
        );
    }

    /// Values another column's filter has hidden are not ones the user could
    /// check by hand, so "select everything listed" has to skip them - and has to
    /// count "everything" the same way, or selecting the full list of a column
    /// with hidden values would never read as "no filter".
    #[test]
    fn selecting_the_whole_list_ignores_values_hidden_by_another_column() {
        let rows = vec![row("a", None), row("b", None), row("c", Some(AnyColumn(1)))];
        assert_eq!(listed_value_filter(&rows, &listed([0, 1, 2])), None);

        let values = listed_value_filter(&rows, &listed([0, 2]))
            .expect("one of two available values is a narrower filter");
        assert_eq!(values, HashSet::from_iter([Some("a".into())]));
    }

    /// The user could not find the filter at all, because the heading only showed
    /// its buttons while the pointer was over the column. The arrow now stands in
    /// every heading; the sort button beside it is still a hover affordance,
    /// which is what makes this assertion mean anything - `debug_bounds` reports
    /// an element's rectangle whether or not it was painted, so the control is
    /// the button that is *not* drawn.
    #[gpui::test]
    async fn the_filter_arrow_is_drawn_without_hovering_the_column(cx: &mut TestAppContext) {
        let (_pane, _editor, mut cx) = open_grid("name,value\nalpha,1\nbeta,2", cx).await;
        cx.run_until_parked();

        let arrow = cx
            .debug_bounds(FILTER_BUTTON_DEBUG_SELECTOR)
            .expect("the filter arrow should be drawn with no pointer over the heading");
        assert!(
            arrow.size.width > px(0.) && arrow.size.height > px(0.),
            "the filter arrow was drawn with no size ({arrow:?})"
        );
        assert!(
            cx.debug_bounds(SORT_BUTTON_DEBUG_SELECTOR).is_none(),
            "the sort button should stay hidden until the heading is hovered"
        );
    }

    /// With the header row turned off the headings only carry their spreadsheet
    /// letters, which was one candidate explanation for the filter going
    /// unnoticed: the buttons have to be there in that mode too.
    #[gpui::test]
    async fn the_filter_arrow_is_drawn_without_a_header_row(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("name,value\nalpha,1\nbeta,2", cx).await;
        cx.run_until_parked();

        cx.dispatch_action(crate::ToggleHeaderRow);
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert!(!pane.has_header_row());
        });

        let arrow = cx
            .debug_bounds(FILTER_BUTTON_DEBUG_SELECTOR)
            .expect("the filter arrow should be drawn on a grid with no header row");
        assert!(
            arrow.size.width > px(0.) && arrow.size.height > px(0.),
            "the filter arrow was drawn with no size ({arrow:?})"
        );
    }

    /// Excel's Alt+Down. The binding sits in the `"TabularDataGrid > Editor"`
    /// block because the overlay editor holds the focus, and `alt-down` is bound
    /// to `editor::MoveLineDown` at the editor's own depth.
    #[gpui::test]
    async fn alt_down_opens_the_caret_columns_filter_menu(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("name,value\nalpha,1\nbeta,2", cx).await;
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert!(!pane.filter_menu_handle.is_deployed());
        });

        cx.simulate_keystrokes("right");
        cx.run_until_parked();
        cx.simulate_keystrokes("alt-down");
        cx.run_until_parked();

        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.grid.cursor.col, AnyColumn(1));
            assert!(
                pane.filter_menu_handle.is_deployed(),
                "alt-down should have deployed the caret column's filter menu"
            );
        });

        cx.simulate_keystrokes("alt-down");
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert!(
                !pane.filter_menu_handle.is_deployed(),
                "alt-down should close the menu it opened"
            );
        });
    }

    /// The way back out of a table narrowed on two columns at once.
    #[gpui::test]
    async fn clear_all_filters_brings_every_row_back(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("c0,c1\na,keep\nb,keep\na,drop", cx).await;
        cx.run_until_parked();

        pane.update(&mut cx, |pane, cx| {
            pane.toggle_filter(AnyColumn(0), Some("a".into()), cx);
            pane.toggle_filter(AnyColumn(1), Some("keep".into()), cx);
        });
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.body_row_count(), 1);
            assert!(pane.engine.has_any_filters());
        });

        cx.dispatch_action(ClearAllFilters);
        cx.run_until_parked();

        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.body_row_count(), 3);
            assert!(!pane.engine.has_any_filters());
        });
    }

    /// A text condition narrows the grid itself, not just the value list.
    #[gpui::test]
    async fn a_text_condition_narrows_the_grid(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("c0,c1\nApfel,1\napfelsaft,2\nBirne,3", cx).await;
        cx.run_until_parked();

        pane.update(&mut cx, |pane, cx| {
            pane.set_text_filter(AnyColumn(0), TextFilterOp::Contains, "APFEL".into(), cx);
        });
        cx.run_until_parked();

        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.body_row_count(), 2);
            assert_eq!(
                pane.cell_value(GridRow::Body(DisplayRow(0)), AnyColumn(0)),
                SharedString::from("Apfel")
            );
            assert_eq!(
                pane.cell_value(GridRow::Body(DisplayRow(1)), AnyColumn(0)),
                SharedString::from("apfelsaft")
            );
        });
    }

    fn buffer_text(editor: &Entity<editor::Editor>, cx: &mut VisualTestContext) -> String {
        editor.read_with(cx, |editor, cx| editor.text(cx))
    }

    /// While rows are hidden a display row is not its own data row, so an edit
    /// has to travel through the mapping rather than count lines of the file.
    #[gpui::test]
    async fn editing_a_filtered_row_writes_to_its_own_data_row(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("c0,c1\na,1\nb,2\na,3", cx).await;
        cx.run_until_parked();

        pane.update(&mut cx, |pane, cx| {
            pane.toggle_filter(AnyColumn(0), Some("a".into()), cx);
        });
        cx.run_until_parked();

        // The second row still showing is the file's fourth line, not its third.
        cx.simulate_keystrokes("down down right");
        cx.run_until_parked();
        cx.simulate_input("x");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "c0,c1\na,1\nb,2\na,x");
    }

    /// The same for a structural edit: the new row goes after the data row the
    /// caret is on, wherever the filter has moved that row on screen.
    #[gpui::test]
    async fn inserting_a_row_while_filtered_lands_after_its_data_row(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("c0,c1\na,1\nb,2\na,3", cx).await;
        cx.run_until_parked();

        pane.update(&mut cx, |pane, cx| {
            pane.toggle_filter(AnyColumn(0), Some("a".into()), cx);
        });
        cx.run_until_parked();

        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        cx.dispatch_action(crate::InsertRowBelow);
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "c0,c1\na,1\n,\nb,2\na,3");
    }

    /// The pane owns the popover handle, the handle owns the closure that builds
    /// the menu, and that closure needs the pane: closing the loop with a strong
    /// handle leaves the grid - and the colors model it shares - alive for the
    /// rest of the session.
    #[gpui::test]
    async fn closing_the_grid_releases_it_despite_the_filter_menu_handle(cx: &mut TestAppContext) {
        let (workspace, pane, _editor, mut cx) = open_grid_in_workspace("a,b\nc,d", cx).await;
        cx.run_until_parked();
        // Deployed at least once, so that the handle really holds the builder.
        cx.simulate_keystrokes("alt-down");
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert!(pane.filter_menu_handle.is_deployed());
        });

        let item_id = pane.entity_id();
        let released = pane.downgrade();
        drop(pane);
        workspace
            .update_in(&mut cx, |workspace, window, cx| {
                workspace.active_pane().update(cx, |pane, cx| {
                    pane.close_item_by_id(item_id, SaveIntent::Skip, window, cx)
                })
            })
            .await
            .expect("closing the grid tab should succeed");
        cx.run_until_parked();

        released.assert_released();
    }

    /// The column heading's menu is the other way to the filter popover, for a
    /// user who never notices the arrow. It opens the popover of the column that
    /// was clicked, which the caret has to be on for the handle to belong to it.
    #[gpui::test]
    async fn the_heading_menu_opens_the_filter_popover(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("name,value\nalpha,1\nbeta,2", cx).await;
        cx.run_until_parked();

        pane.update_in(&mut cx, |pane, window, cx| {
            pane.open_filter_menu_for_column(AnyColumn(0), window, cx);
        });
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.grid.cursor.col, AnyColumn(0));
            assert!(
                pane.filter_menu_handle.is_deployed(),
                "the caret's own column opens straight away"
            );
        });
    }

    /// A column that is not the caret's has to become it first: the handle that
    /// opens the popover is registered by the caret column's button alone, so
    /// asking for another column's menu has to move the caret there.
    #[gpui::test]
    async fn the_heading_menu_moves_the_caret_to_the_column_it_opens(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("name,value\nalpha,1\nbeta,2", cx).await;
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.grid.cursor.col, AnyColumn(0))
        });

        pane.update_in(&mut cx, |pane, window, cx| {
            pane.open_filter_menu_for_column(AnyColumn(1), window, cx);
        });
        cx.run_until_parked();

        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.grid.cursor.col,
                AnyColumn(1),
                "the popover's handle only ever belongs to the caret's column"
            );
            assert_eq!(
                pane.grid.selection_kind,
                crate::grid::GridSelectionKind::Columns
            );
        });
    }

    ///// Two conditions on one column, through the popover /////

    /// The pair Excel's custom filter dialog builds, driven the way the popover
    /// drives it: a first condition, a join, then a second condition.
    #[gpui::test]
    async fn a_second_condition_narrows_the_grid_further(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) =
            open_grid("c0\nApfel\napfelsaft\nBirne\nbirnensaft", cx).await;
        cx.run_until_parked();

        pane.update(&mut cx, |pane, cx| {
            pane.set_text_filter(AnyColumn(0), TextFilterOp::Contains, "apfel".into(), cx);
        });
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| assert_eq!(pane.body_row_count(), 2));

        pane.update(&mut cx, |pane, cx| {
            pane.set_second_text_filter(
                AnyColumn(0),
                FilterJoin::And,
                TextFilterOp::EndsWith,
                "saft".into(),
                cx,
            );
        });
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.body_row_count(), 1);
            assert_eq!(
                pane.cell_value(GridRow::Body(DisplayRow(0)), AnyColumn(0)),
                SharedString::from("apfelsaft")
            );
        });

        // "または" widens the same pair without either being retyped.
        pane.update(&mut cx, |pane, cx| {
            pane.set_filter_join(AnyColumn(0), FilterJoin::Or, cx);
        });
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| assert_eq!(pane.body_row_count(), 3));

        pane.update(&mut cx, |pane, cx| {
            pane.clear_second_text_filter(AnyColumn(0), cx);
        });
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| assert_eq!(pane.body_row_count(), 2));
    }

    ///// The filters kept between sessions /////

    fn settle_filter_save(cx: &mut VisualTestContext) {
        cx.executor().advance_clock(FILTER_SAVE_DEBOUNCE * 2);
        cx.run_until_parked();
    }

    fn filters_key(pane: &Entity<TabularDataPreviewPane>, cx: &mut VisualTestContext) -> String {
        cx.update(|_window, cx| filters_key_for_editor(pane.read(cx).editor(), cx))
            .expect("a grid on a saved file should have a key")
    }

    fn stored_filters(key: &str, cx: &mut VisualTestContext) -> Option<StoredTableView> {
        cx.update(|_window, cx| {
            let payload = KeyValueStore::global(cx)
                .read_kvp(key)
                .expect("reading the key value store should succeed")?;
            Some(serde_json::from_str(&payload).expect("the stored payload should parse"))
        })
    }

    /// The point of storing the filters is what a grid opened later shows, so
    /// this goes all the way to the row count of a second pane on the file
    /// rather than stopping at "the payload came back".
    #[gpui::test]
    async fn a_stored_filter_hides_the_same_rows_in_a_grid_opened_later(cx: &mut TestAppContext) {
        let (workspace, pane, editor, mut cx) =
            open_grid_in_workspace("c0,c1\na,1\nb,2\na,3", cx).await;
        cx.run_until_parked();
        let key = filters_key(&pane, &mut cx);

        pane.update(&mut cx, |pane, cx| {
            pane.toggle_filter(AnyColumn(0), Some("a".into()), cx);
        });
        settle_filter_save(&mut cx);
        assert!(
            stored_filters(&key, &mut cx).is_some(),
            "the filter should have reached the store"
        );

        let reopened = workspace.update_in(&mut cx, |_workspace, window, cx| {
            cx.new(|cx| TabularDataPreviewPane::build(editor.clone(), window, cx))
        });
        cx.run_until_parked();

        reopened.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.body_row_count(),
                2,
                "the grid opened later should hide the same row"
            );
            assert!(pane.engine.has_active_filters(AnyColumn(0)));
            assert_eq!(
                pane.cell_value(GridRow::Body(DisplayRow(1)), AnyColumn(1)),
                SharedString::from("3")
            );
        });
    }

    /// Clearing the filters has to empty the stored row too, or the next grid
    /// would put back a narrowing the user just undid.
    #[gpui::test]
    async fn clearing_the_filters_empties_the_stored_row(cx: &mut TestAppContext) {
        let (workspace, pane, editor, mut cx) =
            open_grid_in_workspace("c0,c1\na,1\nb,2\na,3", cx).await;
        cx.run_until_parked();
        let key = filters_key(&pane, &mut cx);

        pane.update(&mut cx, |pane, cx| {
            pane.toggle_filter(AnyColumn(0), Some("a".into()), cx);
        });
        settle_filter_save(&mut cx);
        pane.update(&mut cx, |pane, cx| pane.clear_all_filters(cx));
        settle_filter_save(&mut cx);

        assert_eq!(stored_filters(&key, &mut cx), None);

        let reopened = workspace.update_in(&mut cx, |_workspace, window, cx| {
            cx.new(|cx| TabularDataPreviewPane::build(editor.clone(), window, cx))
        });
        cx.run_until_parked();
        reopened.read_with(&cx, |pane, _| {
            assert_eq!(pane.body_row_count(), 3);
            assert!(!pane.engine.has_any_filters());
        });
    }

    /// "Save as" gives the buffer a new path, and the view is stored under the
    /// path: it has to follow the file rather than stay on the name it left.
    #[gpui::test]
    async fn saving_under_a_new_name_moves_the_filters(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("c0,c1\na,1\nb,2\na,3", cx).await;
        cx.run_until_parked();
        let old_key = filters_key(&pane, &mut cx);

        pane.update(&mut cx, |pane, cx| {
            pane.toggle_filter(AnyColumn(0), Some("a".into()), cx);
        });
        settle_filter_save(&mut cx);
        assert!(stored_filters(&old_key, &mut cx).is_some());

        let (project, worktree_id) = cx.update(|_window, cx| {
            let project = editor
                .read(cx)
                .project()
                .cloned()
                .expect("the fixture editor should belong to a project");
            let worktree_id = project
                .read(cx)
                .worktrees(cx)
                .next()
                .expect("the fixture project should have a worktree")
                .read(cx)
                .id();
            (project, worktree_id)
        });
        pane.update_in(&mut cx, |pane, window, cx| {
            workspace::item::Item::save_as(
                pane,
                project,
                project::ProjectPath {
                    worktree_id,
                    path: util::rel_path::rel_path("copy.csv").into(),
                },
                window,
                cx,
            )
        })
        .await
        .expect("saving the fixture under a new name should succeed");
        settle_filter_save(&mut cx);

        let new_key = filters_key(&pane, &mut cx);
        assert!(
            new_key.ends_with("copy.csv"),
            "the filters should follow the file to {new_key}"
        );
        assert!(
            stored_filters(&new_key, &mut cx).is_some(),
            "the new path should hold the filters"
        );
        assert_eq!(
            stored_filters(&old_key, &mut cx),
            None,
            "the row of the path the file left should be gone"
        );
    }

    /// The restored grid says so, because rows missing from a file the user
    /// just opened otherwise read as a broken table.
    #[gpui::test]
    async fn restoring_a_filter_tells_the_user_how_many_rows_are_hidden(cx: &mut TestAppContext) {
        let (workspace, pane, editor, mut cx) =
            open_grid_in_workspace("c0,c1\na,1\nb,2\na,3", cx).await;
        cx.run_until_parked();

        pane.update(&mut cx, |pane, cx| {
            pane.toggle_filter(AnyColumn(0), Some("a".into()), cx);
        });
        settle_filter_save(&mut cx);

        let reopened = workspace.update_in(&mut cx, |_workspace, window, cx| {
            cx.new(|cx| TabularDataPreviewPane::build(editor.clone(), window, cx))
        });
        cx.run_until_parked();

        reopened.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.grid.status_message.as_deref(),
                Some("前回の絞り込みを復元しました (1 行を非表示)")
            );
        });
    }

    /// Columns inserted or deleted to the left of a filtered column used to
    /// leave the filter behind on an index that now belongs to its neighbour.
    #[gpui::test]
    async fn a_column_inserted_to_the_left_takes_the_filter_with_it(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("c0,c1,c2\na,keep,1\nb,drop,2\nc,keep,3", cx).await;
        cx.run_until_parked();

        pane.update(&mut cx, |pane, cx| {
            pane.toggle_filter(AnyColumn(1), Some("keep".into()), cx);
        });
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| assert_eq!(pane.body_row_count(), 2));

        pane.update(&mut cx, |pane, cx| {
            pane.select_cell(
                GridCell::new(GridRow::Body(DisplayRow(0)), AnyColumn(0)),
                false,
                cx,
            );
        });
        cx.dispatch_action(crate::InsertColumnLeft);
        cx.run_until_parked();

        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.body_row_count(),
                2,
                "the filter has to follow its own values into column C"
            );
            assert_eq!(
                pane.cell_value(GridRow::Body(DisplayRow(0)), AnyColumn(2)),
                SharedString::from("keep")
            );
            assert!(pane.engine.has_active_filters(AnyColumn(2)));
        });
    }

    /// Deleting the filtered column brings every row back, and undoing the
    /// delete hides them again rather than leaving the table wide open.
    #[gpui::test]
    async fn deleting_the_filtered_column_and_undoing_it_restores_the_filter(
        cx: &mut TestAppContext,
    ) {
        let (pane, _editor, mut cx) = open_grid("c0,c1,c2\na,keep,1\nb,drop,2\nc,keep,3", cx).await;
        cx.run_until_parked();

        pane.update(&mut cx, |pane, cx| {
            pane.toggle_filter(AnyColumn(1), Some("keep".into()), cx);
            pane.change_column_selection(AnyColumn(1), SelectionChange::Replace, cx);
        });
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| assert_eq!(pane.body_row_count(), 2));

        cx.dispatch_action(crate::DeleteColumns);
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.body_row_count(),
                3,
                "with the column gone there is nothing left to filter by"
            );
            assert!(!pane.engine.has_any_filters());
        });

        cx.dispatch_action(editor::actions::Undo);
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.body_row_count(),
                2,
                "undoing the delete has to bring the filter back with the column"
            );
            assert_eq!(
                pane.cell_value(GridRow::Body(DisplayRow(0)), AnyColumn(1)),
                SharedString::from("keep")
            );
        });
    }

    /// The records are matched to the buffer transactions they belong to, so a
    /// grid edit that is not a column operation has to pass through the middle
    /// of a run of them without taking anyone's turn: insert, sort, edit a cell,
    /// delete, then undo all the way back and redo.
    #[gpui::test]
    async fn column_operations_around_another_edit_undo_and_redo_in_step(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("c0,c1,c2\na,keep,1\nb,drop,2\nc,keep,3", cx).await;
        cx.run_until_parked();

        let filtered_column = |pane: &TabularDataPreviewPane| {
            (0..8)
                .map(AnyColumn)
                .find(|&col| pane.engine.has_active_filters(col))
        };

        pane.update(&mut cx, |pane, cx| {
            pane.toggle_filter(AnyColumn(1), Some("keep".into()), cx);
        });
        cx.run_until_parked();

        // Insert, which moves the filter right.
        pane.update(&mut cx, |pane, cx| {
            pane.select_cell(
                GridCell::new(GridRow::Body(DisplayRow(0)), AnyColumn(0)),
                false,
                cx,
            );
        });
        cx.dispatch_action(crate::InsertColumnLeft);
        cx.run_until_parked();

        // Sorting applied after the insert is the newer of the two and keeps the
        // column it was applied to through everything below.
        pane.update(&mut cx, |pane, cx| {
            pane.engine.applied_sorting = Some(AppliedSorting {
                col_idx: AnyColumn(2),
                direction: SortDirection::Asc,
            });
            pane.apply_sort(cx);
        });
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert_eq!(filtered_column(pane), Some(AnyColumn(2)));
            assert_eq!(pane.body_row_count(), 2);
        });

        // A plain cell edit in the middle: a transaction of its own, and no
        // column operation at all. It has to land on a cell that really holds
        // something, or it writes nothing and is no transaction at all.
        let cleared = GridCell::new(GridRow::Body(DisplayRow(0)), AnyColumn(1));
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.cell_value(cleared.row, cleared.col),
                SharedString::from("a")
            );
        });
        pane.update(&mut cx, |pane, cx| pane.select_cell(cleared, false, cx));
        cx.dispatch_action(crate::ClearCells);
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.cell_value(cleared.row, cleared.col),
                SharedString::new("")
            );
            assert_eq!(filtered_column(pane), Some(AnyColumn(2)));
        });

        // Delete, which takes the filter out of the stack entirely.
        pane.update(&mut cx, |pane, cx| {
            pane.change_column_selection(AnyColumn(2), SelectionChange::Replace, cx);
        });
        cx.dispatch_action(crate::DeleteColumns);
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert_eq!(filtered_column(pane), None);
            assert_eq!(pane.body_row_count(), 3);
        });

        cx.dispatch_action(editor::actions::Undo);
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                filtered_column(pane),
                Some(AnyColumn(2)),
                "undoing the delete brings the filter back with its column"
            );
            assert_eq!(pane.body_row_count(), 2);
        });

        cx.dispatch_action(editor::actions::Undo);
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.cell_value(cleared.row, cleared.col),
                SharedString::from("a"),
                "the undo in the middle is the cell edit"
            );
            assert_eq!(
                filtered_column(pane),
                Some(AnyColumn(2)),
                "the cell edit is not a column operation and must not move anything"
            );
        });

        cx.dispatch_action(editor::actions::Undo);
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                filtered_column(pane),
                Some(AnyColumn(1)),
                "undoing the insert takes the filter back left"
            );
            assert_eq!(pane.body_row_count(), 2);
        });

        cx.dispatch_action(editor::actions::Redo);
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                filtered_column(pane),
                Some(AnyColumn(2)),
                "redoing it puts the filter back where the insert had left it"
            );
            assert_eq!(pane.body_row_count(), 2);
        });
    }

    /// The write waits out a pause, and the tab the user narrowed is very often
    /// closed inside it - the same ground the colors cover.
    #[gpui::test]
    async fn closing_the_tab_inside_the_debounce_still_stores_the_filters(cx: &mut TestAppContext) {
        let (workspace, pane, _editor, mut cx) =
            open_grid_in_workspace("c0,c1\na,1\nb,2\na,3", cx).await;
        cx.run_until_parked();
        let key = filters_key(&pane, &mut cx);
        let item_id = pane.entity_id();

        pane.update(&mut cx, |pane, cx| {
            pane.toggle_filter(AnyColumn(0), Some("a".into()), cx);
        });
        cx.run_until_parked();
        drop(pane);

        workspace
            .update_in(&mut cx, |workspace, window, cx| {
                workspace.active_pane().update(cx, |pane, cx| {
                    pane.close_item_by_id(item_id, SaveIntent::Skip, window, cx)
                })
            })
            .await
            .expect("closing the grid tab should succeed");
        cx.run_until_parked();

        settle_filter_save(&mut cx);
        assert!(
            stored_filters(&key, &mut cx).is_some(),
            "a closed tab's last narrowing should still have been stored"
        );
    }

    /// A row edited by hand, or left behind by a later version, must not keep a
    /// grid from opening - and must not leave it narrowed by something nobody
    /// can read back.
    #[gpui::test]
    async fn a_stored_row_that_cannot_be_read_leaves_the_grid_alone(cx: &mut TestAppContext) {
        let (workspace, pane, editor, mut cx) =
            open_grid_in_workspace("c0,c1\na,1\nb,2\na,3", cx).await;
        cx.run_until_parked();
        let key = filters_key(&pane, &mut cx);

        let store = cx.update(|_window, cx| KeyValueStore::global(cx));
        store
            .write_kvp(
                key,
                r#"{"filters":[{"col":0,"kind":"who_knows"}]"#.to_owned(),
            )
            .await
            .expect("writing the fixture payload should succeed");

        let reopened = workspace.update_in(&mut cx, |_workspace, window, cx| {
            cx.new(|cx| TabularDataPreviewPane::build(editor.clone(), window, cx))
        });
        cx.run_until_parked();
        reopened.read_with(&cx, |pane, _| {
            assert_eq!(pane.body_row_count(), 3);
            assert!(!pane.engine.has_any_filters());
            assert_eq!(pane.grid.status_message, None);
        });
    }

    /// Right clicking a column heading leaves the caret on that column even when
    /// the click landed inside a wider column selection, because the heading
    /// menu's filter entry opens the caret column's popover and no other. The
    /// whole path has to hold, or the entry silently filters a different column.
    #[gpui::test]
    async fn right_clicking_a_heading_leaves_its_filter_entry_pointing_at_it(
        cx: &mut TestAppContext,
    ) {
        let (pane, _editor, mut cx) = open_grid("a,b,c\n1,2,3", cx).await;
        cx.run_until_parked();

        // Columns A to C dragged as one block, with the caret at its far end.
        pane.update(&mut cx, |pane, cx| {
            pane.change_column_selection(AnyColumn(0), SelectionChange::Replace, cx);
            pane.change_column_selection(AnyColumn(2), SelectionChange::Extend, cx);
        });
        pane.update_in(&mut cx, |pane, window, cx| {
            pane.on_column_mouse_down(
                AnyColumn(1),
                MouseButton::Right,
                gpui::Modifiers::none(),
                gpui::point(px(0.), px(0.)),
                window,
                cx,
            );
        });
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.grid.cursor.col,
                AnyColumn(1),
                "the clicked column has to become the caret's"
            );
        });

        // A menu entry's handler runs while the menu it was picked from is
        // still open; the menu only dismisses itself afterwards.
        pane.update_in(&mut cx, |pane, window, cx| {
            pane.open_filter_menu_for_column(AnyColumn(1), window, cx);
        });
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert!(
                pane.filter_menu_handle.is_deployed(),
                "the entry has to reach the popover of the column that was clicked"
            );
        });
    }
}
