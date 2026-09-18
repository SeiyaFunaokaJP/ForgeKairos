//! The side by side table comparison: one tab holding two panes of cells, the
//! left one what the table is being compared against and the right one what it
//! is now, with the rows of the two lined up against each other.
//!
//! The comparison itself lives in [`crate::table_diff`]; this module is the
//! view over it, and the three entry points that build one.

use std::ops::Range;

use anyhow::Result;
use buffer_diff::{BufferDiff, BufferDiffEvent};
use chrono::{DateTime, Local};
use editor::markdown_actions::display_width;
use gpui::{
    Action as _, AnyElement, App, AppContext as _, Entity, EventEmitter, FocusHandle, Focusable,
    HighlightStyle, PathPromptOptions, ScrollHandle, ScrollStrategy, StyledText, Subscription,
    Task, UniformListScrollHandle, WeakEntity, actions, uniform_list,
};
use language::{Buffer, BufferEvent};
use project::{DirectoryLister, Project};
use settings::Settings as _;
use theme_settings::ThemeSettings;
use ui::{
    ActiveTheme as _, Button, ButtonSize, ButtonStyle, Checkbox, ContextMenu, DropdownMenu,
    ElementId, Icon, IconButton, IconName, IconSize, Label, LabelCommon as _, LabelSize,
    ScrollAxes, Scrollbars, SharedString, StyledTypography as _, ToggleState, Tooltip,
    WithScrollbar as _, div, h_flex, prelude::*, v_flex,
};
use workspace::{
    Workspace,
    item::{Item, TabContentParams},
};

use crate::{
    TabularDataPreviewPane,
    grid::{GridRect, GridRow, cell_line_height, column_label},
    renderer::table_cell::CELL_FRAME_HEIGHT,
    table_diff::{
        CellCharDiff, ChangeStop, DiffRow, DiffRowKind, DiffSide, TableDiff, cell_at,
        cell_char_diff,
    },
    types::AnyColumn,
};

actions!(
    tabular_data,
    [DiffWithHead, DiffWithFile, DiffSelectedCells, RecomputeDiff]
);

/// How long an edit to the file is left to settle before the comparison is
/// computed again. The whole table is re-aligned, so doing it per keystroke
/// would make typing in the grid beside it stutter.
const RECOMPUTE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(300);

/// Rows sampled to size a column, the same bound the grid puts on it: enough to
/// catch the shape of the data without walking a file of millions of rows.
const COLUMN_WIDTH_SAMPLE_ROWS: usize = 200;
/// Rough advance of one character in the buffer font. The comparison is read
/// only, so unlike the grid's columns these cannot be dragged and the estimate
/// is all there is.
const COLUMN_WIDTH_PER_CHARACTER: f32 = 8.0;
const COLUMN_WIDTH_PADDING: f32 = 16.0;
const MINIMUM_COLUMN_WIDTH: f32 = 64.0;
const MAXIMUM_COLUMN_WIDTH: f32 = 320.0;
/// Room for the row number and the padding around it.
const ROW_NUMBER_PADDING: f32 = 14.0;
/// The strip the two horizontal scrollbars are laid into, over the foot of the
/// body the way a scrollbar always overlays what it scrolls.
const HORIZONTAL_SCROLLBAR_HEIGHT: Pixels = px(12.);
/// The header row's place in the element ids the panes are keyed by. It is not
/// a row of the list, so it cannot collide with one.
const HEADER_ELEMENT_INDEX: usize = usize::MAX;

/// Marks the two panes of the header row, so that a test can check that they
/// are laid out beside each other and at the same width - the whole premise of
/// reading two tables side by side. `debug_selector` compiles away outside of
/// tests.
pub(crate) const DIFF_LEFT_PANE_DEBUG_SELECTOR: &str = "tabular-diff-left-pane";
pub(crate) const DIFF_RIGHT_PANE_DEBUG_SELECTOR: &str = "tabular-diff-right-pane";

#[derive(Clone, Copy, PartialEq, Eq)]
enum PaneSide {
    Left,
    Right,
}

/// What the left pane is a view of, which is what lets it follow the version it
/// was taken from rather than freeze at the moment the tab was opened.
enum LeftSource {
    /// The version git has committed. Held as the diff rather than as the text
    /// it happened to hold, so that a commit, a checkout or a staged change
    /// under the file moves the left pane with it.
    Head(Entity<BufferDiff>),
    /// Another file, held open as a buffer so that edits to it land here too.
    File(Entity<Buffer>),
}

impl LeftSource {
    /// The text this side holds now, or `None` when there is nothing to read it
    /// out of any more - a file that stopped being tracked has no committed
    /// version, and an empty left pane would read as every row having just been
    /// added.
    fn read(&self, cx: &App) -> Option<SharedString> {
        match self {
            LeftSource::Head(diff) => diff.read(cx).base_text_string(cx).map(SharedString::from),
            // A buffer whose file was deleted or renamed away still reads as the
            // text it last held, so the file being gone has to be asked about
            // separately or the comparison would go on claiming to be against a
            // file that is not there.
            LeftSource::File(buffer) => {
                let buffer = buffer.read(cx);
                let file_is_gone = buffer
                    .file()
                    .is_some_and(|file| file.disk_state().is_deleted());
                (!file_is_gone).then(|| buffer.text().into())
            }
        }
    }
}

/// One of the two blocks of a cell comparison, as what it takes to read the
/// block again rather than as the values it held.
#[derive(Clone, Copy)]
enum CellBlock {
    /// A rectangle of the grid, read cell by cell at the positions it covers.
    Rect(GridRect),
    Clipboard,
}

/// Reads one block of a cell comparison out of the grid as it stands now.
fn read_cell_block(
    grid: &TabularDataPreviewPane,
    block: CellBlock,
    clipboard: &str,
) -> Vec<Vec<SharedString>> {
    match block {
        CellBlock::Rect(rect) => rect
            .rows()
            .map(|row| {
                rect.cols()
                    .map(|col| grid.cell_value(GridRow::from_index(row), AnyColumn(col)))
                    .collect()
            })
            .collect(),
        CellBlock::Clipboard => vec![vec![clipboard.to_owned().into()]],
    }
}

/// Where the two sides of a comparison come from, which is what decides how it
/// is taken again.
enum DiffSource {
    /// Two whole tables as text, both sides re-read whenever either of them
    /// changes, so that the comparison keeps up with the grid beside it and
    /// with whatever it is being compared against.
    Files {
        left: LeftSource,
        /// The last text the left hand side was successfully read as. A
        /// committed version can go away under the comparison - the file may
        /// stop being tracked - and an empty left pane would claim every row was
        /// just added, so the last version that was there is kept and said to be
        /// stale instead.
        left_text: SharedString,
        buffer: Entity<Buffer>,
        /// Read separately per side: the version committed for this same file is
        /// laid out with the same delimiter, but another file the user picked is
        /// only a table at all when it is read with its own one, so comparing a
        /// `.csv` against a `.tsv` has to split each side its own way.
        left_delimiter: char,
        right_delimiter: char,
    },
    /// Two blocks of cells lifted out of one grid. These do not follow the grid:
    /// a block is a place in the table rather than a thing, and rows moving
    /// under it would quietly change what the comparison is of. The user takes
    /// it again when they want it again.
    Cells {
        /// Weak because the comparison outlives the grid's tab, which is exactly
        /// the case "take it again" has to refuse rather than guess at.
        grid: WeakEntity<TabularDataPreviewPane>,
        left: CellBlock,
        right: CellBlock,
        left_rows: Vec<Vec<SharedString>>,
        right_rows: Vec<Vec<SharedString>>,
    },
}

/// The inputs of one comparison, owned so that it can be handed to a background
/// thread.
enum DiffInput {
    Texts {
        left_text: SharedString,
        right_text: SharedString,
        left_delimiter: char,
        right_delimiter: char,
        first_row_is_header: bool,
    },
    Rows {
        left_rows: Vec<Vec<SharedString>>,
        right_rows: Vec<Vec<SharedString>>,
        first_row_is_header: bool,
    },
}

impl DiffInput {
    fn compute(self) -> (DiffSide, DiffSide, TableDiff) {
        let (left, right) = match self {
            DiffInput::Texts {
                left_text,
                right_text,
                left_delimiter,
                right_delimiter,
                first_row_is_header,
            } => (
                DiffSide::parse(&left_text, left_delimiter, first_row_is_header),
                DiffSide::parse(&right_text, right_delimiter, first_row_is_header),
            ),
            DiffInput::Rows {
                left_rows,
                right_rows,
                first_row_is_header,
            } => (
                DiffSide::from_rows(left_rows, first_row_is_header),
                DiffSide::from_rows(right_rows, first_row_is_header),
            ),
        };
        let diff = TableDiff::compute(&left, &right);
        (left, right, diff)
    }
}

pub(crate) struct TableDiffView {
    focus_handle: FocusHandle,
    title: SharedString,
    left_title: SharedString,
    right_title: SharedString,
    source: DiffSource,
    /// Whether the first line of either side is read as a header. The grid's own
    /// setting seeds it, and from then on it is the comparison's own: the grid's
    /// tab can be closed while this one stays open.
    first_row_is_header: bool,
    left: DiffSide,
    right: DiffSide,
    diff: TableDiff,
    /// When the left hand side was last read, which the pane's title says so
    /// that a comparison left open overnight does not read as being about the
    /// version that is committed now.
    left_taken_at: Option<DateTime<Local>>,
    /// Set when the left hand side could not be read again and the pane is
    /// showing the last version that could be.
    left_is_stale: bool,
    /// Whether a comparison is in flight, which is all there is to show for a
    /// table big enough that aligning it takes a moment.
    is_computing: bool,
    column_widths: Vec<Pixels>,
    row_number_width: Pixels,
    /// `None` while every row is shown; the changed rows only when the view is
    /// filtered down to the differences.
    shown_rows: Option<Vec<usize>>,
    /// Which change "previous" and "next" last stopped at, as an index into
    /// [`TableDiff::change_blocks`].
    current_change: Option<usize>,
    row_height: Pixels,
    list_scroll: UniformListScrollHandle,
    /// Shared by both panes, which is what keeps them showing the same columns:
    /// the handle holds the offset, so a scroll in either pane moves both.
    horizontal_scroll: ScrollHandle,
    recompute_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl TableDiffView {
    ///// Entry points /////

    /// Compares the grid's file against the version git has committed.
    pub(crate) fn diff_with_head(
        workspace: &mut Workspace,
        grid: Entity<TabularDataPreviewPane>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let pane = grid.read(cx);
        let Some(buffer) = pane.buffer(cx) else {
            return;
        };
        let delimiter = pane.current_delimiter(cx);
        let first_row_is_header = pane.settings.first_row_is_header;
        let name = file_name(&buffer, cx);

        let git_store = workspace.project().read(cx).git_store().clone();
        let committed = git_store.update(cx, |git_store, cx| {
            git_store.open_uncommitted_diff(buffer.clone(), cx)
        });

        cx.spawn_in(window, async move |workspace, cx| {
            let committed = load_committed_diff(committed, cx).await;
            let Some(committed) = committed else {
                grid.update(cx, |grid, cx| {
                    grid.set_status_message(
                        "git の管理下にないか、まだ一度もコミットされていないファイルです",
                        cx,
                    );
                });
                return;
            };

            workspace
                .update_in(cx, |workspace, window, cx| {
                    let view = cx.new(|cx| {
                        Self::from_files(
                            format!("{name} ↔ HEAD").into(),
                            "HEAD".into(),
                            "現在".into(),
                            LeftSource::Head(committed),
                            buffer,
                            delimiter,
                            delimiter,
                            first_row_is_header,
                            cx,
                        )
                    });
                    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
                })
                .ok();
        })
        .detach();
    }

    /// Compares the grid's file against another file the user picks.
    pub(crate) fn diff_with_file(
        workspace: &mut Workspace,
        grid: Entity<TabularDataPreviewPane>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let pane = grid.read(cx);
        let Some(buffer) = pane.buffer(cx) else {
            return;
        };
        let delimiter = pane.current_delimiter(cx);
        let first_row_is_header = pane.settings.first_row_is_header;
        let name = file_name(&buffer, cx);

        let project = workspace.project().clone();
        let picked = workspace.prompt_for_open_path(
            PathPromptOptions {
                files: true,
                directories: false,
                multiple: false,
                prompt: Some("比較する".into()),
                starting_directory: None,
            },
            DirectoryLister::Project(project.clone()),
            window,
            cx,
        );

        cx.spawn(async move |workspace, cx| {
            let Some(path) = picked
                .await
                .ok()
                .flatten()
                .and_then(|paths| paths.into_iter().next())
            else {
                return;
            };
            let other_name: SharedString = path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| path.to_string_lossy().to_string())
                .into();
            // The picked file is split the way the grid would split it if it
            // were the one open, so that comparing a `.csv` against a `.tsv`
            // reads both as tables rather than the tab separated one as a
            // single column.
            let other_delimiter = path
                .extension()
                .and_then(|extension| extension.to_str())
                .and_then(crate::parser::TabularFormat::from_extension)
                .map(crate::parser::TabularFormat::delimiter)
                .unwrap_or(delimiter);

            match load_project_file(project, &path, cx).await {
                Ok(other_buffer) => {
                    workspace
                        .update_in(cx, |workspace, window, cx| {
                            let view = cx.new(|cx| {
                                Self::from_files(
                                    format!("{name} ↔ {other_name}").into(),
                                    other_name.clone(),
                                    name.clone().into(),
                                    LeftSource::File(other_buffer),
                                    buffer,
                                    other_delimiter,
                                    delimiter,
                                    first_row_is_header,
                                    cx,
                                )
                            });
                            workspace.add_item_to_active_pane(
                                Box::new(view),
                                None,
                                true,
                                window,
                                cx,
                            );
                        })
                        .ok();
                }
                Err(error) => {
                    log::error!("failed to read {path:?} for a table comparison: {error}");
                    grid.update(cx, |grid, cx| {
                        grid.set_status_message("選んだファイルを読み込めませんでした", cx);
                    });
                }
            }
        })
        .detach();
    }

    /// Compares two blocks of cells inside one grid.
    ///
    /// A selection two columns wide compares the two columns, a selection two
    /// rows tall in one column compares the two cells, and a single cell is
    /// compared against the clipboard. Anything else has no two things in it to
    /// compare, and says so.
    pub(crate) fn diff_selected_cells(
        workspace: &mut Workspace,
        grid: Entity<TabularDataPreviewPane>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let clipboard = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default();
        let Some(comparison) = grid.update(cx, |grid, cx| {
            let comparison = cell_comparison(grid, &clipboard);
            if comparison.is_none() {
                grid.set_status_message(
                    "Ctrl で 2 つの範囲を選ぶとその 2 つを比べます (2 列か 2 行でも、\
                     1 つのセルはクリップボードと比べます)",
                    cx,
                );
            }
            comparison
        }) else {
            return;
        };

        let view = cx.new(|cx| {
            Self::from_cells(
                comparison.title,
                comparison.left_title,
                comparison.right_title,
                grid.downgrade(),
                comparison.left,
                comparison.right,
                cx,
            )
        });
        workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
    }

    ///// Construction /////

    #[allow(clippy::too_many_arguments)]
    fn from_files(
        title: SharedString,
        left_title: SharedString,
        right_title: SharedString,
        left: LeftSource,
        buffer: Entity<Buffer>,
        left_delimiter: char,
        right_delimiter: char,
        first_row_is_header: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions = vec![cx.subscribe(&buffer, |this, _buffer, event, cx| {
            if matches!(
                event,
                BufferEvent::Edited { .. } | BufferEvent::Reloaded | BufferEvent::Saved
            ) {
                this.recompute(true, cx);
            }
        })];
        match &left {
            LeftSource::Head(diff) => {
                subscriptions.push(cx.subscribe(diff, |this, _diff, event, cx| {
                    let base_text_changed = match event {
                        BufferDiffEvent::BaseTextChanged => true,
                        BufferDiffEvent::DiffChanged(changed) => changed.base_text_changed,
                    };
                    if base_text_changed {
                        this.recompute(true, cx);
                    }
                }));
            }
            LeftSource::File(other) => {
                subscriptions.push(cx.subscribe(other, |this, _buffer, event, cx| {
                    if matches!(
                        event,
                        BufferEvent::Edited { .. } | BufferEvent::Reloaded | BufferEvent::Saved
                    ) {
                        this.recompute(true, cx);
                    }
                }));
            }
        }

        let mut view = Self::empty(title, left_title, right_title, cx);
        view.first_row_is_header = first_row_is_header;
        view.source = DiffSource::Files {
            left,
            left_text: SharedString::default(),
            buffer,
            left_delimiter,
            right_delimiter,
        };
        view._subscriptions = subscriptions;
        view.recompute(false, cx);
        view
    }

    #[allow(clippy::too_many_arguments)]
    fn from_cells(
        title: SharedString,
        left_title: SharedString,
        right_title: SharedString,
        grid: WeakEntity<TabularDataPreviewPane>,
        left: CellComparisonSide,
        right: CellComparisonSide,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::empty(title, left_title, right_title, cx);
        view.source = DiffSource::Cells {
            grid,
            left: left.block,
            right: right.block,
            left_rows: left.rows,
            right_rows: right.rows,
        };
        view.left_taken_at = Some(Local::now());
        view.recompute(false, cx);
        view
    }

    fn empty(
        title: SharedString,
        left_title: SharedString,
        right_title: SharedString,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            title,
            left_title,
            right_title,
            source: DiffSource::Cells {
                grid: WeakEntity::new_invalid(),
                left: CellBlock::Clipboard,
                right: CellBlock::Clipboard,
                left_rows: Vec::new(),
                right_rows: Vec::new(),
            },
            first_row_is_header: false,
            left: DiffSide::default(),
            right: DiffSide::default(),
            diff: TableDiff::compute(&DiffSide::default(), &DiffSide::default()),
            left_taken_at: None,
            left_is_stale: false,
            is_computing: false,
            column_widths: Vec::new(),
            row_number_width: px(MINIMUM_COLUMN_WIDTH),
            shown_rows: None,
            current_change: None,
            row_height: cell_line_height(cx) + CELL_FRAME_HEIGHT,
            list_scroll: UniformListScrollHandle::new(),
            horizontal_scroll: ScrollHandle::new(),
            recompute_task: None,
            _subscriptions: Vec::new(),
        }
    }

    /// The two sides as they are now, copied so that the alignment can run off
    /// the UI thread.
    ///
    /// Reading the buffer copies the whole file, so this is taken once the edits
    /// have settled rather than when the event arrives: an edit per keystroke
    /// would otherwise copy a large file on the UI thread on every one of them,
    /// which is exactly what the debounce is there to avoid.
    fn diff_input(&mut self, cx: &App) -> DiffInput {
        let first_row_is_header = self.first_row_is_header;
        let mut left_was_read = true;
        let input = match &mut self.source {
            DiffSource::Files {
                left,
                left_text,
                buffer,
                left_delimiter,
                right_delimiter,
            } => {
                match left.read(cx) {
                    Some(text) => *left_text = text,
                    None => left_was_read = false,
                }
                DiffInput::Texts {
                    left_text: left_text.clone(),
                    right_text: buffer.read(cx).text().into(),
                    left_delimiter: *left_delimiter,
                    right_delimiter: *right_delimiter,
                    first_row_is_header,
                }
            }
            DiffSource::Cells {
                left_rows,
                right_rows,
                ..
            } => DiffInput::Rows {
                left_rows: left_rows.clone(),
                right_rows: right_rows.clone(),
                first_row_is_header,
            },
        };
        if matches!(self.source, DiffSource::Files { .. }) {
            self.left_is_stale = !left_was_read;
            if left_was_read {
                self.left_taken_at = Some(Local::now());
            }
        }
        input
    }

    /// Takes both sides again from where they came from.
    ///
    /// The two file sides follow on their own, so this only does anything they
    /// would not have done by themselves for a comparison of cells, which is a
    /// place in a grid rather than a thing that can announce that it changed.
    fn recompare(&mut self, cx: &mut Context<Self>) {
        let blocks = match &self.source {
            DiffSource::Cells {
                grid, left, right, ..
            } => Some((grid.clone(), *left, *right)),
            DiffSource::Files { .. } => None,
        };
        if let Some((grid, left_block, right_block)) = blocks {
            // The reason is already on the toolbar and in the button's tooltip,
            // so a comparison that cannot be taken again leaves what it shows
            // alone rather than emptying itself.
            let Some(grid) = grid.upgrade() else {
                return;
            };
            let clipboard = cx
                .read_from_clipboard()
                .and_then(|item| item.text())
                .unwrap_or_default();
            let (left, right) = grid.read_with(cx, |grid, _cx| {
                (
                    read_cell_block(grid, left_block, &clipboard),
                    read_cell_block(grid, right_block, &clipboard),
                )
            });
            if let DiffSource::Cells {
                left_rows,
                right_rows,
                ..
            } = &mut self.source
            {
                *left_rows = left;
                *right_rows = right;
            }
            self.left_taken_at = Some(Local::now());
        }
        self.recompute(false, cx);
    }

    fn toggle_first_row_is_header(&mut self, cx: &mut Context<Self>) {
        self.first_row_is_header = !self.first_row_is_header;
        self.recompute(false, cx);
    }

    /// Whether the comparison can still be taken again, which a comparison of
    /// cells stops being once the grid it read them out of is gone.
    fn can_recompare(&self) -> bool {
        match &self.source {
            DiffSource::Files { .. } => true,
            DiffSource::Cells { grid, .. } => grid.upgrade().is_some(),
        }
    }

    fn recompute(&mut self, debounce: bool, cx: &mut Context<Self>) {
        self.is_computing = true;
        cx.notify();
        self.recompute_task = Some(cx.spawn(async move |this, cx| {
            if debounce {
                cx.background_executor().timer(RECOMPUTE_DEBOUNCE).await;
            }
            let Ok(input) = this.update(cx, |this, cx| this.diff_input(cx)) else {
                return;
            };
            let computed = cx.background_spawn(async move { input.compute() }).await;
            this.update(cx, |this, cx| {
                let (left, right, diff) = computed;
                this.column_widths = column_widths(&left, &right, diff.column_count);
                this.row_number_width = row_number_width(&left, &right);
                this.left = left;
                this.right = right;
                this.diff = diff;
                this.is_computing = false;
                this.refresh_shown_rows();
                cx.notify();
            })
            .ok();
        }));
    }

    ///// Rows on screen /////

    fn refresh_shown_rows(&mut self) {
        if self.shown_rows.is_some() {
            self.shown_rows = Some(self.diff.changed_row_indices());
        }
        let stops = self.diff.change_stops();
        self.current_change = self
            .current_change
            .filter(|_| !stops.is_empty())
            .map(|change| change.min(stops.len().saturating_sub(1)));
    }

    fn shown_row_count(&self) -> usize {
        match &self.shown_rows {
            Some(rows) => rows.len(),
            None => self.diff.rows.len(),
        }
    }

    /// The aligned row a position on screen stands for.
    fn row_at(&self, shown_index: usize) -> Option<usize> {
        match &self.shown_rows {
            Some(rows) => rows.get(shown_index).copied(),
            None => (shown_index < self.diff.rows.len()).then_some(shown_index),
        }
    }

    /// Where an aligned row sits on screen, which is what scrolling to a change
    /// needs and what the filter makes non-trivial.
    fn shown_index_of(&self, row: usize) -> usize {
        match &self.shown_rows {
            Some(rows) => rows.binary_search(&row).unwrap_or_else(|index| index),
            None => row,
        }
    }

    fn toggle_only_differences(&mut self, cx: &mut Context<Self>) {
        self.shown_rows = match self.shown_rows {
            Some(_) => None,
            None => Some(self.diff.changed_row_indices()),
        };
        cx.notify();
    }

    fn go_to_change(&mut self, forward: bool, cx: &mut Context<Self>) {
        let stops = self.diff.change_stops();
        if stops.is_empty() {
            return;
        }
        let change = match (self.current_change, forward) {
            (None, true) => 0,
            (None, false) => stops.len() - 1,
            (Some(current), true) => (current + 1).min(stops.len() - 1),
            (Some(current), false) => current.saturating_sub(1),
        };
        self.current_change = Some(change);
        match stops.get(change) {
            // The header has a strip of its own above the list and is always on
            // screen, so stopping at it means putting the top of the table under
            // it rather than scrolling to a row.
            Some(ChangeStop::Header) => self.list_scroll.scroll_to_item(0, ScrollStrategy::Top),
            Some(&ChangeStop::Row(row)) => self
                .list_scroll
                .scroll_to_item(self.shown_index_of(row), ScrollStrategy::Center),
            None => {}
        }
        cx.notify();
    }

    /// Which stop of how many "previous" and "next" are sitting on, which is the
    /// only sign that a step landed on the header rather than doing nothing.
    fn change_position(&self) -> Option<(usize, usize)> {
        // Counting the stops walks every aligned row, of which there can be
        // MAX_DIFF_ROWS, and the toolbar this is for is drawn on every frame.
        let current = self.current_change?;
        let stops = self.diff.change_stops().len();
        (current < stops).then_some((current + 1, stops))
    }
}

///// Rendering /////

impl Render for TableDiffView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.row_height = cell_line_height(cx) + CELL_FRAME_HEIGHT;
        let colors = cx.theme().colors();

        v_flex()
            .key_context("TabularDataDiff")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(colors.editor_background)
            .font_buffer(cx)
            .text_size(ThemeSettings::get_global(cx).buffer_font_size(cx))
            .on_action(cx.listener(|this, _: &RecomputeDiff, _window, cx| {
                this.recompare(cx);
            }))
            .child(self.render_toolbar(cx))
            .child(self.render_pane_titles(cx))
            .child(self.render_header_row(cx))
            .child(self.render_body(window, cx))
    }
}

impl TableDiffView {
    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let changes = self.diff.change_count();
        let counts = if changes == 0 {
            "差分なし".to_owned()
        } else {
            format!(
                "{changes} 件の差分 (追加 {} / 削除 {} / 変更 {})",
                self.diff.added_rows, self.diff.removed_rows, self.diff.modified_rows
            )
        };

        h_flex()
            .gap_3()
            .p_1()
            .flex_none()
            .bg(colors.surface_background)
            .border_b_1()
            .border_color(colors.border)
            .text_ui(cx)
            .font_ui(cx)
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        IconButton::new(
                            ElementId::Name("tabular-diff-previous".into()),
                            IconName::ArrowUp,
                        )
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("前の差分"))
                        .on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.go_to_change(false, cx);
                            },
                        )),
                    )
                    .child(
                        IconButton::new(
                            ElementId::Name("tabular-diff-next".into()),
                            IconName::ArrowDown,
                        )
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("次の差分"))
                        .on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.go_to_change(true, cx);
                            },
                        )),
                    ),
            )
            .child(
                Checkbox::new(
                    ElementId::Name("tabular-diff-only-changes".into()),
                    if self.shown_rows.is_some() {
                        ToggleState::Selected
                    } else {
                        ToggleState::Unselected
                    },
                )
                .label("差分のある行だけ表示")
                .on_click(cx.listener(|this, _state, _window, cx| {
                    this.toggle_only_differences(cx);
                })),
            )
            .child(
                Checkbox::new(
                    ElementId::Name("tabular-diff-header-row".into()),
                    if self.first_row_is_header {
                        ToggleState::Selected
                    } else {
                        ToggleState::Unselected
                    },
                )
                .label("先頭行を見出し")
                .on_click(cx.listener(|this, _state, _window, cx| {
                    this.toggle_first_row_is_header(cx);
                })),
            )
            .child(self.render_recompare_button(cx))
            .child(div().flex_1())
            .when_some(self.change_position(), |this, (current, total)| {
                this.child(
                    Label::new(format!("{current} / {total} 箇所目"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .child(
                Label::new(counts)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .when_some(self.left_stale_reason(), |this, reason| {
                this.child(
                    Label::new(reason)
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                )
            })
            .when(!self.can_recompare(), |this| {
                this.child(
                    Label::new("元の表のタブが閉じているため、この比較は取り直せません")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .when(self.diff.truncated, |this| {
                this.child(
                    Label::new(format!(
                        "先頭 {} 行だけを比較しました",
                        crate::grid::grouped_digits(crate::table_diff::MAX_DIFF_ROWS)
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Warning),
                )
            })
            .when(self.is_computing, |this| {
                this.child(
                    Label::new("比較中…")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .into_any_element()
    }

    fn render_recompare_button(&self, cx: &mut Context<Self>) -> AnyElement {
        let can_recompare = self.can_recompare();
        Button::new(ElementId::Name("tabular-diff-recompare".into()), "再比較")
            .style(ButtonStyle::Subtle)
            .size(ButtonSize::Compact)
            .start_icon(Icon::new(IconName::RotateCcw).size(IconSize::Small))
            .disabled(!can_recompare)
            .tooltip(Tooltip::text(if can_recompare {
                "両側をもう一度読み直して比べます"
            } else {
                "元の表のタブが閉じているため取り直せません"
            }))
            .on_click(cx.listener(|this, _event, _window, cx| {
                this.recompare(cx);
            }))
            .into_any_element()
    }

    /// What went missing under the left pane, when it is showing the last
    /// version it could read rather than the one it is comparing against.
    ///
    /// The two sides go stale for different reasons and a comparison that only
    /// said it was out of date would leave the user looking for the wrong thing.
    fn left_stale_reason(&self) -> Option<SharedString> {
        if !self.left_is_stale {
            return None;
        }
        let reason = match &self.source {
            DiffSource::Files {
                left: LeftSource::Head(_),
                ..
            } => format!(
                "{} にこのファイルがなくなりました (最後に読めた内容を表示しています)",
                self.left_title
            ),
            DiffSource::Files {
                left: LeftSource::File(_),
                ..
            } => format!(
                "{} が見つかりません (最後に読めた内容を表示しています)",
                self.left_title
            ),
            DiffSource::Cells { .. } => return None,
        };
        Some(reason.into())
    }

    /// The left pane's title with when its contents were read, which is what
    /// says whether "HEAD" means the commit that is checked out now.
    fn left_pane_title(&self) -> SharedString {
        match self.left_taken_at {
            Some(taken_at) => {
                format!("{} ({} 取得)", self.left_title, taken_at.format("%H:%M")).into()
            }
            None => self.left_title.clone(),
        }
    }

    fn render_pane_titles(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let title = |text: SharedString| {
            div()
                .flex_1()
                .min_w_0()
                .px_2()
                .py_0p5()
                .overflow_hidden()
                .child(
                    Label::new(text)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .single_line(),
                )
        };
        h_flex()
            .flex_none()
            .w_full()
            .bg(colors.surface_background)
            .font_ui(cx)
            .child(title(self.left_pane_title()))
            .child(self.render_divider(cx))
            .child(title(self.right_title.clone()))
            .into_any_element()
    }

    /// The header row of the two tables, or the column letters when the grid is
    /// not reading the first line as a header - either way the strip that names
    /// the columns and stays put while the body scrolls.
    fn render_header_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let labels: Vec<SharedString> = (0..self.diff.column_count)
            .map(|column| SharedString::from(column_label(column)))
            .collect();
        let (kind, changed_columns) = match &self.diff.header {
            Some(header) => (header.kind, header.changed_columns.as_slice()),
            None => (DiffRowKind::Unchanged, &[] as &[usize]),
        };
        let has_header = self.diff.header.is_some();
        let left_cells = self.left.header.as_deref().unwrap_or(&labels);
        let right_cells = self.right.header.as_deref().unwrap_or(&labels);
        // A side that has no header at all while the other one does leaves the
        // strip blank, number included: a row number over an empty row would
        // claim the file has a line there.
        let left_present = !has_header || self.left.header.is_some();
        let right_present = !has_header || self.right.header.is_some();

        h_flex()
            .flex_none()
            .w_full()
            .h(self.row_height)
            .items_stretch()
            .bg(colors.title_bar_background)
            .border_b_1()
            .border_color(colors.border)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .debug_selector(|| DIFF_LEFT_PANE_DEBUG_SELECTOR.to_owned())
                    .child(self.render_pane(
                        PaneSide::Left,
                        PaneRow {
                            kind,
                            changed_columns,
                            cells: left_cells,
                            other_cells: right_cells,
                            row_number: (has_header && left_present).then_some(1),
                            present: left_present,
                        },
                        HEADER_ELEMENT_INDEX,
                        cx,
                    )),
            )
            .child(self.render_divider(cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .debug_selector(|| DIFF_RIGHT_PANE_DEBUG_SELECTOR.to_owned())
                    .child(self.render_pane(
                        PaneSide::Right,
                        PaneRow {
                            kind,
                            changed_columns,
                            cells: right_cells,
                            other_cells: left_cells,
                            row_number: (has_header && right_present).then_some(1),
                            present: right_present,
                        },
                        HEADER_ELEMENT_INDEX,
                        cx,
                    )),
            )
            .into_any_element()
    }

    fn render_body(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.diff.column_count == 0 {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(cx.theme().colors().text_muted)
                .child(if self.is_computing {
                    "比較中…"
                } else {
                    "比べるものがありません"
                })
                .into_any_element();
        }

        let row_count = self.shown_row_count();
        div()
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(
                uniform_list(
                    "tabular-diff-rows",
                    row_count,
                    cx.processor(|this, range: Range<usize>, _window, cx| {
                        range
                            .map(|shown_index| this.render_aligned_row(shown_index, cx))
                            .collect()
                    }),
                )
                .size_full()
                .track_scroll(&self.list_scroll),
            )
            .vertical_scrollbar_for(&self.list_scroll, window, cx)
            .child(self.render_horizontal_scrollbars(window, cx))
            .into_any_element()
    }

    /// One horizontal scrollbar under each pane. The two track the one shared
    /// offset, so either of them moves both panes; they are laid out to match a
    /// pane's scrolling area exactly, which is what makes the thumb the right
    /// size.
    fn render_horizontal_scrollbars(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .absolute()
            .bottom_0()
            .left_0()
            .right_0()
            .h(HORIZONTAL_SCROLLBAR_HEIGHT)
            .child(self.render_horizontal_scrollbar("tabular-diff-hscroll-left", window, cx))
            .child(div().w(px(1.)).flex_none())
            .child(self.render_horizontal_scrollbar("tabular-diff-hscroll-right", window, cx))
            .into_any_element()
    }

    fn render_horizontal_scrollbar(
        &self,
        id: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(div().w(self.row_number_width).flex_none())
            .child(
                div().flex_1().min_w_0().h_full().custom_scrollbars(
                    Scrollbars::new(ScrollAxes::Horizontal)
                        .id(ElementId::Name(id.into()))
                        .tracked_scroll_handle(&self.horizontal_scroll),
                    window,
                    cx,
                ),
            )
            .into_any_element()
    }

    fn render_divider(&self, cx: &App) -> AnyElement {
        div()
            .w(px(1.))
            .flex_none()
            .h_full()
            .bg(cx.theme().colors().border)
            .into_any_element()
    }

    fn render_aligned_row(&self, shown_index: usize, cx: &Context<Self>) -> AnyElement {
        let row = self
            .row_at(shown_index)
            .and_then(|row| self.diff.rows.get(row));
        let Some(row) = row else {
            return div().h(self.row_height).w_full().into_any_element();
        };

        h_flex()
            .h(self.row_height)
            .w_full()
            .items_stretch()
            .child(self.render_pane(
                PaneSide::Left,
                self.pane_row(PaneSide::Left, row),
                shown_index,
                cx,
            ))
            .child(self.render_divider(cx))
            .child(self.render_pane(
                PaneSide::Right,
                self.pane_row(PaneSide::Right, row),
                shown_index,
                cx,
            ))
            .into_any_element()
    }

    fn pane_row<'a>(&'a self, side: PaneSide, row: &'a DiffRow) -> PaneRow<'a> {
        let (this_side, this_row, other_side, other_row) = match side {
            PaneSide::Left => (&self.left, row.left_row, &self.right, row.right_row),
            PaneSide::Right => (&self.right, row.right_row, &self.left, row.left_row),
        };
        PaneRow {
            kind: row.kind,
            changed_columns: row.changed_columns.as_slice(),
            cells: this_row.map(|row| this_side.row(row)).unwrap_or(&[]),
            other_cells: other_row.map(|row| other_side.row(row)).unwrap_or(&[]),
            row_number: this_row.map(|row| this_side.row_number(row)),
            present: this_row.is_some(),
        }
    }

    fn column_width(&self, column: usize) -> Pixels {
        self.column_widths
            .get(column)
            .copied()
            .unwrap_or(px(MINIMUM_COLUMN_WIDTH))
    }

    /// Width of one pane's cells laid end to end.
    ///
    /// Set on the row of cells explicitly because the pane that scrolls it is a
    /// block container: a row left to size itself there fills the pane instead
    /// of overflowing it, and an element whose content is no wider than itself
    /// has nothing to scroll - which, with one handle shared by every pane,
    /// would pin all of them at the first column.
    fn content_width(&self) -> Pixels {
        (0..self.diff.column_count).fold(px(0.), |total, column| total + self.column_width(column))
    }

    fn render_pane(
        &self,
        side: PaneSide,
        row: PaneRow<'_>,
        element_index: usize,
        cx: &App,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let status = cx.theme().status();
        let background = if !row.present {
            Some(colors.surface_background)
        } else {
            match (row.kind, side) {
                (DiffRowKind::Added, PaneSide::Right) => Some(status.created_background),
                (DiffRowKind::Removed, PaneSide::Left) => Some(status.deleted_background),
                _ => None,
            }
        };

        let cells = (0..self.diff.column_count).map(|column| {
            let width = self.column_width(column);
            let value = if row.present {
                cell_at(row.cells, column)
            } else {
                ""
            };
            let is_changed = row.kind == DiffRowKind::Modified && row.is_changed(column);
            div()
                .w(width)
                .flex_none()
                .h_full()
                .px_1()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .border_r_1()
                .border_color(colors.border_variant)
                .when(is_changed, |this| this.bg(status.modified_background))
                .child(if is_changed {
                    let other = cell_at(row.other_cells, column);
                    highlighted_cell(side, value, other, cx)
                } else {
                    SharedString::from(value.to_owned()).into_any_element()
                })
        });

        let element_id = ElementId::NamedInteger(
            match side {
                PaneSide::Left => SharedString::new_static("tabular-diff-left"),
                PaneSide::Right => SharedString::new_static("tabular-diff-right"),
            },
            element_index as u64,
        );

        h_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .items_stretch()
            .when_some(background, |this, background| this.bg(background))
            .child(
                div()
                    .w(self.row_number_width)
                    .flex_none()
                    .h_full()
                    .px_1()
                    .overflow_hidden()
                    .text_color(colors.editor_line_number)
                    .child(
                        row.row_number
                            .map(|number| SharedString::from(number.to_string()))
                            .unwrap_or_default(),
                    ),
            )
            .child(
                div()
                    .id(element_id)
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_x_scroll()
                    .restrict_scroll_to_axis()
                    .track_scroll(&self.horizontal_scroll)
                    .child(
                        h_flex()
                            .w(self.content_width())
                            .h_full()
                            .items_stretch()
                            .children(cells),
                    ),
            )
            .into_any_element()
    }
}

/// One aligned row as one pane has to draw it.
struct PaneRow<'a> {
    kind: DiffRowKind,
    changed_columns: &'a [usize],
    cells: &'a [SharedString],
    /// The other pane's cells for the same row, which is what the character
    /// level highlighting inside a changed cell is measured against.
    other_cells: &'a [SharedString],
    row_number: Option<usize>,
    /// Whether this side has a row here at all. A row only one side has leaves
    /// the other blank, the way a two pane comparison always has.
    present: bool,
}

impl PaneRow<'_> {
    fn is_changed(&self, column: usize) -> bool {
        self.changed_columns.contains(&column)
    }
}

/// A changed cell with the characters that differ picked out, so that a long
/// value whose middle changed does not leave the user comparing two strings by
/// eye.
fn highlighted_cell(side: PaneSide, value: &str, other: &str, cx: &App) -> AnyElement {
    let CellCharDiff { left, right } = match side {
        PaneSide::Left => cell_char_diff(value, other),
        PaneSide::Right => cell_char_diff(other, value),
    };
    let ranges = match side {
        PaneSide::Left => left,
        PaneSide::Right => right,
    };
    let status = cx.theme().status();
    let background = match side {
        PaneSide::Left => status.deleted_background,
        PaneSide::Right => status.created_background,
    };
    let highlight = HighlightStyle {
        background_color: Some(background),
        ..HighlightStyle::default()
    };
    StyledText::new(value.to_owned())
        .with_highlights(ranges.into_iter().map(|range| (range, highlight)))
        .into_any_element()
}

fn column_widths(left: &DiffSide, right: &DiffSide, column_count: usize) -> Vec<Pixels> {
    (0..column_count)
        .map(|column| {
            let widest = [left, right]
                .into_iter()
                .flat_map(|side| {
                    side.header.iter().map(Vec::as_slice).chain(
                        side.rows
                            .iter()
                            .take(COLUMN_WIDTH_SAMPLE_ROWS)
                            .map(Vec::as_slice),
                    )
                })
                .map(|row| {
                    cell_at(row, column)
                        .lines()
                        .map(display_width)
                        .max()
                        .unwrap_or(0)
                })
                .max()
                .unwrap_or(0);
            px(
                (widest as f32 * COLUMN_WIDTH_PER_CHARACTER + COLUMN_WIDTH_PADDING)
                    .clamp(MINIMUM_COLUMN_WIDTH, MAXIMUM_COLUMN_WIDTH),
            )
        })
        .collect()
}

/// Wide enough for the largest number either pane puts in front of a row. That
/// is the last row's line rather than the row count: one quoted line break puts
/// the two apart.
fn row_number_width(left: &DiffSide, right: &DiffSide) -> Pixels {
    let last = left.last_row_number().max(right.last_row_number()).max(1);
    let digits = last.to_string().len().max(2);
    px(digits as f32 * COLUMN_WIDTH_PER_CHARACTER + ROW_NUMBER_PADDING)
}

fn file_name(buffer: &Entity<Buffer>, cx: &App) -> String {
    buffer
        .read(cx)
        .file()
        .and_then(|file| file.path().file_name().map(ToOwned::to_owned))
        .unwrap_or_else(|| "無題".to_owned())
}

/// The diff holding the committed version, or `None` when there is no committed
/// version to compare against.
///
/// The diff itself is what is kept rather than the text it holds right now: the
/// project drops it as soon as nothing holds it, and holding it is what lets the
/// left pane follow the commit that is checked out.
async fn load_committed_diff(
    committed: Task<Result<Entity<BufferDiff>>>,
    cx: &mut gpui::AsyncWindowContext,
) -> Option<Entity<BufferDiff>> {
    let diff = committed
        .await
        .inspect_err(|error| log::error!("failed to load the committed table: {error}"))
        .ok()?;
    let has_committed_version = diff.read_with(cx, |diff, cx| diff.base_text_string(cx).is_some());
    has_committed_version.then_some(diff)
}

/// The other file, kept open as a buffer so that the comparison follows edits to
/// it the same way it follows edits to the grid's own file.
async fn load_project_file(
    project: Entity<Project>,
    path: &std::path::Path,
    cx: &mut gpui::AsyncApp,
) -> Result<Entity<Buffer>> {
    let project_path = cx
        .update(|cx| Workspace::project_path_for_path(project.clone(), path, false, cx))
        .await?
        .1;
    project
        .update(cx, |project, cx| project.open_buffer(project_path, cx))
        .await
}

/// What a comparison of two blocks of cells inside one grid is made of.
struct CellComparison {
    title: SharedString,
    left_title: SharedString,
    right_title: SharedString,
    left: CellComparisonSide,
    right: CellComparisonSide,
}

/// One side of it: the values as they were read, and where they were read from
/// so that "再比較" can read them again.
struct CellComparisonSide {
    block: CellBlock,
    rows: Vec<Vec<SharedString>>,
}

impl CellComparisonSide {
    fn new(grid: &TabularDataPreviewPane, block: CellBlock, clipboard: &str) -> Self {
        Self {
            rows: read_cell_block(grid, block, clipboard),
            block,
        }
    }

    fn rect(grid: &TabularDataPreviewPane, rect: GridRect) -> Self {
        Self::new(grid, CellBlock::Rect(rect), "")
    }

    /// One cell of the grid, which is a rectangle of one row and one column.
    fn cell(grid: &TabularDataPreviewPane, row: usize, column: usize) -> Self {
        Self::rect(
            grid,
            GridRect {
                first_row: row,
                last_row: row,
                first_col: column,
                last_col: column,
            },
        )
    }
}

/// Reads the grid's selection as two things to compare.
///
/// Kept a free function over the grid's public reads so that it can be tested
/// without a window, and so that it is the one place to extend once a selection
/// can hold more than one range.
fn cell_comparison(grid: &TabularDataPreviewPane, clipboard: &str) -> Option<CellComparison> {
    let rect = grid.selection_rect();
    let single_column = rect.first_col == rect.last_col;
    let column_pair = rect.first_col + 1 == rect.last_col;
    // Rows are named the way the grid's own address in the toolbar names them,
    // which counts the header as line 1 so that the name is the line of the
    // file. A title pointing at a different line than the toolbar does would be
    // worse than no title at all.
    let row_offset = usize::from(grid.has_header_row());

    // Two blocks picked with Ctrl say outright which two things to compare, so
    // they win over every guess made from the shape of one block. Blocks of
    // different shapes are compared as they are: the row matching below already
    // pads whichever side runs out first.
    let blocks = grid.selection_rects();
    if let [left, right] = blocks.as_slice() {
        let address_of = |block: &GridRect| {
            SharedString::from(format!(
                "{}{}:{}{}",
                column_label(block.first_col),
                block.first_row + row_offset,
                column_label(block.last_col),
                block.last_row + row_offset
            ))
        };
        let (left_title, right_title) = (address_of(left), address_of(right));
        return Some(CellComparison {
            title: format!("セルの比較 ({left_title} ↔ {right_title})").into(),
            left: CellComparisonSide::rect(grid, *left),
            right: CellComparisonSide::rect(grid, *right),
            left_title,
            right_title,
        });
    }

    let column_of = |column: usize| SharedString::from(format!("列 {}", column_label(column)));
    let row_of = |row: usize| SharedString::from(format!("行 {row}"));
    let column_down = |column: usize| GridRect {
        first_row: rect.first_row,
        last_row: rect.last_row,
        first_col: column,
        last_col: column,
    };

    // Two columns side by side is the comparison the user is most likely after
    // in a spreadsheet, so a block that is both two columns wide and two rows
    // tall is read as two columns rather than as two rows.
    if column_pair {
        return Some(CellComparison {
            title: format!(
                "セルの比較 ({} ↔ {})",
                column_label(rect.first_col),
                column_label(rect.last_col)
            )
            .into(),
            left_title: column_of(rect.first_col),
            right_title: column_of(rect.last_col),
            left: CellComparisonSide::rect(grid, column_down(rect.first_col)),
            right: CellComparisonSide::rect(grid, column_down(rect.last_col)),
        });
    }

    if single_column && rect.first_row != rect.last_row {
        let column = rect.first_col;
        let (first_number, last_number) = (rect.first_row + row_offset, rect.last_row + row_offset);
        return Some(CellComparison {
            title: format!(
                "セルの比較 ({}{} ↔ {}{})",
                column_label(column),
                first_number,
                column_label(column),
                last_number
            )
            .into(),
            left_title: row_of(first_number),
            right_title: row_of(last_number),
            left: CellComparisonSide::cell(grid, rect.first_row, column),
            right: CellComparisonSide::cell(grid, rect.last_row, column),
        });
    }

    if single_column && rect.first_row == rect.last_row {
        if clipboard.is_empty() {
            return None;
        }
        let address = grid.cursor_address();
        return Some(CellComparison {
            title: format!("セルの比較 (クリップボード ↔ {address})").into(),
            left_title: "クリップボード".into(),
            right_title: SharedString::from(address),
            left: CellComparisonSide::new(grid, CellBlock::Clipboard, clipboard),
            right: CellComparisonSide::cell(grid, rect.first_row, rect.first_col),
        });
    }

    None
}

impl EventEmitter<()> for TableDiffView {}

impl Focusable for TableDiffView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for TableDiffView {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, _cx: &App) -> AnyElement {
        Label::new(self.title.clone())
            .single_line()
            .color(params.text_color())
            .into_any_element()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::DiffSplit))
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(format!("{} と {} の差分", self.left_title, self.right_title).into())
    }
}

///// The grid's own entry point /////

impl TabularDataPreviewPane {
    /// The toolbar's "差分" menu.
    pub(crate) fn render_diff_menu(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let dispatch_context = self.grid.cell_editor.focus_handle(cx);
        let menu = ContextMenu::build(window, cx, move |menu, _window, _cx| {
            menu.context(dispatch_context)
                .action("HEAD と比較", DiffWithHead.boxed_clone())
                .action("ファイルと比較…", DiffWithFile.boxed_clone())
                .action("選択したセルを比較", DiffSelectedCells.boxed_clone())
        });

        DropdownMenu::new(ElementId::Name("tabular-diff-menu".into()), "差分", menu)
            .trigger_size(ButtonSize::Compact)
            .trigger_tooltip(Tooltip::text("この表を別の版や別のファイルと見比べます"))
            .into_any_element()
    }

    /// Registers the three comparison actions on the workspace.
    ///
    /// They sit on the workspace rather than on the grid because opening a tab
    /// needs the workspace anyway, and because loading a committed version or
    /// reading another file is asynchronous.
    pub(crate) fn register_diff_actions(workspace: &mut Workspace) {
        workspace.register_action_renderer(|div, _, _, cx| {
            div.on_action(cx.listener(|workspace, _: &DiffWithHead, window, cx| {
                if let Some(grid) = active_grid(workspace, cx) {
                    TableDiffView::diff_with_head(workspace, grid, window, cx);
                }
            }))
            .on_action(cx.listener(|workspace, _: &DiffWithFile, window, cx| {
                if let Some(grid) = active_grid(workspace, cx) {
                    TableDiffView::diff_with_file(workspace, grid, window, cx);
                }
            }))
            .on_action(cx.listener(
                |workspace, _: &DiffSelectedCells, window, cx| {
                    if let Some(grid) = active_grid(workspace, cx) {
                        TableDiffView::diff_selected_cells(workspace, grid, window, cx);
                    }
                },
            ))
        });
    }
}

fn active_grid(
    workspace: &Workspace,
    cx: &mut Context<Workspace>,
) -> Option<Entity<TabularDataPreviewPane>> {
    workspace
        .active_item(cx)
        .and_then(|item| item.downcast::<TabularDataPreviewPane>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        grid_editing::tests::open_grid_in_workspace,
        table_diff::{DiffRowKind, DiffSide},
    };
    use gpui::{TestAppContext, VisualTestContext};
    use workspace::SaveIntent;

    /// A comparison of two scratch files, which is the shape every comparison
    /// but the one of cells has: two texts that each follow their own buffer.
    fn diff_view(left: &str, right: &str, cx: &mut VisualTestContext) -> Entity<TableDiffView> {
        let (view, _left_buffer, _right_buffer) = diff_view_over_buffers(left, right, cx);
        view
    }

    fn diff_view_over_buffers(
        left: &str,
        right: &str,
        cx: &mut VisualTestContext,
    ) -> (Entity<TableDiffView>, Entity<Buffer>, Entity<Buffer>) {
        let left = left.to_owned();
        let right = right.to_owned();
        cx.update(|_window, cx| {
            let left_buffer = cx.new(|cx| Buffer::local(left, cx));
            let right_buffer = cx.new(|cx| Buffer::local(right, cx));
            let view = cx.new(|cx| {
                TableDiffView::from_files(
                    "比較".into(),
                    "左".into(),
                    "右".into(),
                    LeftSource::File(left_buffer.clone()),
                    right_buffer.clone(),
                    ',',
                    ',',
                    true,
                    cx,
                )
            });
            (view, left_buffer, right_buffer)
        })
    }

    fn cell_diff(comparison: &CellComparison) -> TableDiff {
        TableDiff::compute(
            &DiffSide::from_rows(comparison.left.rows.clone(), false),
            &DiffSide::from_rows(comparison.right.rows.clone(), false),
        )
    }

    /// The two panes have to hold the same number of lines, or the rows of one
    /// file would sit beside unrelated rows of the other.
    #[gpui::test]
    async fn the_two_panes_show_the_same_number_of_rows(cx: &mut TestAppContext) {
        let (_workspace, _pane, _editor, mut cx) =
            open_grid_in_workspace("name,value\na,1\nb,2\nc,3", cx).await;
        let view = diff_view(
            "name,value\na,1\nb,2\nc,3",
            "name,value\na,1\nb,9\nc,3\nd,4",
            &mut cx,
        );
        cx.run_until_parked();

        view.read_with(&cx, |view, _| {
            assert_eq!(view.shown_row_count(), view.diff.rows.len());
            for shown in 0..view.shown_row_count() {
                let row = view
                    .row_at(shown)
                    .and_then(|row| view.diff.rows.get(row))
                    .expect("every shown position should stand for an aligned row");
                assert!(
                    row.left_row.is_some() || row.right_row.is_some(),
                    "an aligned row has to come from at least one side"
                );
            }
            assert_eq!(view.diff.modified_rows, 1);
            assert_eq!(view.diff.added_rows, 1);
        });
    }

    /// The filter has to leave exactly the changed rows, and the positions it
    /// leaves have to keep pointing at the rows they stand for.
    #[gpui::test]
    async fn showing_only_the_differences_keeps_the_positions_honest(cx: &mut TestAppContext) {
        let (_workspace, _pane, _editor, mut cx) = open_grid_in_workspace("a,b", cx).await;
        let view = diff_view("h\na\nb\nc\nd", "h\na\nB\nc\nD", &mut cx);
        cx.run_until_parked();

        let all_rows = view.read_with(&cx, |view, _| view.shown_row_count());
        view.update(&mut cx, |view, cx| view.toggle_only_differences(cx));

        view.read_with(&cx, |view, _| {
            assert_eq!(view.shown_row_count(), 2);
            assert!(view.shown_row_count() < all_rows);
            for shown in 0..view.shown_row_count() {
                let row = view
                    .row_at(shown)
                    .and_then(|row| view.diff.rows.get(row))
                    .expect("a filtered position should still stand for an aligned row");
                assert_eq!(row.kind, DiffRowKind::Modified);
            }
            assert_eq!(view.shown_index_of(1), 0);
            assert_eq!(view.shown_index_of(3), 1);
        });

        view.update(&mut cx, |view, cx| view.toggle_only_differences(cx));
        view.read_with(&cx, |view, _| {
            assert_eq!(view.shown_row_count(), all_rows);
            assert_eq!(view.shown_index_of(3), 3);
        });
    }

    /// A wheel with nothing but a vertical delta must leave the columns where
    /// they are: an element that scrolls sideways takes a vertical delta as a
    /// sideways one unless it is restricted to its own axis, which would drag
    /// the table sideways every time the user scrolled down it.
    #[gpui::test]
    async fn scrolling_down_does_not_drag_the_columns_sideways(cx: &mut TestAppContext) {
        let mut columns = String::new();
        for column in 0..40 {
            if column > 0 {
                columns.push(',');
            }
            columns.push_str(&format!("column-{column}"));
        }
        let wide = format!("{columns}\n{columns}");
        let (_workspace, _pane, view, mut cx) = open_head_comparison(&wide, &wide, cx).await;

        let pane_bounds = cx
            .debug_bounds(DIFF_LEFT_PANE_DEBUG_SELECTOR)
            .expect("the left pane should have been drawn");
        let over_the_cells = pane_bounds.center();
        cx.simulate_mouse_move(over_the_cells, None, gpui::Modifiers::none());
        cx.run_until_parked();

        // Without this the rest of the test proves nothing: an element whose
        // content is no wider than itself is pinned at zero, and both of the
        // assertions below would hold however the wheel was handled.
        let scrollable_by = view.read_with(&cx, |view, _| view.horizontal_scroll.max_offset().x);
        assert!(
            scrollable_by > px(100.),
            "the columns have to reach past the pane for this to be a test ({scrollable_by:?})"
        );

        cx.simulate_event(gpui::ScrollWheelEvent {
            position: over_the_cells,
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-200.))),
            ..Default::default()
        });
        cx.run_until_parked();
        let after_scrolling_down = view.read_with(&cx, |view, _| view.horizontal_scroll.offset().x);
        assert_eq!(
            after_scrolling_down,
            px(0.),
            "a vertical wheel should not move the columns"
        );

        cx.simulate_event(gpui::ScrollWheelEvent {
            position: over_the_cells,
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(-120.), px(0.))),
            ..Default::default()
        });
        cx.run_until_parked();
        let after_scrolling_sideways =
            view.read_with(&cx, |view, _| view.horizontal_scroll.offset().x);
        assert_eq!(
            after_scrolling_sideways,
            px(-120.),
            "a sideways wheel should move the columns by what it was given"
        );
    }

    /// The toolbar has grown a toggle, a button, a position and two warnings,
    /// and a window narrow enough for all of it to overflow must still leave the
    /// panes wider than nothing: a pane whose content is no wider than itself
    /// has nothing to scroll, which is how the columns got stuck once before.
    #[gpui::test]
    async fn the_columns_still_scroll_in_a_window_the_toolbar_overflows(cx: &mut TestAppContext) {
        let columns: Vec<String> = (0..40).map(|column| format!("column-{column}")).collect();
        let committed = format!("{}\n{}", columns.join(","), columns.join(","));
        let mut changed = columns.clone();
        changed[0] = "column-changed".to_owned();
        let working_copy = format!("{}\n{}", columns.join(","), changed.join(","));
        let (_workspace, _pane, view, mut cx) =
            open_head_comparison(&committed, &working_copy, cx).await;

        // Puts the position on the toolbar as well, which is the widest the
        // toolbar gets without a warning on it.
        view.update(&mut cx, |view, cx| view.go_to_change(true, cx));
        cx.simulate_resize(gpui::size(px(420.), px(600.)));
        cx.run_until_parked();

        let pane_bounds = cx
            .debug_bounds(DIFF_LEFT_PANE_DEBUG_SELECTOR)
            .expect("the left pane should have been drawn");
        assert!(
            pane_bounds.size.width < px(320.),
            "the window has to be narrow for this to be a test ({:?})",
            pane_bounds.size.width
        );
        view.read_with(&cx, |view, _| {
            assert_eq!(view.change_position(), Some((1, 1)));
            assert!(
                view.horizontal_scroll.max_offset().x > px(100.),
                "the columns still have to reach past the pane ({:?})",
                view.horizontal_scroll.max_offset().x
            );
        });
    }

    /// The rows still scroll down. Restricting each pane to its own axis is what
    /// keeps a vertical wheel off the columns, and it must not also take that
    /// wheel away from the list the rows sit in.
    #[gpui::test]
    async fn scrolling_over_the_body_moves_the_rows(cx: &mut TestAppContext) {
        // Wide as well as long: the columns have to reach past the pane, or the
        // assertion that they stayed put would hold however the wheel was
        // handled.
        let columns: Vec<String> = (0..20).map(|column| format!("column-{column}")).collect();
        let mut table = columns.join(",");
        for row in 0..200 {
            table.push('\n');
            table.push_str(&columns.join(&format!("-{row},")));
        }
        let (_workspace, _pane, view, mut cx) = open_head_comparison(&table, &table, cx).await;

        let header = cx
            .debug_bounds(DIFF_LEFT_PANE_DEBUG_SELECTOR)
            .expect("the header row should have been drawn");
        let over_the_body = gpui::point(header.center().x, header.bottom() + px(100.));
        cx.simulate_mouse_move(over_the_body, None, gpui::Modifiers::none());
        cx.run_until_parked();

        let scrollable_by = view.read_with(&cx, |view, _| view.horizontal_scroll.max_offset().x);
        assert!(
            scrollable_by > px(100.),
            "the columns have to reach past the pane for this to be a test ({scrollable_by:?})"
        );

        cx.simulate_event(gpui::ScrollWheelEvent {
            position: over_the_body,
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-200.))),
            ..Default::default()
        });
        cx.run_until_parked();

        view.read_with(&cx, |view, _| {
            assert_eq!(
                view.horizontal_scroll.offset().x,
                px(0.),
                "a wheel over the body should not drag the columns sideways"
            );
            let rows_offset = view.list_scroll.0.borrow().base_handle.offset().y;
            assert!(
                rows_offset < px(-100.),
                "the rows should have scrolled under the wheel ({rows_offset:?})"
            );
        });
    }

    /// Both panes scroll sideways together, which they do by holding the one
    /// offset rather than by copying one to the other.
    #[gpui::test]
    async fn the_two_panes_share_one_horizontal_offset(cx: &mut TestAppContext) {
        let (_workspace, _pane, _editor, mut cx) = open_grid_in_workspace("a,b", cx).await;
        let wide = "a,b,c,d,e,f,g,h\n1,2,3,4,5,6,7,8";
        let view = diff_view(wide, wide, &mut cx);
        cx.run_until_parked();

        let offset = view.read_with(&cx, |view, _| view.horizontal_scroll.clone());
        offset.set_offset(gpui::point(px(-120.), px(0.)));

        view.read_with(&cx, |view, _| {
            assert_eq!(view.horizontal_scroll.offset().x, px(-120.));
        });
    }

    #[gpui::test]
    async fn stepping_through_the_changes_walks_the_blocks(cx: &mut TestAppContext) {
        let (_workspace, _pane, _editor, mut cx) = open_grid_in_workspace("a,b", cx).await;
        let view = diff_view("h\na\nb\nc\nd\ne", "h\na\nB\nC\nd\nE", &mut cx);
        cx.run_until_parked();

        view.read_with(&cx, |view, _| {
            assert_eq!(view.diff.change_blocks(), [1, 4]);
            assert_eq!(view.current_change, None);
        });

        view.update(&mut cx, |view, cx| view.go_to_change(true, cx));
        view.read_with(&cx, |view, _| assert_eq!(view.current_change, Some(0)));
        view.update(&mut cx, |view, cx| view.go_to_change(true, cx));
        view.read_with(&cx, |view, _| assert_eq!(view.current_change, Some(1)));
        // The last change is the last stop rather than a wrap back to the top.
        view.update(&mut cx, |view, cx| view.go_to_change(true, cx));
        view.read_with(&cx, |view, _| assert_eq!(view.current_change, Some(1)));
        view.update(&mut cx, |view, cx| view.go_to_change(false, cx));
        view.read_with(&cx, |view, _| assert_eq!(view.current_change, Some(0)));
    }

    /// Editing the grid has to bring the comparison along with it, or the tab
    /// beside it would keep showing a table the file no longer holds.
    #[gpui::test]
    async fn an_edit_to_the_file_recomputes_the_comparison(cx: &mut TestAppContext) {
        let (workspace, _pane, editor, mut cx) =
            open_grid_in_workspace("name,value\na,1\nb,2", cx).await;
        let buffer = editor
            .read_with(&cx, |editor, cx| editor.buffer().read(cx).as_singleton())
            .expect("the fixture is a single file");

        let view = workspace.update_in(&mut cx, |_workspace, _window, cx| {
            let committed = cx.new(|cx| Buffer::local("name,value\na,1\nb,2", cx));
            cx.new(|cx| {
                TableDiffView::from_files(
                    "比較".into(),
                    "HEAD".into(),
                    "現在".into(),
                    LeftSource::File(committed),
                    buffer.clone(),
                    ',',
                    ',',
                    true,
                    cx,
                )
            })
        });
        cx.run_until_parked();
        view.read_with(&cx, |view, _| assert_eq!(view.diff.change_count(), 0));

        buffer.update(&mut cx, |buffer, cx| {
            buffer.set_text("name,value\na,1\nb,99", cx);
        });
        cx.run_until_parked();
        cx.executor().advance_clock(RECOMPUTE_DEBOUNCE * 2);
        cx.run_until_parked();

        view.read_with(&cx, |view, _| {
            assert_eq!(view.diff.modified_rows, 1);
            assert_eq!(
                view.diff.rows.last().map(|row| row.changed_columns.clone()),
                Some(vec![1])
            );
        });
    }

    /// Opens a grid on a file git has a committed version of, and compares the
    /// two, which is the path all of the HEAD comparison rests on.
    async fn open_head_comparison(
        committed: &str,
        working_copy: &str,
        cx: &mut TestAppContext,
    ) -> (
        Entity<Workspace>,
        Entity<TabularDataPreviewPane>,
        Entity<TableDiffView>,
        VisualTestContext,
    ) {
        let (workspace, pane, _editor, mut cx) = open_grid_in_workspace(working_copy, cx).await;
        let fs = workspace.read_with(&cx, |workspace, cx| {
            workspace.project().read(cx).fs().clone()
        });
        let fake_fs = fs.as_fake();
        fake_fs
            .insert_tree(util::path!("/project/.git"), serde_json::json!({}))
            .await;
        fake_fs.set_head_for_repo(
            util::path!("/project/.git").as_ref(),
            &[("data.csv", committed.to_owned())],
            "0000000",
        );
        cx.run_until_parked();

        workspace.update_in(&mut cx, |workspace, window, cx| {
            TableDiffView::diff_with_head(workspace, pane.clone(), window, cx);
        });
        cx.run_until_parked();

        let view = workspace
            .read_with(&cx, |workspace, cx| {
                workspace.items_of_type::<TableDiffView>(cx).next()
            })
            .expect("the comparison should have opened as a tab");
        (workspace, pane, view, cx)
    }

    /// A comparison against the committed version has to reach git through the
    /// project, which is the path that also works on a remote project.
    #[gpui::test]
    async fn the_committed_version_opens_as_a_comparison(cx: &mut TestAppContext) {
        let (workspace, pane, view, cx) =
            open_head_comparison("name,value\na,1\nb,2", "name,value\na,1\nb,99", cx).await;

        view.read_with(&cx, |view, _| {
            assert_eq!(view.diff.modified_rows, 1);
            assert_eq!(
                view.diff.rows.last().map(|row| row.changed_columns.clone()),
                Some(vec![1])
            );
        });
        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.grid.status_message, None);
        });
        // The grid and the comparison are two views of one file. A pane
        // recognizes an already-open file by its project entry, so the
        // comparison would merely re-activate the grid's tab if it claimed to
        // be that file's singleton item.
        workspace.read_with(&cx, |workspace, cx| {
            let pane_items = workspace.active_pane().read(cx).items().count();
            assert_eq!(
                pane_items, 2,
                "the comparison should sit in the grid's own pane beside it"
            );
        });
    }

    /// The two panes have to end up beside each other and the same width. A
    /// comparison whose left pane was squeezed to nothing would still pass
    /// every assertion about the alignment behind it.
    #[gpui::test]
    async fn the_comparison_draws_its_two_panes_side_by_side(cx: &mut TestAppContext) {
        let (_workspace, _pane, _view, mut cx) =
            open_head_comparison("name,value\na,1\nb,2", "name,value\na,1\nb,99", cx).await;

        let left = cx
            .debug_bounds(DIFF_LEFT_PANE_DEBUG_SELECTOR)
            .expect("the left pane should have been drawn");
        let right = cx
            .debug_bounds(DIFF_RIGHT_PANE_DEBUG_SELECTOR)
            .expect("the right pane should have been drawn");

        assert!(
            left.size.width > px(0.) && left.size.height > px(0.),
            "the left pane was drawn with no size ({left:?})"
        );
        assert_eq!(
            left.size.width, right.size.width,
            "the two panes should be the same width ({left:?} against {right:?})"
        );
        assert!(
            right.origin.x >= left.origin.x + left.size.width,
            "the right pane should start where the left one ends ({left:?} against {right:?})"
        );
        assert_eq!(
            left.origin.y, right.origin.y,
            "the two panes' header rows should sit on the same line"
        );
    }

    /// Without a repository there is nothing to compare against, and the grid
    /// has to say so rather than open an empty tab.
    #[gpui::test]
    async fn a_file_outside_git_says_why_it_cannot_be_compared(cx: &mut TestAppContext) {
        let (workspace, pane, _editor, mut cx) =
            open_grid_in_workspace("name,value\na,1", cx).await;

        workspace.update_in(&mut cx, |workspace, window, cx| {
            TableDiffView::diff_with_head(workspace, pane.clone(), window, cx);
        });
        cx.run_until_parked();

        workspace.read_with(&cx, |workspace, cx| {
            assert_eq!(workspace.items_of_type::<TableDiffView>(cx).count(), 0);
        });
        pane.read_with(&cx, |pane, _| {
            assert!(
                pane.grid
                    .status_message
                    .as_ref()
                    .is_some_and(|message| message.contains("git")),
                "the grid should have said why it could not compare ({:?})",
                pane.grid.status_message
            );
        });
    }

    #[gpui::test]
    async fn two_selected_columns_are_compared_against_each_other(cx: &mut TestAppContext) {
        let (_workspace, pane, _editor, mut cx) =
            open_grid_in_workspace("before,after\na,a\nb,B\nc,c", cx).await;
        cx.simulate_keystrokes("down");
        cx.simulate_keystrokes("shift-down shift-down shift-right");
        cx.run_until_parked();

        let comparison = pane
            .read_with(&cx, |pane, _| cell_comparison(pane, ""))
            .expect("two columns should be comparable");

        assert_eq!(comparison.left.rows.len(), 3);
        assert_eq!(comparison.right.rows.len(), 3);
        let diff = cell_diff(&comparison);
        assert_eq!(diff.modified_rows, 1);
        assert_eq!(diff.change_count(), 1);
    }

    /// The rows a comparison names have to be the rows the grid names, or its
    /// title would point at a different line of the file than the toolbar does.
    #[gpui::test]
    async fn a_two_row_comparison_names_the_rows_the_grid_names(cx: &mut TestAppContext) {
        let (_workspace, pane, _editor, mut cx) =
            open_grid_in_workspace("name,value\nalpha,1\nbeta,2", cx).await;
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        let first_address = pane.read_with(&cx, |pane, _| pane.cursor_address());
        cx.simulate_keystrokes("shift-down");
        cx.run_until_parked();
        let last_address = pane.read_with(&cx, |pane, _| pane.cursor_address());

        let comparison = pane
            .read_with(&cx, |pane, _| cell_comparison(pane, ""))
            .expect("two rows of one column should be comparable");

        assert_eq!(first_address, "A2");
        assert_eq!(last_address, "A3");
        assert_eq!(comparison.left_title, "行 2");
        assert_eq!(comparison.right_title, "行 3");
        assert_eq!(comparison.title, "セルの比較 (A2 ↔ A3)");
    }

    /// The comparison holds both whole tables and the buffer of the file it
    /// follows, so closing its tab has to let go of all of it.
    #[gpui::test]
    async fn closing_the_comparison_releases_everything_it_held(cx: &mut TestAppContext) {
        let (workspace, _pane, view, mut cx) =
            open_head_comparison("name,value\na,1", "name,value\na,2", cx).await;

        let released_view = view.downgrade();
        drop(view);

        let close = workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.active_pane().update(cx, |pane, cx| {
                pane.close_active_item(&workspace::CloseActiveItem::default(), window, cx)
            })
        });
        close.await.expect("closing the comparison tab should work");
        cx.run_until_parked();

        released_view.assert_released();
    }

    #[gpui::test]
    async fn one_selected_cell_is_compared_against_the_clipboard(cx: &mut TestAppContext) {
        let (_workspace, pane, _editor, mut cx) =
            open_grid_in_workspace("name,value\nalpha,1", cx).await;
        cx.simulate_keystrokes("down");
        cx.run_until_parked();

        let comparison = pane
            .read_with(&cx, |pane, _| cell_comparison(pane, "alpaca"))
            .expect("a single cell should be comparable against the clipboard");
        assert_eq!(comparison.left_title, "クリップボード");
        let diff = cell_diff(&comparison);
        assert_eq!(diff.modified_rows, 1);

        pane.read_with(&cx, |pane, _| {
            assert!(
                cell_comparison(pane, "").is_none(),
                "an empty clipboard leaves nothing to compare a single cell against"
            );
        });
    }

    /// Two blocks picked with Ctrl say outright which two things to compare, so
    /// they win over every guess the shape of a single block would allow - here
    /// a block that is two columns wide would otherwise have been read as its
    /// own two columns.
    #[gpui::test]
    async fn two_ctrl_picked_ranges_are_the_two_sides(cx: &mut TestAppContext) {
        use crate::grid::{GridCell, SelectionChange};
        use crate::types::DisplayRow;

        let (_workspace, pane, _editor, mut cx) =
            open_grid_in_workspace("l,r,x\na,a,1\nb,B,2\nc,c,3", cx).await;

        pane.update(&mut cx, |pane, cx| {
            let cell = |row: usize, col: usize| {
                GridCell::new(GridRow::Body(DisplayRow(row)), AnyColumn(col))
            };
            pane.change_cell_selection(cell(0, 0), SelectionChange::Replace, cx);
            pane.change_cell_selection(cell(2, 0), SelectionChange::Extend, cx);
            pane.change_cell_selection(cell(0, 1), SelectionChange::Add, cx);
            pane.change_cell_selection(cell(2, 1), SelectionChange::Extend, cx);
        });

        let comparison = pane
            .read_with(&cx, |pane, _| cell_comparison(pane, ""))
            .expect("two ranges should be comparable");

        assert_eq!(comparison.left_title, "A2:A4");
        assert_eq!(comparison.right_title, "B2:B4");
        assert_eq!(comparison.left.rows.len(), 3);
        assert_eq!(comparison.right.rows.len(), 3);
        let diff = cell_diff(&comparison);
        assert_eq!(diff.change_count(), 1, "only the middle row differs");
    }

    /// Ranges of different shapes are compared as they are rather than refused:
    /// the row matching pads whichever side runs out first.
    #[gpui::test]
    async fn two_ranges_of_different_shapes_are_still_compared(cx: &mut TestAppContext) {
        use crate::grid::{GridCell, SelectionChange};
        use crate::types::DisplayRow;

        let (_workspace, pane, _editor, mut cx) =
            open_grid_in_workspace("l,r\na,a\nb,b\nc,c", cx).await;

        pane.update(&mut cx, |pane, cx| {
            let cell = |row: usize, col: usize| {
                GridCell::new(GridRow::Body(DisplayRow(row)), AnyColumn(col))
            };
            pane.change_cell_selection(cell(0, 0), SelectionChange::Replace, cx);
            pane.change_cell_selection(cell(2, 0), SelectionChange::Extend, cx);
            pane.change_cell_selection(cell(0, 1), SelectionChange::Add, cx);
        });

        let comparison = pane
            .read_with(&cx, |pane, _| cell_comparison(pane, ""))
            .expect("two ranges of different heights should still be comparable");

        assert_eq!(comparison.left.rows.len(), 3);
        assert_eq!(comparison.right.rows.len(), 1);
    }

    fn cells(row: &[SharedString]) -> Vec<&str> {
        row.iter().map(SharedString::as_ref).collect()
    }

    fn project_fs(
        workspace: &Entity<Workspace>,
        cx: &VisualTestContext,
    ) -> std::sync::Arc<dyn fs::Fs> {
        workspace.read_with(cx, |workspace, cx| {
            workspace.project().read(cx).fs().clone()
        })
    }

    /// A commit under the file has to take the left pane with it: the comparison
    /// is of HEAD, not of the version HEAD happened to hold when the tab opened.
    #[gpui::test]
    async fn a_new_commit_moves_the_left_pane(cx: &mut TestAppContext) {
        let (workspace, _pane, view, cx) =
            open_head_comparison("name,value\na,1", "name,value\na,2", cx).await;

        view.read_with(&cx, |view, _| {
            assert_eq!(cells(view.left.row(0)), ["a", "1"]);
            assert_eq!(view.diff.modified_rows, 1);
        });

        let fs = project_fs(&workspace, &cx);
        fs.as_fake().set_head_for_repo(
            util::path!("/project/.git").as_ref(),
            &[("data.csv", "name,value\na,2".to_owned())],
            "1111111",
        );
        cx.run_until_parked();
        cx.executor().advance_clock(RECOMPUTE_DEBOUNCE * 2);
        cx.run_until_parked();

        view.read_with(&cx, |view, _| {
            assert_eq!(
                cells(view.left.row(0)),
                ["a", "2"],
                "the left pane should be showing the version that is committed now"
            );
            assert_eq!(view.diff.change_count(), 0);
            assert!(!view.left_is_stale);
        });
    }

    /// A file that stops being tracked has no committed version left to read, and
    /// an empty left pane would claim every row had just been added.
    #[gpui::test]
    async fn losing_the_committed_version_keeps_the_last_one_it_could_read(
        cx: &mut TestAppContext,
    ) {
        let (workspace, _pane, view, cx) =
            open_head_comparison("name,value\na,1", "name,value\na,2", cx).await;

        let fs = project_fs(&workspace, &cx);
        fs.as_fake()
            .set_head_for_repo(util::path!("/project/.git").as_ref(), &[], "2222222");
        cx.run_until_parked();
        cx.executor().advance_clock(RECOMPUTE_DEBOUNCE * 2);
        cx.run_until_parked();

        view.read_with(&cx, |view, _| {
            assert_eq!(
                cells(view.left.row(0)),
                ["a", "1"],
                "the last version that could be read has to stay on screen"
            );
            assert!(
                view.left_is_stale,
                "and the comparison has to say that it is out of date"
            );
        });
    }

    /// The other file is held open rather than read once, so editing it moves the
    /// left pane the way editing the grid's own file moves the right one.
    #[gpui::test]
    async fn an_edit_to_the_other_file_moves_the_left_pane(cx: &mut TestAppContext) {
        let (_workspace, _pane, _editor, mut cx) = open_grid_in_workspace("a,b", cx).await;
        let (view, other_file, _current) = diff_view_over_buffers("h,v\na,1", "h,v\na,1", &mut cx);
        cx.run_until_parked();
        view.read_with(&cx, |view, _| assert_eq!(view.diff.change_count(), 0));

        other_file.update(&mut cx, |buffer, cx| buffer.set_text("h,v\na,9", cx));
        cx.run_until_parked();
        cx.executor().advance_clock(RECOMPUTE_DEBOUNCE * 2);
        cx.run_until_parked();

        view.read_with(&cx, |view, _| {
            assert_eq!(cells(view.left.row(0)), ["a", "9"]);
            assert_eq!(view.diff.modified_rows, 1);
        });
    }

    /// The header setting belongs to the comparison rather than to the grid it
    /// was opened from, and turning it off puts the first line back among the
    /// rows that are lined up against each other.
    #[gpui::test]
    async fn the_header_toggle_moves_the_first_line_into_the_alignment(cx: &mut TestAppContext) {
        let (_workspace, _pane, _editor, mut cx) = open_grid_in_workspace("a,b", cx).await;
        let view = diff_view("h,v\nx,1", "h,V\nx,1", &mut cx);
        cx.run_until_parked();

        view.read_with(&cx, |view, _| {
            assert!(view.first_row_is_header);
            assert!(view.diff.header_changed());
            assert_eq!(view.diff.rows.len(), 1);
            assert_eq!(view.left.row_number(0), 2);
        });

        view.update(&mut cx, |view, cx| view.toggle_first_row_is_header(cx));
        cx.run_until_parked();

        view.read_with(&cx, |view, _| {
            assert!(!view.first_row_is_header);
            assert_eq!(view.diff.header, None);
            assert_eq!(
                view.diff.rows.len(),
                2,
                "the first line takes part in the alignment now"
            );
            assert_eq!(view.left.row_number(0), 1);
        });
    }

    /// A comparison whose only difference is in the header still has somewhere
    /// for "next" to go, or the toolbar would count a difference the buttons
    /// could not reach.
    #[gpui::test]
    async fn stepping_through_the_changes_stops_at_the_header(cx: &mut TestAppContext) {
        let (_workspace, _pane, _editor, mut cx) = open_grid_in_workspace("a,b", cx).await;
        let view = diff_view("h,v\nx,1", "h,V\nx,1", &mut cx);
        cx.run_until_parked();

        view.read_with(&cx, |view, _| {
            assert!(view.diff.change_blocks().is_empty());
            assert_eq!(view.diff.change_count(), 1);
            assert_eq!(view.change_position(), None);
        });

        view.update(&mut cx, |view, cx| view.go_to_change(true, cx));
        view.read_with(&cx, |view, _| {
            assert_eq!(view.current_change, Some(0));
            assert_eq!(view.change_position(), Some((1, 1)));
        });
    }

    /// The header is read before the rows, so stepping forward from the top
    /// walks the table in the order it is drawn in.
    #[gpui::test]
    async fn the_header_is_the_first_stop_before_the_rows(cx: &mut TestAppContext) {
        let (_workspace, _pane, _editor, mut cx) = open_grid_in_workspace("a,b", cx).await;
        let view = diff_view("h,v\nx,1\ny,2", "h,V\nx,9\ny,2", &mut cx);
        cx.run_until_parked();

        view.update(&mut cx, |view, cx| view.go_to_change(true, cx));
        view.read_with(&cx, |view, _| {
            assert_eq!(
                view.diff.change_stops(),
                [ChangeStop::Header, ChangeStop::Row(0)]
            );
            assert_eq!(view.change_position(), Some((1, 2)));
        });

        view.update(&mut cx, |view, cx| view.go_to_change(true, cx));
        view.read_with(&cx, |view, _| {
            assert_eq!(view.change_position(), Some((2, 2)));
        });
    }

    /// A comparison of cells is a place in a grid rather than something that can
    /// say it changed, so taking it again is what brings it up to date.
    #[gpui::test]
    async fn taking_a_cell_comparison_again_reads_the_grid_as_it_is_now(cx: &mut TestAppContext) {
        use crate::grid::{GridCell, SelectionChange};
        use crate::types::DisplayRow;

        let (workspace, pane, editor, mut cx) =
            open_grid_in_workspace("before,after\na,a\nb,b", cx).await;
        let buffer = editor
            .read_with(&cx, |editor, cx| editor.buffer().read(cx).as_singleton())
            .expect("the fixture is a single file");
        pane.update(&mut cx, |pane, cx| {
            let cell = |row: usize, col: usize| {
                GridCell::new(GridRow::Body(DisplayRow(row)), AnyColumn(col))
            };
            pane.change_cell_selection(cell(0, 0), SelectionChange::Replace, cx);
            pane.change_cell_selection(cell(1, 0), SelectionChange::Extend, cx);
            pane.change_cell_selection(cell(0, 1), SelectionChange::Add, cx);
            pane.change_cell_selection(cell(1, 1), SelectionChange::Extend, cx);
        });
        workspace.update_in(&mut cx, |workspace, window, cx| {
            TableDiffView::diff_selected_cells(workspace, pane.clone(), window, cx);
        });
        cx.run_until_parked();

        let view = workspace
            .read_with(&cx, |workspace, cx| {
                workspace.items_of_type::<TableDiffView>(cx).next()
            })
            .expect("the cell comparison should have opened as a tab");
        view.read_with(&cx, |view, _| {
            assert!(view.can_recompare());
            assert_eq!(view.diff.change_count(), 0);
        });

        buffer.update(&mut cx, |buffer, cx| {
            buffer.set_text("before,after\na,changed\nb,b", cx);
        });
        cx.executor()
            .advance_clock(crate::parser::REPARSE_DEBOUNCE * 2);
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.cell_value(GridRow::Body(DisplayRow(0)), AnyColumn(1)),
                "changed",
                "the grid itself has to be showing the new value for this to be a test"
            );
        });
        view.read_with(&cx, |view, _| {
            assert_eq!(
                view.diff.change_count(),
                0,
                "cells are compared as they were read; nothing should follow the grid on its own"
            );
        });

        view.update(&mut cx, |view, cx| view.recompare(cx));
        cx.run_until_parked();

        view.read_with(&cx, |view, _| {
            assert_eq!(cells(view.right.row(0)), ["changed"]);
            assert_eq!(view.diff.change_count(), 1);
        });
    }

    /// Once the grid is gone there is nothing left to read the cells out of, and
    /// the comparison has to say so rather than quietly empty itself.
    #[gpui::test]
    async fn a_cell_comparison_cannot_be_taken_again_without_its_grid(cx: &mut TestAppContext) {
        let (_workspace, _pane, _editor, mut cx) = open_grid_in_workspace("a,b", cx).await;
        let side = |value: &str| CellComparisonSide {
            block: CellBlock::Clipboard,
            rows: vec![vec![SharedString::from(value.to_owned())]],
        };
        let view = cx.update(|_window, cx| {
            cx.new(|cx| {
                TableDiffView::from_cells(
                    "比較".into(),
                    "左".into(),
                    "右".into(),
                    WeakEntity::new_invalid(),
                    side("a"),
                    side("b"),
                    cx,
                )
            })
        });
        cx.run_until_parked();

        view.read_with(&cx, |view, _| {
            assert!(!view.can_recompare());
            assert_eq!(cells(view.left.row(0)), ["a"]);
        });

        view.update(&mut cx, |view, cx| view.recompare(cx));
        cx.run_until_parked();

        view.read_with(&cx, |view, _| {
            assert_eq!(
                cells(view.left.row(0)),
                ["a"],
                "a comparison that cannot be taken again has to keep what it holds"
            );
        });
    }

    /// The left pane says when it was read, so that a comparison left open for
    /// hours does not read as being about the version committed now.
    #[gpui::test]
    async fn the_left_pane_title_says_when_it_was_read(cx: &mut TestAppContext) {
        let (_workspace, _pane, view, cx) =
            open_head_comparison("name,value\na,1", "name,value\na,2", cx).await;

        let title = view.read_with(&cx, |view, _| view.left_pane_title());
        let (name, taken) = title
            .split_once(" (")
            .unwrap_or_else(|| panic!("the left title should say when it was read ({title})"));
        let time = taken
            .strip_suffix(" 取得)")
            .unwrap_or_else(|| panic!("the left title should say when it was read ({title})"));
        let (hour, minute) = time
            .split_once(':')
            .unwrap_or_else(|| panic!("the time should be a clock time ({title})"));

        assert_eq!(name, "HEAD");
        assert!(
            hour.len() == 2 && hour.parse::<u32>().is_ok_and(|hour| hour < 24),
            "{title}"
        );
        assert!(
            minute.len() == 2 && minute.parse::<u32>().is_ok_and(|minute| minute < 60),
            "{title}"
        );
    }

    /// "HEAD" has to mean the commit rather than what has been staged: a change
    /// added to the index but not committed is still a difference the
    /// comparison is there to show.
    #[gpui::test]
    async fn the_left_pane_is_the_commit_rather_than_the_index(cx: &mut TestAppContext) {
        let (workspace, pane, view, mut cx) =
            open_head_comparison("name,value\na,1", "name,value\na,3", cx).await;
        let buffer = pane
            .read_with(&cx, |pane, cx| pane.buffer(cx))
            .expect("the grid is open on a file");

        let fs = project_fs(&workspace, &cx);
        fs.as_fake().set_index_for_repo(
            util::path!("/project/.git").as_ref(),
            &[("data.csv", "name,value\na,2".to_owned())],
        );
        cx.run_until_parked();
        // Both sides are read again from here, so a comparison reading the index
        // would have every chance to pick the staged version up.
        buffer.update(&mut cx, |buffer, cx| {
            buffer.set_text("name,value\na,4", cx);
        });
        cx.run_until_parked();
        cx.executor().advance_clock(RECOMPUTE_DEBOUNCE * 2);
        cx.run_until_parked();

        view.read_with(&cx, |view, _| {
            assert_eq!(
                cells(view.right.row(0)),
                ["a", "4"],
                "the right pane has to have been read again for this to be a test"
            );
            assert_eq!(
                cells(view.left.row(0)),
                ["a", "1"],
                "the left pane is the committed version, not the staged one"
            );
        });
    }

    /// A committed version that comes back - a branch switched back, a revert -
    /// has to take the warning down with it rather than leave the comparison
    /// saying it is out of date for the rest of the session.
    #[gpui::test]
    async fn the_committed_version_coming_back_clears_the_warning(cx: &mut TestAppContext) {
        let (workspace, _pane, view, cx) =
            open_head_comparison("name,value\na,1", "name,value\na,2", cx).await;
        let fs = project_fs(&workspace, &cx);

        fs.as_fake()
            .set_head_for_repo(util::path!("/project/.git").as_ref(), &[], "2222222");
        cx.run_until_parked();
        cx.executor().advance_clock(RECOMPUTE_DEBOUNCE * 2);
        cx.run_until_parked();
        view.read_with(&cx, |view, _| {
            assert!(view.left_is_stale);
            let reason = view
                .left_stale_reason()
                .expect("a stale left pane has to say what went missing");
            assert!(reason.contains("HEAD"), "{reason}");
        });

        fs.as_fake().set_head_for_repo(
            util::path!("/project/.git").as_ref(),
            &[("data.csv", "name,value\na,9".to_owned())],
            "3333333",
        );
        cx.run_until_parked();
        cx.executor().advance_clock(RECOMPUTE_DEBOUNCE * 2);
        cx.run_until_parked();

        view.read_with(&cx, |view, _| {
            assert!(
                !view.left_is_stale,
                "the committed version can be read again"
            );
            assert_eq!(view.left_stale_reason(), None);
            assert_eq!(cells(view.left.row(0)), ["a", "9"]);
        });
    }

    /// A file deleted or renamed away under the comparison leaves the buffer
    /// holding the text it last had, which would otherwise leave the left pane
    /// quietly standing in for a file that is not there any more.
    #[gpui::test]
    async fn the_other_file_going_away_is_said_rather_than_ignored(cx: &mut TestAppContext) {
        let (workspace, pane, _editor, mut cx) =
            open_grid_in_workspace("name,value\na,1", cx).await;
        let project = workspace.read_with(&cx, |workspace, _| workspace.project().clone());
        let fs = project_fs(&workspace, &cx);
        fs.as_fake()
            .insert_file(
                util::path!("/project/other.csv"),
                b"name,value\na,2".to_vec(),
            )
            .await;
        cx.run_until_parked();
        let other = project
            .update(&mut cx, |project, cx| {
                project.open_local_buffer(util::path!("/project/other.csv"), cx)
            })
            .await
            .expect("the other file should open");
        let buffer = pane
            .read_with(&cx, |pane, cx| pane.buffer(cx))
            .expect("the grid is open on a file");
        let view = cx.update(|_window, cx| {
            cx.new(|cx| {
                TableDiffView::from_files(
                    "比較".into(),
                    "other.csv".into(),
                    "data.csv".into(),
                    LeftSource::File(other),
                    buffer,
                    ',',
                    ',',
                    true,
                    cx,
                )
            })
        });
        cx.run_until_parked();
        view.read_with(&cx, |view, _| {
            assert_eq!(cells(view.left.row(0)), ["a", "2"]);
            assert!(!view.left_is_stale);
        });

        fs.remove_file(
            util::path!("/project/other.csv").as_ref(),
            fs::RemoveOptions::default(),
        )
        .await
        .expect("the other file should be removable");
        cx.run_until_parked();
        view.update(&mut cx, |view, cx| view.recompare(cx));
        cx.run_until_parked();

        view.read_with(&cx, |view, _| {
            assert!(
                view.left_is_stale,
                "a comparison against a file that is gone has to say so"
            );
            let reason = view
                .left_stale_reason()
                .expect("a stale left pane has to say what went missing");
            assert!(reason.contains("other.csv"), "{reason}");
            assert_eq!(
                cells(view.left.row(0)),
                ["a", "2"],
                "and has to keep the last version it could read"
            );
        });
    }

    /// The comparison holds the versions it follows open for as long as its tab
    /// is. Closing the tab has to let go of them - and of the comparison itself,
    /// which its own subscriptions could otherwise keep alive.
    #[gpui::test]
    async fn closing_the_comparison_releases_it(cx: &mut TestAppContext) {
        let (workspace, _pane, view, mut cx) =
            open_head_comparison("name,value\na,1", "name,value\na,2", cx).await;

        let item_id = view.entity_id();
        let released = view.downgrade();
        drop(view);
        workspace
            .update_in(&mut cx, |workspace, window, cx| {
                workspace.active_pane().update(cx, |pane, cx| {
                    pane.close_item_by_id(item_id, SaveIntent::Skip, window, cx)
                })
            })
            .await
            .expect("closing the comparison tab should succeed");
        cx.run_until_parked();

        released.assert_released();
    }

    /// Reading the first row as a header can leave a comparison of two single
    /// rows holding nothing but headers, which still has to have somewhere for
    /// "next" to stop and a column for the strip to draw.
    #[gpui::test]
    async fn a_comparison_of_nothing_but_headers_still_has_a_stop(cx: &mut TestAppContext) {
        let (_workspace, _pane, _editor, mut cx) = open_grid_in_workspace("a,b", cx).await;
        let side = |value: &str| CellComparisonSide {
            block: CellBlock::Clipboard,
            rows: vec![vec![SharedString::from(value.to_owned())]],
        };
        let view = cx.update(|_window, cx| {
            cx.new(|cx| {
                TableDiffView::from_cells(
                    "比較".into(),
                    "左".into(),
                    "右".into(),
                    WeakEntity::new_invalid(),
                    side("a"),
                    side("b"),
                    cx,
                )
            })
        });
        cx.run_until_parked();

        view.update(&mut cx, |view, cx| view.toggle_first_row_is_header(cx));
        cx.run_until_parked();

        view.read_with(&cx, |view, _| {
            assert!(
                view.diff.rows.is_empty(),
                "the one row each side had is the header now"
            );
            assert!(
                view.diff.column_count > 0,
                "the header strip still has a column to draw"
            );
            assert_eq!(view.diff.change_count(), 1);
            assert_eq!(view.diff.change_stops(), [ChangeStop::Header]);
        });

        view.update(&mut cx, |view, cx| view.go_to_change(true, cx));
        view.read_with(&cx, |view, _| {
            assert_eq!(view.change_position(), Some((1, 1)));
        });
    }

    /// The column holding the row numbers is sized by the largest number a pane
    /// shows, which one quoted line break can push far past the row count.
    #[test]
    fn the_row_number_column_fits_the_largest_line_number() {
        let short = DiffSide::parse("a,b\nc,d", ',', false);
        let long = DiffSide::parse(&format!("\"{}\",b\nc,d", "x\n".repeat(200)), ',', false);

        assert_eq!(long.rows.len(), 2);
        assert_eq!(long.last_row_number(), 202);
        assert!(
            row_number_width(&long, &long) > row_number_width(&short, &short),
            "a comparison numbering a row 202 needs more room than one numbering it 2"
        );
    }
}
