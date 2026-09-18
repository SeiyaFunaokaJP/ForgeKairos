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
    Font, HighlightStyle, ListAlignment, ListOffset, ListState, PathPromptOptions, ScrollHandle,
    StyledText, Subscription, Task, WeakEntity, actions, canvas, list,
};
use language::{Buffer, BufferEvent};
use project::{DirectoryLister, Project};
use settings::Settings as _;
use theme_settings::ThemeSettings;
use ui::{
    ActiveTheme as _, Button, ButtonSize, ButtonStyle, Checkbox, ContextMenu, DropdownMenu,
    ElementId, Icon, IconButton, IconName, IconPosition, IconSize, Label, LabelCommon as _,
    LabelSize, PopoverMenu, ScrollAxes, Scrollbars, SharedString, StyledTypography as _,
    ToggleState, Tooltip, WithScrollbar as _, div, h_flex, prelude::*, v_flex,
};
use workspace::{
    Workspace,
    item::{Item, ItemEvent, TabContentParams},
};

use crate::{
    TabularDataPreviewPane,
    grid::{GridRect, GridRow, GridSelectionKind, cell_line_height, column_label},
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
/// One character's width for each pixel of font size, for a buffer font whose
/// advance cannot be read: the 8px the columns were sized at before they were
/// measured, over the default 15px buffer font.
const FALLBACK_CHARACTER_WIDTH_PER_FONT_SIZE: f32 = 8.0 / 15.0;
const COLUMN_WIDTH_PADDING: f32 = 16.0;
const MINIMUM_COLUMN_WIDTH: f32 = 64.0;
const MAXIMUM_COLUMN_WIDTH: f32 = 320.0;
/// Room for the row number and the padding around it.
const ROW_NUMBER_PADDING: f32 = 14.0;
const MINIMUM_ROW_NUMBER_DIGITS: usize = 2;
/// The strip the two horizontal scrollbars are laid into, over the foot of the
/// body the way a scrollbar always overlays what it scrolls.
const HORIZONTAL_SCROLLBAR_HEIGHT: Pixels = px(12.);
/// The header row's place in the element ids the panes are keyed by. It is not
/// a row of the list, so it cannot collide with one.
const HEADER_ELEMENT_INDEX: usize = usize::MAX;
/// How far above and below the view the list measures rows it does not draw.
/// The list only scrolls as far as the rows it has measured reach, so this
/// is also how far one turn of the wheel can go in a frame where it has nothing
/// else to go on.
const LIST_OVERDRAW: Pixels = px(200.);

/// Marks the two panes of the header row, so that a test can check that they
/// are laid out beside each other and at the same width - the whole premise of
/// reading two tables side by side. `debug_selector` compiles away outside of
/// tests.
pub(crate) const DIFF_LEFT_PANE_DEBUG_SELECTOR: &str = "tabular-diff-left-pane";
pub(crate) const DIFF_RIGHT_PANE_DEBUG_SELECTOR: &str = "tabular-diff-right-pane";
/// Marks the pane titles that are lists to pick from, which is what a test
/// needs to tell them apart from the plain titles every other comparison has.
const DIFF_LEFT_PICK_DEBUG_SELECTOR: &str = "tabular-diff-left-pick";
const DIFF_RIGHT_PICK_DEBUG_SELECTOR: &str = "tabular-diff-right-pick";

/// The most things one comparison of cells offers to pick from. Past it the
/// pick list stops being something a person chooses from, and a whole column
/// selected by its header would otherwise have every row of it read out as a
/// candidate of its own.
const MAX_CELL_CANDIDATES: usize = 100;
/// The most cells a comparison of more than two candidates reads out of the
/// grid, which is the budget the grid already puts on copying a selection.
///
/// Every candidate is read and held when the tab opens and read again by
/// "再比較", so Ctrl+A over a wide table of a million rows would otherwise copy
/// all of it cell by cell on the UI thread. Two candidates are exactly the two
/// sides, which is all a comparison of cells has ever read, so they are left
/// to that.
const MAX_CELL_CANDIDATE_CELLS: usize = crate::grid::MAX_GRID_OPERATION_CELLS;

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

/// One of the blocks of a cell comparison, as what it takes to read the block
/// again rather than as the values it held.
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
    /// Blocks of cells lifted out of one grid, two of them side by side at a
    /// time. These do not follow the grid: a block is a place in the table
    /// rather than a thing, and rows moving under it would quietly change what
    /// the comparison is of. The user takes it again when they want it again.
    Cells {
        /// Weak because the comparison outlives the grid's tab, which is exactly
        /// the case "take it again" has to refuse rather than guess at.
        grid: WeakEntity<TabularDataPreviewPane>,
        comparison: CellComparison,
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
    /// How many characters wide each column's widest value is, full width ones
    /// counting as two. Kept in characters rather than pixels so that the
    /// columns follow the buffer font the moment it changes, rather than when
    /// the comparison is next taken.
    column_characters: Vec<usize>,
    /// How many digits the largest row number either pane shows has.
    row_number_digits: usize,
    /// How wide one character of a value is drawn, which is what the columns
    /// are sized in. The comparison is read only, so unlike the grid's columns
    /// these cannot be dragged wider, and a column sized from a guess narrower
    /// than the font hid the end of a value - the part that changed included -
    /// behind an ellipsis.
    character_width: Pixels,
    /// The buffer font and size `character_width` was measured in.
    character_width_measured_in: Option<(Font, Pixels)>,
    /// `None` while every row is shown; the changed rows only when the view is
    /// filtered down to the differences.
    shown_rows: Option<Vec<usize>>,
    /// Which change "previous" and "next" last stopped at, as an index into
    /// [`TableDiff::change_blocks`].
    current_change: Option<usize>,
    /// How tall one line of a cell is. A row is as many of these tall as its
    /// tallest value has lines, both panes together, so that the two halves of
    /// a row stay side by side.
    line_height: Pixels,
    list_state: ListState,
    /// The width the list's rows were last given a height estimate at, or
    /// `None` before they ever were.
    ///
    /// The list forgets every height it has not measured whenever it is laid
    /// out at a new width, its first layout included, and a list that only
    /// knows the rows it has drawn offers a scrollbar sized to those rows and
    /// a wheel that stops at the last of them.
    list_width_estimated_at: Option<Pixels>,
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

    /// Compares blocks of cells inside one grid.
    ///
    /// Every block picked with Ctrl, every row of rows picked by their numbers,
    /// every column of a block several columns wide, or every cell of a block
    /// one column wide is something either side can show. The first two start
    /// out side by side and the pane titles pick among the rest. A single cell
    /// is compared against the clipboard.
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
        let Some(comparison) =
            grid.update(cx, |grid, cx| match cell_comparison(grid, &clipboard) {
                Ok(comparison) => Some(comparison),
                Err(refusal) => {
                    grid.set_status_message(refusal.message(), cx);
                    None
                }
            })
        else {
            return;
        };

        let view = cx.new(|cx| Self::from_cells(grid.downgrade(), comparison, cx));
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

        let source = DiffSource::Files {
            left,
            left_text: SharedString::default(),
            buffer,
            left_delimiter,
            right_delimiter,
        };
        let mut view = Self::empty(title, left_title, right_title, source, cx);
        view.first_row_is_header = first_row_is_header;
        view._subscriptions = subscriptions;
        view.recompute(false, cx);
        view
    }

    fn from_cells(
        grid: WeakEntity<TabularDataPreviewPane>,
        comparison: CellComparison,
        cx: &mut Context<Self>,
    ) -> Self {
        let title = comparison.title();
        let left_title = comparison.pane_title(PaneSide::Left);
        let right_title = comparison.pane_title(PaneSide::Right);
        let source = DiffSource::Cells { grid, comparison };
        let mut view = Self::empty(title, left_title, right_title, source, cx);
        view.left_taken_at = Some(Local::now());
        view.recompute(false, cx);
        view
    }

    fn empty(
        title: SharedString,
        left_title: SharedString,
        right_title: SharedString,
        source: DiffSource,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self {
            focus_handle: cx.focus_handle(),
            title,
            left_title,
            right_title,
            source,
            first_row_is_header: false,
            left: DiffSide::default(),
            right: DiffSide::default(),
            diff: TableDiff::compute(&DiffSide::default(), &DiffSide::default()),
            left_taken_at: None,
            left_is_stale: false,
            is_computing: false,
            column_characters: Vec::new(),
            row_number_digits: MINIMUM_ROW_NUMBER_DIGITS,
            character_width: px(0.),
            character_width_measured_in: None,
            shown_rows: None,
            current_change: None,
            line_height: cell_line_height(cx),
            list_state: ListState::new(0, ListAlignment::Top, LIST_OVERDRAW),
            list_width_estimated_at: None,
            horizontal_scroll: ScrollHandle::new(),
            recompute_task: None,
            _subscriptions: Vec::new(),
        };
        view.measure_character_width(cx);
        view
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
            DiffSource::Cells { comparison, .. } => DiffInput::Rows {
                left_rows: comparison.rows(PaneSide::Left).to_vec(),
                right_rows: comparison.rows(PaneSide::Right).to_vec(),
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
        if let DiffSource::Cells { grid, comparison } = &mut self.source {
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
            // Every candidate is read again, not only the two on screen: one
            // picked afterwards is shown from what was read here, and it would
            // otherwise be older than the time the left pane's title gives.
            let pane = grid.read(cx);
            for candidate in &mut comparison.candidates {
                candidate.rows = read_cell_block(pane, candidate.block, &clipboard);
            }
            self.left_taken_at = Some(Local::now());
        }
        self.recompute(false, cx);
    }

    /// Puts another of the things the selection offered on one side.
    ///
    /// It is compared as it was read when the comparison was last taken, like
    /// the two it replaces: picking is no reason to start following the grid,
    /// and it has to keep working once the grid's tab is closed.
    fn pick_cell_candidate(&mut self, side: PaneSide, index: usize, cx: &mut Context<Self>) {
        let DiffSource::Cells { comparison, .. } = &mut self.source else {
            return;
        };
        if !comparison.pick(side, index) {
            return;
        }
        self.title = comparison.title();
        self.left_title = comparison.pane_title(PaneSide::Left);
        self.right_title = comparison.pane_title(PaneSide::Right);
        cx.emit(ItemEvent::UpdateTab);
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
                this.column_characters = column_characters(&left, &right, diff.column_count);
                this.row_number_digits = row_number_digits(&left, &right);
                this.left = left;
                this.right = right;
                this.diff = diff;
                this.is_computing = false;
                this.refresh_shown_rows();
                this.sync_list_state();
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
        let scroll_top = self.list_state.logical_scroll_top();
        let top_row = self.row_at(scroll_top.item_ix);
        self.shown_rows = match self.shown_rows {
            Some(_) => None,
            None => Some(self.diff.changed_row_indices()),
        };
        // The positions on screen stand for different rows once the filter
        // changes, so the view is kept on the row it was showing - or, when
        // that row is filtered out, on the first change after it - rather than
        // on a position that now means something else.
        let scroll_top = match top_row {
            Some(row) => {
                let item_ix = self.shown_index_of(row);
                let offset_in_item = if self.row_at(item_ix) == Some(row) {
                    scroll_top.offset_in_item
                } else {
                    px(0.)
                };
                ListOffset {
                    item_ix,
                    offset_in_item,
                }
            }
            None => scroll_top,
        };
        self.reset_list(scroll_top);
        cx.notify();
    }

    /// Brings the list up to the rows about to be shown after a comparison was
    /// taken again, holding on to where it was scrolled to.
    fn sync_list_state(&mut self) {
        let row_count = self.shown_row_count();
        if self.list_state.item_count() == row_count {
            // The rows are the same in number but their values may not be,
            // and a value that gained a line makes its row taller.
            self.list_state.remeasure_items(0..row_count);
        } else {
            // `reset` throws the scroll position away along with the rows.
            let scroll_top = self.list_state.logical_scroll_top();
            self.reset_list(scroll_top);
        }
    }

    /// Replaces every row of the list with a fresh one estimated at a single
    /// line, which the rows correct as they are drawn, and scrolls it to
    /// `scroll_top`.
    fn reset_list(&mut self, scroll_top: ListOffset) {
        self.list_state
            .reset_with_uniform_height(self.shown_row_count(), self.row_height(1));
        self.list_state.scroll_to(scroll_top);
    }

    /// Puts the row height estimate back after the list dropped it for being
    /// laid out at a new width (see [`Self::list_width_estimated_at`]).
    fn restore_row_height_estimate(&mut self) {
        let list_width = self.list_state.viewport_bounds().size.width;
        if list_width <= px(0.) || self.list_width_estimated_at == Some(list_width) {
            return;
        }
        self.list_width_estimated_at = Some(list_width);
        let scroll_top = self.list_state.logical_scroll_top();
        self.reset_list(scroll_top);
    }

    fn row_height(&self, lines: usize) -> Pixels {
        self.line_height * lines + CELL_FRAME_HEIGHT
    }

    /// Measures how wide one character of the buffer font is drawn, unless it
    /// already was in the font and size the cells are drawn in now.
    fn measure_character_width(&mut self, cx: &App) {
        let settings = ThemeSettings::get_global(cx);
        let font = &settings.buffer_font;
        let font_size = settings.buffer_font_size(cx);
        if let Some((measured_font, measured_size)) = &self.character_width_measured_in
            && measured_font == font
            && *measured_size == font_size
        {
            return;
        }
        let text_system = cx.text_system();
        let font_id = text_system.resolve_font(font);
        self.character_width = match text_system.em_advance(font_id, font_size) {
            Ok(width) => width,
            Err(error) => {
                log::error!(
                    "failed to measure a character of {:?} for a table comparison: {error}",
                    font.family
                );
                font_size * FALLBACK_CHARACTER_WIDTH_PER_FONT_SIZE
            }
        };
        self.character_width_measured_in = Some((font.clone(), font_size));
    }

    /// How many lines the row at a position on screen is tall.
    fn shown_row_line_count(&self, shown_index: usize) -> usize {
        let Some(row) = self
            .row_at(shown_index)
            .and_then(|row| self.diff.rows.get(row))
        else {
            return 1;
        };
        let left = row
            .left_row
            .map_or(&[] as &[SharedString], |row| self.left.row(row));
        let right = row
            .right_row
            .map_or(&[] as &[SharedString], |row| self.right.row(row));
        tallest_cell_line_count([left, right])
    }

    /// Scrolls a row to the middle of the view, which leaves what comes before
    /// and after a change in sight along with it.
    ///
    /// Worked out from the rows' own line counts rather than asked of the list,
    /// which knows the heights only of the rows it has drawn and would count
    /// the rest as nothing.
    fn scroll_row_to_center(&self, shown_index: usize) {
        let viewport_height = self.list_state.viewport_bounds().size.height;
        let row_height = self.row_height(self.shown_row_line_count(shown_index));
        let mut space_above = ((viewport_height - row_height) * 0.5).max(px(0.));
        let mut scroll_top = ListOffset {
            item_ix: shown_index,
            offset_in_item: px(0.),
        };
        while space_above > px(0.) && scroll_top.item_ix > 0 {
            scroll_top.item_ix -= 1;
            let height = self.row_height(self.shown_row_line_count(scroll_top.item_ix));
            if height >= space_above {
                scroll_top.offset_in_item = height - space_above;
                break;
            }
            space_above -= height;
        }
        self.list_state.scroll_to(scroll_top);
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
            Some(ChangeStop::Header) => self.list_state.scroll_to(ListOffset::default()),
            Some(&ChangeStop::Row(row)) => self.scroll_row_to_center(self.shown_index_of(row)),
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
        let line_height = cell_line_height(cx);
        if line_height != self.line_height {
            self.line_height = line_height;
            // Every row is a whole number of lines tall, so a new font size
            // changes the height of all of them.
            self.list_state.remeasure();
        }
        self.measure_character_width(cx);
        if self.diff.column_count > 0 {
            self.restore_row_height_estimate();
        }
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
        h_flex()
            .flex_none()
            .w_full()
            .bg(colors.surface_background)
            .font_ui(cx)
            .child(self.render_pane_title(PaneSide::Left, self.left_pane_title(), cx))
            .child(self.render_divider(cx))
            .child(self.render_pane_title(PaneSide::Right, self.right_title.clone(), cx))
            .into_any_element()
    }

    /// One pane's title. When the selection offered more than the two things
    /// on screen, it is also where the side is switched to another of them.
    fn render_pane_title(
        &self,
        side: PaneSide,
        text: SharedString,
        cx: &Context<Self>,
    ) -> AnyElement {
        let title = div().flex_1().min_w_0().px_2().py_0p5().overflow_hidden();
        let comparison = match &self.source {
            DiffSource::Cells { comparison, .. } if comparison.candidates.len() > 2 => comparison,
            _ => {
                return title
                    .child(
                        Label::new(text)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .single_line(),
                    )
                    .into_any_element();
            }
        };

        let (id, debug_selector) = match side {
            PaneSide::Left => ("tabular-diff-left-pick", DIFF_LEFT_PICK_DEBUG_SELECTOR),
            PaneSide::Right => ("tabular-diff-right-pick", DIFF_RIGHT_PICK_DEBUG_SELECTOR),
        };
        let names: Vec<SharedString> = comparison
            .candidates
            .iter()
            .map(|candidate| candidate.pane_title.clone())
            .collect();
        let picked = comparison.picked(side);
        // Weak because the menu outlives this frame: one left open over a tab
        // being closed must not keep the comparison alive.
        let view = cx.weak_entity();
        title
            .debug_selector(|| debug_selector.to_owned())
            .child(
                PopoverMenu::new(ElementId::Name(id.into()))
                    .trigger_with_tooltip(
                        Button::new(ElementId::NamedInteger(id.into(), 0), text)
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Compact)
                            .label_size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(true)
                            .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall)),
                        Tooltip::text("この側で比べるものを選びます"),
                    )
                    .menu(move |window, cx| {
                        let names = names.clone();
                        let view = view.clone();
                        Some(ContextMenu::build(window, cx, move |menu, _window, _cx| {
                            names
                                .into_iter()
                                .enumerate()
                                .fold(menu, |menu, (index, name)| {
                                    let view = view.clone();
                                    menu.toggleable_entry(
                                        name,
                                        index == picked,
                                        IconPosition::Start,
                                        None,
                                        move |_window, cx| {
                                            view.update(cx, |view, cx| {
                                                view.pick_cell_candidate(side, index, cx);
                                            })
                                            .ok();
                                        },
                                    )
                                })
                        }))
                    }),
            )
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
        let lines = tallest_cell_line_count(
            [(left_present, left_cells), (right_present, right_cells)]
                .into_iter()
                .filter_map(|(present, cells)| present.then_some(cells)),
        );

        h_flex()
            .flex_none()
            .w_full()
            .h(self.row_height(lines))
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

        div()
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(
                list(
                    self.list_state.clone(),
                    cx.processor(|this, shown_index: usize, _window, cx| {
                        this.render_aligned_row(shown_index, cx)
                    }),
                )
                .size_full(),
            )
            .child(self.render_list_width_watcher(cx))
            .vertical_scrollbar_for(&self.list_state, window, cx)
            .child(self.render_horizontal_scrollbars(window, cx))
            .into_any_element()
    }

    /// Draws nothing, and asks for another frame when the list has just been
    /// laid out at a width its rows' height estimate was not made for, which is
    /// when the list throws the estimate away (see
    /// [`Self::list_width_estimated_at`]).
    ///
    /// The width the list ends up at is only known once it has been laid out,
    /// after this view was rendered, so this sits after the list: by the time
    /// it is laid out itself, the list's layout for this frame is there to read.
    fn render_list_width_watcher(&self, cx: &Context<Self>) -> AnyElement {
        let list_state = self.list_state.clone();
        let estimated_at = self.list_width_estimated_at;
        let view = cx.entity().downgrade();
        canvas(
            move |_bounds, window, _cx| {
                let list_width = list_state.viewport_bounds().size.width;
                if list_width > px(0.) && estimated_at != Some(list_width) {
                    // A view notified while the frame is being laid out is
                    // forgotten along with the frame, so the notification goes
                    // out with the next one instead.
                    window.on_next_frame(move |_window, cx| {
                        // The tab may have been closed by then, and a view
                        // that is gone has nothing left to redraw.
                        view.update(cx, |_, cx| cx.notify()).ok();
                    });
                }
            },
            |_bounds, _state, _window, _cx| {},
        )
        .absolute()
        .size_0()
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
            .child(div().w(self.row_number_width()).flex_none())
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
            return div().h(self.row_height(1)).w_full().into_any_element();
        };

        h_flex()
            .h(self.row_height(self.shown_row_line_count(shown_index)))
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
        let characters = self.column_characters.get(column).copied().unwrap_or(0);
        (self.character_width * characters as f32 + px(COLUMN_WIDTH_PADDING))
            .clamp(px(MINIMUM_COLUMN_WIDTH), px(MAXIMUM_COLUMN_WIDTH))
    }

    /// Wide enough for the largest number either pane puts in front of a row.
    fn row_number_width(&self) -> Pixels {
        self.character_width * self.row_number_digits as f32 + px(ROW_NUMBER_PADDING)
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
        let highlight = HighlightStyle {
            background_color: Some(match side {
                PaneSide::Left => status.deleted_background,
                PaneSide::Right => status.created_background,
            }),
            ..HighlightStyle::default()
        };

        let cells = (0..self.diff.column_count).map(|column| {
            let width = self.column_width(column);
            let value = if row.present {
                cell_at(row.cells, column)
            } else {
                ""
            };
            let is_changed = row.kind == DiffRowKind::Modified && row.is_changed(column);
            let highlights = if is_changed {
                changed_character_ranges(side, value, cell_at(row.other_cells, column))
            } else {
                Vec::new()
            };
            let lines = highlighted_lines(value, &highlights)
                .into_iter()
                .map(|(line, ranges)| {
                    if ranges.is_empty() {
                        SharedString::from(line.to_owned()).into_any_element()
                    } else {
                        StyledText::new(line.to_owned())
                            .with_highlights(ranges.into_iter().map(|range| (range, highlight)))
                            .into_any_element()
                    }
                });
            v_flex()
                .debug_selector(|| cell_debug_selector(side, element_index, column))
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
                .children(lines)
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
            // The row was sized in lines of exactly this height, so the text
            // has to be laid out in them too or a value of many lines would
            // outgrow its row.
            .line_height(self.line_height)
            .when_some(background, |this, background| this.bg(background))
            .child(
                div()
                    .w(self.row_number_width())
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

/// Marks one cell of one pane, so that a test can check the width it was drawn
/// at. `element_index` is the row's position on screen, or
/// [`HEADER_ELEMENT_INDEX`] for the header strip.
fn cell_debug_selector(side: PaneSide, element_index: usize, column: usize) -> String {
    let side = match side {
        PaneSide::Left => "left",
        PaneSide::Right => "right",
    };
    format!("tabular-diff-{side}-cell-{element_index}-{column}")
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

/// The characters of a changed cell that differ from the other pane's value, as
/// byte ranges into `value`, so that a long value whose middle changed does not
/// leave the user comparing two strings by eye.
fn changed_character_ranges(side: PaneSide, value: &str, other: &str) -> Vec<Range<usize>> {
    match side {
        PaneSide::Left => {
            let CellCharDiff { left, .. } = cell_char_diff(value, other);
            left
        }
        PaneSide::Right => {
            let CellCharDiff { right, .. } = cell_char_diff(other, value);
            right
        }
    }
}

/// How many lines a value is drawn on.
fn cell_line_count(value: &str) -> usize {
    value.split('\n').count()
}

/// How many lines the tallest of these cells is drawn on, which is how tall the
/// row holding them is: a comparison shows every value in full, since clipping
/// one could hide the very line that changed.
fn tallest_cell_line_count<'a>(rows: impl IntoIterator<Item = &'a [SharedString]>) -> usize {
    rows.into_iter()
        .flatten()
        .map(|cell| cell_line_count(cell))
        .max()
        .unwrap_or(1)
        .max(1)
}

/// A value split into the lines it is drawn as, each with the parts of
/// `highlights` that fall on it made relative to the line.
///
/// Every line is drawn as a text of its own because a text is truncated as
/// though it were one line: a value wider than its column would be cut off at
/// the end of its first line and the rest of it, changes included, never drawn.
fn highlighted_lines<'a>(
    value: &'a str,
    highlights: &[Range<usize>],
) -> Vec<(&'a str, Vec<Range<usize>>)> {
    let mut lines = Vec::new();
    let mut line_start = 0;
    for line in value.split('\n') {
        // A carriage return ahead of the line feed belongs to the line break,
        // and drawn as part of the line it would show up as a stray glyph.
        let shown = line.strip_suffix('\r').unwrap_or(line);
        let shown_end = line_start + shown.len();
        let ranges = highlights
            .iter()
            .filter_map(|range| {
                let start = range.start.max(line_start);
                let end = range.end.min(shown_end);
                (start < end).then(|| start - line_start..end - line_start)
            })
            .collect();
        lines.push((shown, ranges));
        line_start += line.len() + '\n'.len_utf8();
    }
    lines
}

/// How many characters wide the widest value of each column is, over both
/// sides, full width characters counting as two.
fn column_characters(left: &DiffSide, right: &DiffSide, column_count: usize) -> Vec<usize> {
    (0..column_count)
        .map(|column| {
            [left, right]
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
                .unwrap_or(0)
        })
        .collect()
}

/// How many digits the largest number either pane puts in front of a row has.
/// That is the last row's line rather than the row count: one quoted line break
/// puts the two apart.
fn row_number_digits(left: &DiffSide, right: &DiffSide) -> usize {
    let last = left.last_row_number().max(right.last_row_number()).max(1);
    last.to_string().len().max(MINIMUM_ROW_NUMBER_DIGITS)
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

/// One thing a comparison of cells can put on either side: where it was read
/// from, so that "再比較" can read it again, and the values it held then.
struct CellCandidate {
    /// What the pane's title and the pick list call it.
    pane_title: SharedString,
    /// What the tab calls it, which is shorter where it can be: the tab says
    /// "セルの比較" and names both sides, so "列" would only crowd a column's
    /// letter and a cell's address already says its row. A whole row has no
    /// shorter name than "行 N".
    tab_title: SharedString,
    block: CellBlock,
    rows: Vec<Vec<SharedString>>,
}

/// What a comparison of cells inside one grid is made of: everything the
/// selection offered to compare, and which two of those are side by side.
struct CellComparison {
    /// In the order the selection offered them, which is the order the pick
    /// lists show them in.
    candidates: Vec<CellCandidate>,
    left: usize,
    right: usize,
}

impl CellComparison {
    fn picked(&self, side: PaneSide) -> usize {
        match side {
            PaneSide::Left => self.left,
            PaneSide::Right => self.right,
        }
    }

    fn candidate(&self, side: PaneSide) -> Option<&CellCandidate> {
        self.candidates.get(self.picked(side))
    }

    fn rows(&self, side: PaneSide) -> &[Vec<SharedString>] {
        self.candidate(side)
            .map(|candidate| candidate.rows.as_slice())
            .unwrap_or_default()
    }

    fn pane_title(&self, side: PaneSide) -> SharedString {
        self.candidate(side)
            .map(|candidate| candidate.pane_title.clone())
            .unwrap_or_default()
    }

    fn title(&self) -> SharedString {
        let tab_title = |side| {
            self.candidate(side)
                .map(|candidate| candidate.tab_title.clone())
                .unwrap_or_default()
        };
        format!(
            "セルの比較 ({} ↔ {})",
            tab_title(PaneSide::Left),
            tab_title(PaneSide::Right)
        )
        .into()
    }

    /// Puts the candidate at `index` on `side`, and says whether that changed
    /// anything.
    ///
    /// Picking the one the other side already shows swaps the two: a block
    /// compared against itself has nothing to show, and swapping is what
    /// picking it there most likely meant.
    fn pick(&mut self, side: PaneSide, index: usize) -> bool {
        if index >= self.candidates.len() {
            return false;
        }
        let (this_side, other_side) = match side {
            PaneSide::Left => (&mut self.left, &mut self.right),
            PaneSide::Right => (&mut self.right, &mut self.left),
        };
        if *this_side == index {
            return false;
        }
        if *other_side == index {
            *other_side = *this_side;
        }
        *this_side = index;
        true
    }
}

/// Why a selection is not compared: it has nothing in it to compare, or more
/// than is worth reading.
#[derive(Debug, PartialEq, Eq)]
enum CellComparisonRefusal {
    /// A single cell, with nothing on the clipboard to hold it against.
    EmptyClipboard,
    /// More things than the pick lists can sensibly offer.
    TooManyCandidates(usize),
    /// More cells than can be read ahead for candidates that may never be
    /// shown.
    TooManyCells(usize),
}

impl CellComparisonRefusal {
    fn message(&self) -> SharedString {
        match self {
            CellComparisonRefusal::EmptyClipboard => {
                "1 つのセルはクリップボードと比べますが、クリップボードが空です \
                 (Ctrl で複数の範囲を選ぶか、2 列以上か 2 行以上を選ぶと、その中で比べます)"
                    .into()
            }
            CellComparisonRefusal::TooManyCandidates(count) => format!(
                "比べる候補が {count} 個になります。{MAX_CELL_CANDIDATES} 個までになるように\
                 選び直してください (Ctrl で選んだ範囲、行番号で選んだ行、2 列以上なら列、\
                 1 列ならセルが候補です)"
            )
            .into(),
            CellComparisonRefusal::TooManyCells(count) => format!(
                "比べる候補のセルが合わせて {} 個になります。3 つ以上を比べるときは {} セルまでです \
                 (範囲を小さくするか、比べる 2 列、2 行、2 つの範囲のどれかだけを選んでください)",
                crate::grid::grouped_digits(*count),
                crate::grid::grouped_digits(MAX_CELL_CANDIDATE_CELLS)
            )
            .into(),
        }
    }

    /// Whether a selection offering `candidate_count` things to compare,
    /// `cell_count` cells between them, is small enough to be read.
    ///
    /// Decided from the shape alone, so that a selection with too much in it is
    /// turned away before any of it is read.
    fn check(candidate_count: usize, cell_count: usize) -> Result<(), Self> {
        if candidate_count > MAX_CELL_CANDIDATES {
            return Err(CellComparisonRefusal::TooManyCandidates(candidate_count));
        }
        if candidate_count > 2 && cell_count > MAX_CELL_CANDIDATE_CELLS {
            return Err(CellComparisonRefusal::TooManyCells(cell_count));
        }
        Ok(())
    }
}

/// Reads the grid's selection as the things to compare.
///
/// Kept a free function over the grid's public reads so that it can be tested
/// without a window.
fn cell_comparison(
    grid: &TabularDataPreviewPane,
    clipboard: &str,
) -> Result<CellComparison, CellComparisonRefusal> {
    let rect = grid.selection_rect();
    let blocks = grid.selection_rects();
    // Rows are named the way the grid's own address in the toolbar names them,
    // which counts the header as line 1 so that the name is the line of the
    // file. A title pointing at a different line than the toolbar does would be
    // worse than no title at all.
    let row_offset = usize::from(grid.has_header_row());
    // Rows picked by their numbers say that the rows are what is compared,
    // which the shape cannot: a whole row is every column wide, and would
    // otherwise be read as columns. A single one has nothing of its own to be
    // compared with, so it is cut into columns like any other one row block.
    let whole_rows = blocks.len() < 2
        && grid.current_selection_range().kind == GridSelectionKind::Rows
        && rect.row_count() >= 2;

    let candidate_count = if blocks.len() >= 2 {
        blocks.len()
    } else if whole_rows {
        rect.row_count()
    } else if rect.col_count() >= 2 {
        rect.col_count()
    } else {
        rect.row_count()
    };
    // Every block is read whole, whichever way it is cut into candidates.
    CellComparisonRefusal::check(candidate_count, grid.selection_area())?;

    let candidate =
        |block: CellBlock, pane_title: SharedString, tab_title: SharedString| CellCandidate {
            rows: read_cell_block(grid, block, clipboard),
            pane_title,
            tab_title,
            block,
        };

    // Blocks picked with Ctrl say outright which things to compare, so they
    // win over every guess made from the shape of one block. Blocks of
    // different shapes are compared as they are: the row matching already pads
    // whichever side runs out first.
    let candidates: Vec<CellCandidate> = if blocks.len() >= 2 {
        blocks
            .iter()
            .map(|block| {
                let first = format!(
                    "{}{}",
                    column_label(block.first_col),
                    block.first_row + row_offset
                );
                let address =
                    SharedString::from(if block.row_count() == 1 && block.col_count() == 1 {
                        first
                    } else {
                        format!(
                            "{first}:{}{}",
                            column_label(block.last_col),
                            block.last_row + row_offset
                        )
                    });
                candidate(CellBlock::Rect(*block), address.clone(), address)
            })
            .collect()
    } else if whole_rows {
        rect.rows()
            .map(|row| {
                let name = SharedString::from(format!("行 {}", row + row_offset));
                let row_across = GridRect {
                    first_row: row,
                    last_row: row,
                    first_col: rect.first_col,
                    last_col: rect.last_col,
                };
                candidate(CellBlock::Rect(row_across), name.clone(), name)
            })
            .collect()
    } else if rect.col_count() >= 2 {
        // Columns side by side are the comparison the user is most likely after
        // in a spreadsheet, so a block that is both several columns wide and
        // several rows tall is read as columns rather than as rows.
        rect.cols()
            .map(|column| {
                let label = column_label(column);
                let column_down = GridRect {
                    first_row: rect.first_row,
                    last_row: rect.last_row,
                    first_col: column,
                    last_col: column,
                };
                candidate(
                    CellBlock::Rect(column_down),
                    format!("列 {label}").into(),
                    label.into(),
                )
            })
            .collect()
    } else if rect.row_count() >= 2 {
        let column = rect.first_col;
        rect.rows()
            .map(|row| {
                let number = row + row_offset;
                let cell = GridRect {
                    first_row: row,
                    last_row: row,
                    first_col: column,
                    last_col: column,
                };
                candidate(
                    CellBlock::Rect(cell),
                    format!("行 {number}").into(),
                    format!("{}{number}", column_label(column)).into(),
                )
            })
            .collect()
    } else {
        if clipboard.is_empty() {
            return Err(CellComparisonRefusal::EmptyClipboard);
        }
        let address = SharedString::from(grid.cursor_address());
        vec![
            candidate(
                CellBlock::Clipboard,
                "クリップボード".into(),
                "クリップボード".into(),
            ),
            candidate(CellBlock::Rect(rect), address.clone(), address),
        ]
    };

    Ok(CellComparison {
        candidates,
        left: 0,
        right: 1,
    })
}

impl EventEmitter<ItemEvent> for TableDiffView {}

impl Focusable for TableDiffView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for TableDiffView {
    type Event = ItemEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

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
    use settings::SettingsStore;
    use std::{cell::Cell, rc::Rc};
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
            &DiffSide::from_rows(comparison.rows(PaneSide::Left).to_vec(), false),
            &DiffSide::from_rows(comparison.rows(PaneSide::Right).to_vec(), false),
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

    /// A value holding line breaks is shown in full, so its row is as many lines
    /// tall as the value has - in both panes, or the rows after it would stop
    /// lining up. A row one line tall showed only the first line of the value,
    /// and hid a change on any line after it.
    #[gpui::test]
    async fn a_value_of_several_lines_makes_its_row_that_many_lines_tall(cx: &mut TestAppContext) {
        let (_workspace, _pane, view, cx) = open_head_comparison(
            "name,note\na,\"one\ntwo\"\nb,x\nc,x",
            "name,note\na,\"one\nTWO\"\nb,x\nc,\"x\ny\nz\"\nd,\"p\nq\"",
            cx,
        )
        .await;

        view.read_with(&cx, |view, _| {
            let first = view.diff.rows.first().expect("the table has rows");
            assert_eq!(first.kind, DiffRowKind::Modified);
            assert_eq!(
                first.changed_columns,
                [1],
                "only the second line of the note differs, and that has to count"
            );
            let kinds: Vec<DiffRowKind> = view.diff.rows.iter().map(|row| row.kind).collect();
            assert_eq!(
                kinds,
                [
                    DiffRowKind::Modified,
                    DiffRowKind::Unchanged,
                    DiffRowKind::Modified,
                    DiffRowKind::Added,
                ]
            );

            // The third row is tall on the right only and the fourth is on the
            // right only, and both still size the row the two panes share.
            let expected_lines = [2, 1, 3, 2];
            let mut previous_bottom = None;
            for (shown_index, lines) in expected_lines.into_iter().enumerate() {
                let row = view
                    .list_state
                    .bounds_for_item(shown_index)
                    .expect("every row of the table should have been drawn");
                assert_eq!(
                    row.size.height,
                    view.row_height(lines),
                    "row {shown_index} should be {lines} lines tall ({row:?})"
                );
                if let Some(previous_bottom) = previous_bottom {
                    assert_eq!(
                        row.top(),
                        previous_bottom,
                        "row {shown_index} should start where the row above it ends"
                    );
                }
                previous_bottom = Some(row.bottom());
            }
        });
    }

    /// The header strip grows with a header value of several lines the way a
    /// row does, and both of its panes grow together.
    #[gpui::test]
    async fn a_header_of_several_lines_makes_its_strip_that_many_lines_tall(
        cx: &mut TestAppContext,
    ) {
        let (_workspace, _pane, view, mut cx) = open_head_comparison(
            "name,\"first\nsecond\"\na,1",
            "name,\"first\nsecond\"\na,2",
            cx,
        )
        .await;

        let left = cx
            .debug_bounds(DIFF_LEFT_PANE_DEBUG_SELECTOR)
            .expect("the left pane should have been drawn");
        let right = cx
            .debug_bounds(DIFF_RIGHT_PANE_DEBUG_SELECTOR)
            .expect("the right pane should have been drawn");
        let line_height = view.read_with(&cx, |view, _| view.line_height);

        assert!(
            left.size.height >= line_height * 2.,
            "the header strip should hold both lines of the header ({left:?})"
        );
        assert_eq!(left.size.height, right.size.height);
    }

    /// The characters picked out in a changed value are measured over the whole
    /// value, and each line of it is drawn on its own, so a change on a later
    /// line has to be picked out on that line and at its place in it.
    #[test]
    fn a_change_on_a_later_line_of_a_value_is_picked_out_on_that_line() {
        let left = DiffSide::from_rows(vec![vec!["one\ntwo".into()]], false);
        let right = DiffSide::from_rows(vec![vec!["one\ntwO".into()]], false);
        let diff = TableDiff::compute(&left, &right);
        assert_eq!(diff.rows.len(), 1);
        let row = diff.rows.first().expect("the two values make one row");
        assert_eq!(row.kind, DiffRowKind::Modified);
        assert_eq!(row.changed_columns, [0]);

        let lines = |side: PaneSide, value: &str, other: &str| {
            highlighted_lines(value, &changed_character_ranges(side, value, other))
                .into_iter()
                .map(|(line, ranges)| (line.to_owned(), ranges))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            lines(PaneSide::Left, "one\ntwo", "one\ntwO"),
            [("one".to_owned(), vec![]), ("two".to_owned(), vec![2..3])]
        );
        assert_eq!(
            lines(PaneSide::Right, "one\ntwO", "one\ntwo"),
            [("one".to_owned(), vec![]), ("twO".to_owned(), vec![2..3])]
        );
        // A change that runs across a line break is picked out on both lines.
        assert_eq!(
            lines(PaneSide::Left, "ab\ncd", "a"),
            [("ab".to_owned(), vec![1..2]), ("cd".to_owned(), vec![0..2])]
        );
        // A carriage return is part of the line break rather than of the line.
        assert_eq!(
            lines(PaneSide::Right, "one\r\ntwO", "one\r\ntwo"),
            [("one".to_owned(), vec![]), ("twO".to_owned(), vec![2..3])]
        );
        assert_eq!(cell_line_count("one\r\ntwo\n"), 3);
    }

    /// Every range handed to a line has to sit on its characters and inside it,
    /// since a line is drawn as a text of its own and a range off a character
    /// boundary or past its end panics there.
    #[test]
    fn picked_out_ranges_stay_inside_their_lines() {
        let lines = |value: &str, highlights: &[Range<usize>]| {
            highlighted_lines(value, highlights)
                .into_iter()
                .map(|(line, ranges)| (line.to_owned(), ranges))
                .collect::<Vec<_>>()
        };
        assert_eq!(lines("", &[]), [(String::new(), vec![])]);
        assert_eq!(
            lines("あい\nう", &[3..10]),
            [
                ("あい".to_owned(), vec![3..6]),
                ("う".to_owned(), vec![0..3])
            ]
        );
        // Nothing but a line break changed, and a line break is not drawn.
        assert_eq!(
            lines("a\r\nb", &[1..3]),
            [("a".to_owned(), vec![]), ("b".to_owned(), vec![])]
        );
        assert_eq!(
            highlighted_lines(
                "あい\r\nう",
                &changed_character_ranges(PaneSide::Left, "あい\r\nう", "あい\nう")
            ),
            [("あい", vec![]), ("う", vec![])]
        );
        // A value ending in a line break has an empty line at the end, which its
        // row makes room for.
        assert_eq!(
            lines("a\n", &[0..2]),
            [("a".to_owned(), vec![0..1]), (String::new(), vec![])]
        );
        assert_eq!(cell_line_count("a\n"), 2);
        assert_eq!(lines("ab", &[1..9]), [("ab".to_owned(), vec![1..2])]);
    }

    /// "Next" puts the change in the middle of the view even when the rows
    /// above it are taller than one line, which the list cannot know about rows
    /// it has not drawn.
    #[gpui::test]
    async fn stepping_to_a_change_below_tall_rows_centers_it(cx: &mut TestAppContext) {
        let mut committed = String::from("key,value");
        for row in 0..100 {
            if (50..70).contains(&row) {
                committed.push_str(&format!("\nkey-{row},\"first\nsecond\nthird\""));
            } else {
                committed.push_str(&format!("\nkey-{row},value-{row}"));
            }
        }
        let working_copy = committed.replace("value-70", "changed-70");
        let (_workspace, _pane, view, mut cx) =
            open_head_comparison(&committed, &working_copy, cx).await;

        view.update(&mut cx, |view, cx| view.go_to_change(true, cx));
        cx.run_until_parked();

        view.read_with(&cx, |view, _| {
            assert_eq!(view.diff.change_blocks(), [70]);
            let viewport = view.list_state.viewport_bounds();
            let row = view
                .list_state
                .bounds_for_item(70)
                .expect("the change should have been drawn");
            assert!(
                viewport.size.height > view.row_height(3) * 4.,
                "the view has to be taller than a few rows for this to be a test ({viewport:?})"
            );
            let off_center = (row.center().y - viewport.center().y).abs();
            assert!(
                off_center <= view.row_height(1),
                "the change should be in the middle of the view \
                 (row {row:?}, view {viewport:?})"
            );
        });
    }

    /// The scrollbar is sized by the whole table rather than by the rows drawn
    /// so far. The list forgets the heights of the rows it has not drawn every
    /// time it is laid out at a new width, its first layout included, which left
    /// a thumb sized to the first screenful of rows and a wheel that stopped at
    /// the last of them.
    #[gpui::test]
    async fn the_scrollbar_spans_the_whole_table(cx: &mut TestAppContext) {
        let table = numbered_table(200, |_| false);
        let (view, _buffer, cx) = comparison_in_a_window_of_its_own(&table, &table, cx).await;
        draw_until_settled(cx);

        let assert_spans_the_table = |when: &str, cx: &VisualTestContext| {
            view.read_with(cx, |view, _| {
                let viewport_height = view.list_state.viewport_bounds().size.height;
                let table_height = view.row_height(1) * view.shown_row_count();
                assert!(
                    viewport_height > px(0.) && viewport_height < table_height,
                    "the table has to reach past the view for this to be a test ({when})"
                );
                let scrollable_height =
                    view.list_state.max_offset_for_scrollbar().y + viewport_height;
                assert!(
                    (scrollable_height - table_height).abs() < px(1.),
                    "{when}, the scrollbar should span the {table_height:?} of the table \
                     rather than {scrollable_height:?}"
                );
            });
        };
        assert_spans_the_table("once the comparison has opened", cx);

        cx.simulate_resize(gpui::size(px(700.), px(500.)));
        cx.run_until_parked();
        draw_until_settled(cx);
        assert_spans_the_table("after the window was resized", cx);
    }

    /// Once the comparison has settled it asks for no more frames. It asks for
    /// one whenever the list has been laid out at a new width, to put the height
    /// estimate back, and were that frame to ask for another in turn, an open
    /// comparison would keep redrawing itself for as long as it stayed open.
    #[gpui::test]
    async fn a_settled_comparison_asks_for_no_more_frames(cx: &mut TestAppContext) {
        let table = numbered_table(200, |_| false);
        let (view, _buffer, cx) = comparison_in_a_window_of_its_own(&table, &table, cx).await;
        let notifications = Rc::new(Cell::new(0_usize));
        let _observation = cx.update(|_window, cx| {
            let notifications = notifications.clone();
            cx.observe(&view, move |_view, _cx| {
                notifications.set(notifications.get() + 1)
            })
        });

        let assert_settles = |when: &str, cx: &mut VisualTestContext| {
            let frames = draw_until_settled(cx);
            assert!(
                frames <= 1,
                "{when}, the comparison should settle within a frame rather than after {frames}"
            );
            let notified = notifications.get();
            for _ in 0..3 {
                let asked_for = cx.update(|window, cx| window.simulate_next_frame(cx));
                assert_eq!(
                    asked_for, 0,
                    "{when}, a settled comparison should ask for no frames"
                );
                cx.run_until_parked();
            }
            assert_eq!(
                notifications.get(),
                notified,
                "{when}, a settled comparison should not redraw itself"
            );
        };
        assert_settles("once the comparison has opened", cx);

        cx.simulate_resize(gpui::size(px(700.), px(500.)));
        cx.run_until_parked();
        assert_settles("after the window was resized", cx);
    }

    /// Showing only the differences and showing every row again keep the view on
    /// the row it was showing, or, when the filter takes that row away, on the
    /// first change after it: the same position stands for another row once the
    /// filter changes.
    #[gpui::test]
    async fn filtering_the_rows_keeps_the_view_on_the_row_it_was_showing(cx: &mut TestAppContext) {
        // Every third row changed, so that the changes alone still reach far
        // past the view and nothing pulls the view back up to fill it.
        let committed = numbered_table(900, |_| false);
        let working_copy = numbered_table(900, |row| row % 3 == 0);
        let (view, _buffer, cx) =
            comparison_in_a_window_of_its_own(&committed, &working_copy, cx).await;
        draw_until_settled(cx);
        view.read_with(cx, |view, _| assert_eq!(view.diff.modified_rows, 300));

        let scroll_to_row = |row: usize, offset_in_item: Pixels, cx: &mut VisualTestContext| {
            view.update(cx, |view, cx| {
                view.list_state.scroll_to(ListOffset {
                    item_ix: view.shown_index_of(row),
                    offset_in_item,
                });
                cx.notify();
            });
            cx.run_until_parked();
        };
        let toggle = |cx: &mut VisualTestContext| {
            view.update(cx, |view, cx| view.toggle_only_differences(cx));
            cx.run_until_parked();
        };
        let top_row = |cx: &VisualTestContext| {
            view.read_with(cx, |view, _| {
                let top = view.list_state.logical_scroll_top();
                (view.row_at(top.item_ix), top.offset_in_item)
            })
        };

        // Row 301 did not change, so the filter moves the view on to row 303.
        scroll_to_row(301, px(5.), cx);
        toggle(cx);
        assert_eq!(top_row(cx), (Some(303), px(0.)));
        toggle(cx);
        assert_eq!(top_row(cx), (Some(303), px(0.)));

        scroll_to_row(450, px(5.), cx);
        toggle(cx);
        assert_eq!(top_row(cx), (Some(450), px(5.)));
        toggle(cx);
        assert_eq!(top_row(cx), (Some(450), px(5.)));
    }

    /// An edit takes the comparison again, and the view stays where it was
    /// whether or not the edit changed how many rows there are.
    #[gpui::test]
    async fn an_edit_keeps_the_view_where_it_was(cx: &mut TestAppContext) {
        let table = numbered_table(300, |_| false);
        let (view, buffer, cx) = comparison_in_a_window_of_its_own(&table, &table, cx).await;
        draw_until_settled(cx);
        view.update(cx, |view, cx| {
            view.list_state.scroll_to(ListOffset {
                item_ix: 150,
                offset_in_item: px(5.),
            });
            cx.notify();
        });
        cx.run_until_parked();

        let edit = |text: String, cx: &mut VisualTestContext| {
            buffer.update(cx, |buffer, cx| buffer.set_text(text, cx));
            cx.run_until_parked();
            cx.executor().advance_clock(RECOMPUTE_DEBOUNCE * 2);
            cx.run_until_parked();
        };
        let scroll_top = |cx: &VisualTestContext| {
            view.read_with(cx, |view, _| {
                let top = view.list_state.logical_scroll_top();
                (top.item_ix, top.offset_in_item)
            })
        };

        edit(numbered_table(300, |row| row == 10), cx);
        view.read_with(cx, |view, _| assert_eq!(view.diff.modified_rows, 1));
        assert_eq!(scroll_top(cx), (150, px(5.)), "with as many rows as before");

        edit(numbered_table(301, |row| row == 10), cx);
        view.read_with(cx, |view, _| assert_eq!(view.diff.added_rows, 1));
        assert_eq!(scroll_top(cx), (150, px(5.)), "with a row more than before");
    }

    /// A new buffer font size changes the height of every row, including the
    /// ones off screen that the list measured at the old size and would go on
    /// believing.
    #[gpui::test]
    async fn a_new_font_size_measures_the_rows_again(cx: &mut TestAppContext) {
        let table = numbered_table(200, |_| false);
        let (view, _buffer, cx) = comparison_in_a_window_of_its_own(&table, &table, cx).await;
        draw_until_settled(cx);
        let measured_rows = |view: &TableDiffView| {
            (0..view.shown_row_count())
                .filter_map(|shown_index| view.list_state.bounds_for_item(shown_index))
                .map(|bounds| bounds.size.height)
                .collect::<Vec<_>>()
        };
        let old_row_height = view.read_with(cx, |view, _| {
            let viewport = view.list_state.viewport_bounds();
            let measured = measured_rows(view);
            assert!(
                view.row_height(1) * measured.len() > viewport.size.height,
                "the list has to have measured rows past the view for this to be a test"
            );
            view.row_height(1)
        });

        cx.update(|window, cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |content| {
                    content.theme.buffer_font_size = Some(30.0.into());
                });
            });
            window.refresh();
        });
        cx.run_until_parked();

        view.read_with(cx, |view, _| {
            let new_row_height = view.row_height(1);
            assert!(new_row_height > old_row_height);
            for height in measured_rows(view) {
                assert_eq!(
                    height, new_row_height,
                    "every row the list has a height for should have the new one"
                );
            }
        });
    }

    /// A value is given a column as wide as the buffer font draws it, at the
    /// size it is drawn at. The columns were sized at a guessed 8px a character,
    /// narrower than the buffer font's, which cut "line TWO changed" short
    /// behind an ellipsis - the part that changed with it - in a column the
    /// comparison gives no way to widen.
    #[gpui::test]
    async fn a_changed_value_fits_its_column_at_any_font_size(cx: &mut TestAppContext) {
        let value = "line TWO changed";
        let (view, _buffer, cx) = comparison_in_a_window_of_its_own(
            "key,note\na,line two",
            &format!("key,note\na,{value}"),
            cx,
        )
        .await;
        draw_until_settled(cx);

        let assert_fits = |when: &str, cx: &mut VisualTestContext| {
            let text_width = cx.update(|window, cx| {
                let settings = ThemeSettings::get_global(cx);
                let run = gpui::TextRun {
                    font: settings.buffer_font.clone(),
                    ..window.text_style().to_run(value.len())
                };
                window
                    .text_system()
                    .shape_line(value.into(), settings.buffer_font_size(cx), &[run], None)
                    .width
            });
            let cell = drawn_cell(PaneSide::Right, 0, 1, cx);
            // What the cell leaves its text: the padding on either side, and
            // the border on its right.
            let room = cell.size.width - px(4.) * 2. - px(1.);
            assert!(
                text_width <= room,
                "{when}, {value:?} is {text_width:?} wide and should fit the {room:?} its cell \
                 leaves it"
            );

            for (side, element_index) in [
                (PaneSide::Left, 0),
                (PaneSide::Left, HEADER_ELEMENT_INDEX),
                (PaneSide::Right, HEADER_ELEMENT_INDEX),
            ] {
                assert_eq!(
                    drawn_cell(side, element_index, 1, cx).size.width,
                    cell.size.width,
                    "{when}, the column should be as wide in both panes and in the header strip"
                );
            }
            let columns = drawn_cell(PaneSide::Right, 0, 0, cx).size.width + cell.size.width;
            let scrolled_over = view.read_with(cx, |view, _| {
                view.horizontal_scroll
                    .bounds_for_item(0)
                    .expect("the panes should have drawn their cells")
                    .size
                    .width
            });
            assert_eq!(
                scrolled_over, columns,
                "{when}, the panes should scroll over the columns as wide as they are drawn"
            );
        };
        assert_fits("at the font size it opened at", cx);

        // 15px is the size the app starts at. The test settings' 14px is one at
        // which the old guess happened to be just wide enough.
        for font_size in [15.0_f32, 30.0] {
            cx.update(|window, cx| {
                cx.update_global::<SettingsStore, _>(|store, cx| {
                    store.update_user_settings(cx, |content| {
                        content.theme.buffer_font_size = Some(font_size.into());
                    });
                });
                window.refresh();
            });
            cx.run_until_parked();
            draw_until_settled(cx);
            assert_fits(&format!("at a {font_size}px buffer font"), cx);
        }
    }

    /// The bounds one cell was drawn at. `debug_bounds` only takes a selector
    /// that lives for good, so the handful a test asks for are leaked.
    fn drawn_cell(
        side: PaneSide,
        element_index: usize,
        column: usize,
        cx: &mut VisualTestContext,
    ) -> gpui::Bounds<Pixels> {
        let selector: &'static str = cell_debug_selector(side, element_index, column).leak();
        cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} should have been drawn"))
    }

    /// A table of numbered rows under a header, with the rows `changed` picks
    /// out holding a different value from the rest.
    fn numbered_table(rows: usize, changed: impl Fn(usize) -> bool) -> String {
        let mut table = String::from("key,value");
        for row in 0..rows {
            let value = if changed(row) { "changed" } else { "value" };
            table.push_str(&format!("\nkey-{row},{value}-{row}"));
        }
        table
    }

    /// A comparison of two scratch files in a window of its own, so that the
    /// frames it asks for are the only ones asked for there. Returns the buffer
    /// of the right hand side, which an edit goes through.
    async fn comparison_in_a_window_of_its_own<'a>(
        left: &str,
        right: &str,
        cx: &'a mut TestAppContext,
    ) -> (
        Entity<TableDiffView>,
        Entity<Buffer>,
        &'a mut VisualTestContext,
    ) {
        // Only for the settings and the theme the view is drawn with.
        open_grid_in_workspace("a,b", cx).await;
        // The scrollbar's fade asks for frames of its own, and without motion
        // it asks for none.
        cx.update(|cx| cx.set_reduce_motion(true));
        let left = left.to_owned();
        let right = right.to_owned();
        let mut right_buffer = None;
        let (view, cx) = cx.add_window_view(|_window, cx| {
            let left_buffer = cx.new(|cx| Buffer::local(left, cx));
            let buffer = cx.new(|cx| Buffer::local(right, cx));
            right_buffer = Some(buffer.clone());
            TableDiffView::from_files(
                "比較".into(),
                "左".into(),
                "右".into(),
                LeftSource::File(left_buffer),
                buffer,
                ',',
                ',',
                true,
                cx,
            )
        });
        cx.run_until_parked();
        let right_buffer = right_buffer.expect("building the comparison made the buffer");
        (view, right_buffer, cx)
    }

    /// Hands over the frames the comparison asks for - tests have no frame loop
    /// to do it - until it asks for no more, and says how many that took. Gives
    /// up after a handful, which only a comparison that never settles needs.
    fn draw_until_settled(cx: &mut VisualTestContext) -> usize {
        let mut frames = 0;
        while frames < 10 && cx.update(|window, cx| window.simulate_next_frame(cx)) > 0 {
            cx.run_until_parked();
            frames += 1;
        }
        frames
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
            let rows_offset = view.list_state.scroll_px_offset_for_scrollbar().y;
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

        assert_eq!(comparison.rows(PaneSide::Left).len(), 3);
        assert_eq!(comparison.rows(PaneSide::Right).len(), 3);
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
        assert_eq!(comparison.pane_title(PaneSide::Left), "行 2");
        assert_eq!(comparison.pane_title(PaneSide::Right), "行 3");
        assert_eq!(comparison.title(), "セルの比較 (A2 ↔ A3)");
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
        assert_eq!(comparison.pane_title(PaneSide::Left), "クリップボード");
        let diff = cell_diff(&comparison);
        assert_eq!(diff.modified_rows, 1);

        pane.read_with(&cx, |pane, _| {
            assert!(
                cell_comparison(pane, "").is_err(),
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

        assert_eq!(comparison.pane_title(PaneSide::Left), "A2:A4");
        assert_eq!(comparison.pane_title(PaneSide::Right), "B2:B4");
        assert_eq!(comparison.rows(PaneSide::Left).len(), 3);
        assert_eq!(comparison.rows(PaneSide::Right).len(), 3);
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

        assert_eq!(comparison.rows(PaneSide::Left).len(), 3);
        assert_eq!(comparison.rows(PaneSide::Right).len(), 1);
    }

    /// Picks each of `columns` as a block of its own, from the first body row
    /// down to `last_body_row`, the way Ctrl and a drag down each column would.
    fn pick_columns_with_ctrl(
        pane: &Entity<TabularDataPreviewPane>,
        columns: Range<usize>,
        last_body_row: usize,
        cx: &mut VisualTestContext,
    ) {
        use crate::grid::{GridCell, SelectionChange};
        use crate::types::DisplayRow;

        pane.update(cx, |pane, cx| {
            let cell = |row: usize, col: usize| {
                GridCell::new(GridRow::Body(DisplayRow(row)), AnyColumn(col))
            };
            for column in columns.clone() {
                let change = if column == columns.start {
                    SelectionChange::Replace
                } else {
                    SelectionChange::Add
                };
                pane.change_cell_selection(cell(0, column), change, cx);
                pane.change_cell_selection(
                    cell(last_body_row, column),
                    SelectionChange::Extend,
                    cx,
                );
            }
        });
    }

    fn pane_titles(comparison: &CellComparison) -> Vec<&str> {
        comparison
            .candidates
            .iter()
            .map(|candidate| candidate.pane_title.as_ref())
            .collect()
    }

    /// Candidates holding the given values, standing in for cells whose grid is
    /// gone.
    fn clipboard_candidates(values: &[&str]) -> CellComparison {
        CellComparison {
            candidates: values
                .iter()
                .map(|value| CellCandidate {
                    pane_title: format!("候補 {value}").into(),
                    tab_title: SharedString::from(value.to_string()),
                    block: CellBlock::Clipboard,
                    rows: vec![vec![SharedString::from(value.to_string())]],
                })
                .collect(),
            left: 0,
            right: 1,
        }
    }

    /// Opens a comparison of the three columns of a small table, each one
    /// picked with Ctrl.
    async fn open_comparison_of_three_columns(
        cx: &mut TestAppContext,
    ) -> (
        Entity<Workspace>,
        Entity<TabularDataPreviewPane>,
        Entity<Buffer>,
        Entity<TableDiffView>,
        VisualTestContext,
    ) {
        let (workspace, pane, editor, mut cx) =
            open_grid_in_workspace("l,m,r\na,a,x\nb,B,y\nc,c,z", cx).await;
        let buffer = editor
            .read_with(&cx, |editor, cx| editor.buffer().read(cx).as_singleton())
            .expect("the fixture is a single file");
        pick_columns_with_ctrl(&pane, 0..3, 2, &mut cx);
        workspace.update_in(&mut cx, |workspace, window, cx| {
            TableDiffView::diff_selected_cells(workspace, pane.clone(), window, cx);
        });
        cx.run_until_parked();
        let view = workspace
            .read_with(&cx, |workspace, cx| {
                workspace.items_of_type::<TableDiffView>(cx).next()
            })
            .expect("the cell comparison should have opened as a tab");
        (workspace, pane, buffer, view, cx)
    }

    /// Three blocks picked with Ctrl are three things to compare. Only exactly
    /// two used to count, and a third sent the comparison off to the shape of
    /// the last block alone.
    #[gpui::test]
    async fn three_ctrl_picked_ranges_are_all_offered(cx: &mut TestAppContext) {
        let (_workspace, pane, _editor, mut cx) =
            open_grid_in_workspace("l,m,r\na,a,x\nb,B,y\nc,c,z", cx).await;
        pick_columns_with_ctrl(&pane, 0..3, 2, &mut cx);

        // Something on the clipboard is what a single cell would be held
        // against, which blocks picked with Ctrl leave out of it.
        let comparison = pane
            .read_with(&cx, |pane, _| cell_comparison(pane, "clipboard"))
            .expect("three ranges should be comparable");

        assert_eq!(pane_titles(&comparison), ["A2:A4", "B2:B4", "C2:C4"]);
        assert_eq!(comparison.pane_title(PaneSide::Left), "A2:A4");
        assert_eq!(comparison.pane_title(PaneSide::Right), "B2:B4");
        assert_eq!(comparison.title(), "セルの比較 (A2:A4 ↔ B2:B4)");
        assert_eq!(
            cell_diff(&comparison).change_count(),
            1,
            "the first two blocks differ only in the middle row"
        );
    }

    /// A drag down one column offers every cell in it, the ones in between as
    /// much as the two at the ends.
    #[gpui::test]
    async fn a_column_of_three_cells_offers_every_cell(cx: &mut TestAppContext) {
        let (_workspace, pane, _editor, mut cx) =
            open_grid_in_workspace("name\nalpha\nbeta\ngamma", cx).await;
        cx.simulate_keystrokes("down");
        cx.simulate_keystrokes("shift-down shift-down");
        cx.run_until_parked();

        let comparison = pane
            .read_with(&cx, |pane, _| cell_comparison(pane, ""))
            .expect("three cells of one column should be comparable");

        assert_eq!(pane_titles(&comparison), ["行 2", "行 3", "行 4"]);
        let values: Vec<Vec<Vec<&str>>> = comparison
            .candidates
            .iter()
            .map(|candidate| candidate.rows.iter().map(|row| cells(row)).collect())
            .collect();
        assert_eq!(values, [[["alpha"]], [["beta"]], [["gamma"]]]);
        assert_eq!(comparison.title(), "セルの比較 (A2 ↔ A3)");
    }

    /// A block three columns wide is three columns to compare, where it used to
    /// be turned away for not being exactly two.
    #[gpui::test]
    async fn a_range_three_columns_wide_offers_every_column(cx: &mut TestAppContext) {
        use crate::grid::{GridCell, SelectionChange};
        use crate::types::DisplayRow;

        let (_workspace, pane, _editor, mut cx) =
            open_grid_in_workspace("a,b,c\n1,1,2\n3,4,3", cx).await;
        pane.update(&mut cx, |pane, cx| {
            let cell = |row: usize, col: usize| {
                GridCell::new(GridRow::Body(DisplayRow(row)), AnyColumn(col))
            };
            pane.change_cell_selection(cell(0, 0), SelectionChange::Replace, cx);
            pane.change_cell_selection(cell(1, 2), SelectionChange::Extend, cx);
        });

        let comparison = pane
            .read_with(&cx, |pane, _| cell_comparison(pane, ""))
            .expect("three columns should be comparable");

        assert_eq!(pane_titles(&comparison), ["列 A", "列 B", "列 C"]);
        let columns: Vec<Vec<Vec<&str>>> = comparison
            .candidates
            .iter()
            .map(|candidate| candidate.rows.iter().map(|row| cells(row)).collect())
            .collect();
        assert_eq!(columns, [[["1"], ["3"]], [["1"], ["4"]], [["2"], ["3"]]]);
        assert_eq!(comparison.title(), "セルの比較 (A ↔ B)");
    }

    /// Picking the third block on the right compares it instead, and the tab
    /// follows: a tab still naming the block that was swapped out would send
    /// the user looking at the wrong cells.
    #[gpui::test]
    async fn picking_another_candidate_changes_the_comparison_and_the_tab(cx: &mut TestAppContext) {
        use std::{cell::RefCell, rc::Rc};

        let (_workspace, _pane, _buffer, view, mut cx) = open_comparison_of_three_columns(cx).await;
        let events = Rc::new(RefCell::new(Vec::new()));
        let _subscription = cx.update(|_window, cx| {
            let events = events.clone();
            cx.subscribe(&view, move |_view, event: &ItemEvent, _cx| {
                events.borrow_mut().push(*event);
            })
        });
        view.read_with(&cx, |view, cx| {
            assert_eq!(view.tab_content_text(0, cx), "セルの比較 (A2:A4 ↔ B2:B4)");
            assert_eq!(cells(view.right.row(0)), ["a"]);
        });

        view.update(&mut cx, |view, cx| {
            view.pick_cell_candidate(PaneSide::Right, 2, cx)
        });
        cx.run_until_parked();

        view.read_with(&cx, |view, cx| {
            assert_eq!(view.tab_content_text(0, cx), "セルの比較 (A2:A4 ↔ C2:C4)");
            assert_eq!(
                view.tab_tooltip_text(cx).as_deref(),
                Some("A2:A4 と C2:C4 の差分")
            );
            assert_eq!(cells(view.left.row(0)), ["a"]);
            assert_eq!(cells(view.right.row(0)), ["x"]);
        });
        assert_eq!(
            *events.borrow(),
            [ItemEvent::UpdateTab],
            "the workspace redraws a tab's title only when it is told the title changed"
        );

        // The block already on the other side swaps places rather than being
        // compared against itself.
        view.update(&mut cx, |view, cx| {
            view.pick_cell_candidate(PaneSide::Left, 2, cx)
        });
        cx.run_until_parked();

        view.read_with(&cx, |view, cx| {
            assert_eq!(view.tab_content_text(0, cx), "セルの比較 (C2:C4 ↔ A2:A4)");
            let left_title = view.left_pane_title();
            assert!(left_title.starts_with("C2:C4 ("), "{left_title}");
            assert_eq!(cells(view.left.row(0)), ["x"]);
            assert_eq!(cells(view.right.row(0)), ["a"]);
        });
    }

    /// "再比較" reads every candidate again, so one picked afterwards shows the
    /// grid as it is now rather than as it was when the tab was opened.
    #[gpui::test]
    async fn taking_the_comparison_again_reads_every_candidate(cx: &mut TestAppContext) {
        use crate::types::DisplayRow;

        let (_workspace, pane, buffer, view, mut cx) = open_comparison_of_three_columns(cx).await;
        buffer.update(&mut cx, |buffer, cx| {
            buffer.set_text("l,m,r\na,a,x\nb,B,changed\nc,c,z", cx);
        });
        cx.executor()
            .advance_clock(crate::parser::REPARSE_DEBOUNCE * 2);
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.cell_value(GridRow::Body(DisplayRow(1)), AnyColumn(2)),
                "changed",
                "the grid itself has to be showing the new value for this to be a test"
            );
        });

        // Picked before the comparison is taken again, the third block is what
        // was read when the tab was opened.
        view.update(&mut cx, |view, cx| {
            view.pick_cell_candidate(PaneSide::Right, 2, cx)
        });
        cx.run_until_parked();
        view.read_with(&cx, |view, _| {
            assert_eq!(cells(view.right.row(1)), ["y"]);
        });

        // Back to the second, so that the third is off screen while the
        // comparison is taken again.
        view.update(&mut cx, |view, cx| {
            view.pick_cell_candidate(PaneSide::Right, 1, cx);
            view.recompare(cx);
        });
        cx.run_until_parked();
        view.update(&mut cx, |view, cx| {
            view.pick_cell_candidate(PaneSide::Right, 2, cx)
        });
        cx.run_until_parked();

        view.read_with(&cx, |view, _| {
            assert_eq!(cells(view.right.row(1)), ["changed"]);
        });
    }

    /// The pick works from what the comparison holds, so it goes on working
    /// once the grid's tab is closed and nothing can be read again.
    #[gpui::test]
    async fn picking_works_without_the_grid(cx: &mut TestAppContext) {
        let (_workspace, _pane, _editor, mut cx) = open_grid_in_workspace("a,b", cx).await;
        let view = cx.update(|_window, cx| {
            cx.new(|cx| {
                TableDiffView::from_cells(
                    WeakEntity::new_invalid(),
                    clipboard_candidates(&["a", "b", "c"]),
                    cx,
                )
            })
        });
        cx.run_until_parked();

        view.update(&mut cx, |view, cx| {
            view.pick_cell_candidate(PaneSide::Right, 2, cx)
        });
        cx.run_until_parked();

        view.read_with(&cx, |view, cx| {
            assert!(!view.can_recompare());
            assert_eq!(cells(view.right.row(0)), ["c"]);
            assert_eq!(view.tab_content_text(0, cx), "セルの比較 (a ↔ c)");
        });
    }

    #[test]
    fn picking_the_other_sides_candidate_swaps_the_two() {
        let mut comparison = clipboard_candidates(&["a", "b", "c"]);

        assert!(comparison.pick(PaneSide::Right, 2));
        assert_eq!((comparison.left, comparison.right), (0, 2));
        assert!(comparison.pick(PaneSide::Left, 2));
        assert_eq!((comparison.left, comparison.right), (2, 0));
        assert!(
            !comparison.pick(PaneSide::Left, 2),
            "picking what is already there changes nothing"
        );
        assert!(
            !comparison.pick(PaneSide::Right, 3),
            "and neither does picking past the end"
        );
        assert_eq!((comparison.left, comparison.right), (2, 0));
    }

    /// The pane titles are lists to pick from only when there is something else
    /// to pick: with two things to compare a list would offer nothing more than
    /// swapping them.
    #[gpui::test]
    async fn the_pane_titles_are_pick_lists_only_with_more_than_two_candidates(
        cx: &mut TestAppContext,
    ) {
        let (workspace, pane, _editor, mut cx) =
            open_grid_in_workspace("l,m,r\na,a,x\nb,B,y\nc,c,z", cx).await;

        pick_columns_with_ctrl(&pane, 0..2, 2, &mut cx);
        workspace.update_in(&mut cx, |workspace, window, cx| {
            TableDiffView::diff_selected_cells(workspace, pane.clone(), window, cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds(DIFF_LEFT_PANE_DEBUG_SELECTOR).is_some(),
            "the comparison has to be on screen for this to be a test"
        );
        assert_eq!(cx.debug_bounds(DIFF_LEFT_PICK_DEBUG_SELECTOR), None);
        assert_eq!(cx.debug_bounds(DIFF_RIGHT_PICK_DEBUG_SELECTOR), None);

        pick_columns_with_ctrl(&pane, 0..3, 2, &mut cx);
        workspace.update_in(&mut cx, |workspace, window, cx| {
            TableDiffView::diff_selected_cells(workspace, pane.clone(), window, cx);
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds(DIFF_LEFT_PICK_DEBUG_SELECTOR).is_some());
        assert!(cx.debug_bounds(DIFF_RIGHT_PICK_DEBUG_SELECTOR).is_some());
    }

    /// A selection with nothing in it to compare, or with too much, says which
    /// of the two on the grid's status line rather than opening a tab.
    #[gpui::test]
    async fn a_selection_that_cannot_be_compared_says_why(cx: &mut TestAppContext) {
        use crate::grid::{GridCell, SelectionChange};
        use crate::types::DisplayRow;

        let mut table = "value".to_owned();
        for row in 0..MAX_CELL_CANDIDATES + 20 {
            table.push_str(&format!("\n{row}"));
        }
        let (workspace, pane, _editor, mut cx) = open_grid_in_workspace(&table, cx).await;
        let select_down_to = |last_body_row: usize, cx: &mut VisualTestContext| {
            pane.update(cx, |pane, cx| {
                let cell = |row: usize| GridCell::new(GridRow::Body(DisplayRow(row)), AnyColumn(0));
                pane.change_cell_selection(cell(0), SelectionChange::Replace, cx);
                pane.change_cell_selection(cell(last_body_row), SelectionChange::Extend, cx);
            });
        };
        let open_comparisons = |cx: &mut VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                TableDiffView::diff_selected_cells(workspace, pane.clone(), window, cx);
            });
            cx.run_until_parked();
            workspace.read_with(cx, |workspace, cx| {
                workspace.items_of_type::<TableDiffView>(cx).count()
            })
        };
        let status = |cx: &mut VisualTestContext| {
            pane.read_with(cx, |pane, _| {
                pane.grid.status_message.clone().unwrap_or_default()
            })
        };

        select_down_to(MAX_CELL_CANDIDATES - 1, &mut cx);
        let comparison = pane
            .read_with(&cx, |pane, _| cell_comparison(pane, ""))
            .expect("as many cells as the lists offer should be comparable");
        assert_eq!(comparison.candidates.len(), MAX_CELL_CANDIDATES);

        select_down_to(MAX_CELL_CANDIDATES, &mut cx);
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                cell_comparison(pane, "").err(),
                Some(CellComparisonRefusal::TooManyCandidates(
                    MAX_CELL_CANDIDATES + 1
                ))
            );
        });
        assert_eq!(open_comparisons(&mut cx), 0);
        let message = status(&mut cx);
        assert!(
            message.contains(&(MAX_CELL_CANDIDATES + 1).to_string()),
            "{message}"
        );

        // The test platform's clipboard starts out empty, which leaves one cell
        // with nothing to be held against.
        select_down_to(0, &mut cx);
        assert_eq!(open_comparisons(&mut cx), 0);
        let message = status(&mut cx);
        assert!(message.contains("クリップボードが空"), "{message}");
    }

    #[test]
    fn more_than_two_candidates_are_bounded_by_the_cells_they_read() {
        let over = MAX_CELL_CANDIDATE_CELLS + 1;

        assert_eq!(
            CellComparisonRefusal::check(3, MAX_CELL_CANDIDATE_CELLS),
            Ok(())
        );
        assert_eq!(
            CellComparisonRefusal::check(3, over),
            Err(CellComparisonRefusal::TooManyCells(over))
        );
        assert_eq!(
            CellComparisonRefusal::check(2, over),
            Ok(()),
            "two candidates are the two sides, which a comparison of cells has always read"
        );
        assert_eq!(
            CellComparisonRefusal::check(MAX_CELL_CANDIDATES + 1, 0),
            Err(CellComparisonRefusal::TooManyCandidates(
                MAX_CELL_CANDIDATES + 1
            ))
        );
    }

    /// Ctrl+A over a table as wide as the pick lists allow would read every
    /// cell of it ahead of anything being picked, so it is turned away by the
    /// cells it covers rather than let through by its columns.
    #[gpui::test]
    async fn selecting_all_of_a_big_wide_table_is_turned_away(cx: &mut TestAppContext) {
        let columns = MAX_CELL_CANDIDATES;
        let lines = MAX_CELL_CANDIDATE_CELLS / columns + 1;
        let line = vec!["0"; columns].join(",");
        let table = vec![line.as_str(); lines].join("\n");
        let (workspace, pane, _editor, mut cx) = open_grid_in_workspace(&table, cx).await;

        pane.update(&mut cx, |pane, cx| pane.select_all_cells(cx));
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                cell_comparison(pane, "").err(),
                Some(CellComparisonRefusal::TooManyCells(columns * lines))
            );
        });
        workspace.update_in(&mut cx, |workspace, window, cx| {
            TableDiffView::diff_selected_cells(workspace, pane.clone(), window, cx);
        });
        cx.run_until_parked();

        workspace.read_with(&cx, |workspace, cx| {
            assert_eq!(workspace.items_of_type::<TableDiffView>(cx).count(), 0);
        });
        let message = pane.read_with(&cx, |pane, _| {
            pane.grid.status_message.clone().unwrap_or_default()
        });
        assert!(
            message.contains(&crate::grid::grouped_digits(columns * lines)),
            "{message}"
        );

        // Every column picked with Ctrl by its letter is as much to read, and
        // every block counts, not only the one the caret is in.
        pane.update(&mut cx, |pane, cx| {
            use crate::grid::SelectionChange;
            pane.change_column_selection(AnyColumn(0), SelectionChange::Replace, cx);
            for column in 1..columns {
                pane.change_column_selection(AnyColumn(column), SelectionChange::Add, cx);
            }
        });
        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.selection_rects().len(), columns);
            assert_eq!(
                cell_comparison(pane, "").err(),
                Some(CellComparisonRefusal::TooManyCells(columns * lines))
            );
        });
    }

    /// A whole column picked by its letter offers every row of it, named the
    /// way the toolbar's address names that row whether or not the first line
    /// is read as a header.
    #[gpui::test]
    async fn a_whole_column_names_its_rows_the_way_the_toolbar_does(cx: &mut TestAppContext) {
        use crate::grid::{GridCell, SelectionChange};

        let (_workspace, pane, _editor, mut cx) =
            open_grid_in_workspace("name\nalpha\nbeta", cx).await;
        let check = |cx: &mut VisualTestContext| {
            pane.update(cx, |pane, cx| pane.select_column(AnyColumn(0), false, cx));
            let (comparison, rows) = pane.read_with(cx, |pane, _| {
                (
                    cell_comparison(pane, "")
                        .expect("a column of three cells should be comparable"),
                    pane.selection_rect().rows(),
                )
            });
            assert_eq!(pane_titles(&comparison), ["行 1", "行 2", "行 3"]);
            let values: Vec<Vec<Vec<&str>>> = comparison
                .candidates
                .iter()
                .map(|candidate| candidate.rows.iter().map(|row| cells(row)).collect())
                .collect();
            assert_eq!(values, [[["name"]], [["alpha"]], [["beta"]]]);

            let toolbar_addresses: Vec<String> = rows
                .map(|row| {
                    pane.update(cx, |pane, cx| {
                        pane.change_cell_selection(
                            GridCell::new(GridRow::from_index(row), AnyColumn(0)),
                            SelectionChange::Replace,
                            cx,
                        );
                        pane.cursor_address()
                    })
                })
                .collect();
            let tab_titles: Vec<&str> = comparison
                .candidates
                .iter()
                .map(|candidate| candidate.tab_title.as_ref())
                .collect();
            assert_eq!(tab_titles, toolbar_addresses);
        };

        check(&mut cx);

        pane.update(&mut cx, |pane, cx| pane.toggle_header_row(cx));
        cx.run_until_parked();
        assert!(
            !pane.read_with(&cx, |pane, _| pane.has_header_row()),
            "the first line has to be a data row for this half to be a test"
        );
        check(&mut cx);
    }

    /// Whole columns and whole rows picked with Ctrl among other blocks are
    /// named by the cells they cover, the header's line included.
    #[gpui::test]
    async fn ctrl_picked_whole_columns_and_rows_are_named_by_what_they_cover(
        cx: &mut TestAppContext,
    ) {
        use crate::grid::{GridCell, SelectionChange};
        use crate::types::DisplayRow;

        let (_workspace, pane, _editor, mut cx) =
            open_grid_in_workspace("l,m,r\na,a,x\nb,B,y\nc,c,z", cx).await;
        pane.update(&mut cx, |pane, cx| {
            let cell = |row: usize, col: usize| {
                GridCell::new(GridRow::Body(DisplayRow(row)), AnyColumn(col))
            };
            pane.change_cell_selection(cell(0, 0), SelectionChange::Replace, cx);
            pane.change_cell_selection(cell(1, 0), SelectionChange::Extend, cx);
            pane.change_column_selection(AnyColumn(2), SelectionChange::Add, cx);
            pane.change_row_selection(GridRow::Body(DisplayRow(2)), SelectionChange::Add, cx);
        });

        let comparison = pane
            .read_with(&cx, |pane, _| cell_comparison(pane, ""))
            .expect("three ranges should be comparable");

        assert_eq!(pane_titles(&comparison), ["A2:A3", "C1:C4", "A4:C4"]);
        let values: Vec<Vec<Vec<&str>>> = comparison
            .candidates
            .iter()
            .map(|candidate| candidate.rows.iter().map(|row| cells(row)).collect())
            .collect();
        assert_eq!(
            values,
            [
                vec![vec!["a"], vec!["b"]],
                vec![vec!["r"], vec!["x"], vec!["y"], vec!["z"]],
                vec![vec!["c", "c", "z"]],
            ]
        );
    }

    /// A cell picked with Ctrl on its own is named by its one address. Named as
    /// a block it read "B2:B2", the same cell twice over, in the pick lists and
    /// on the tab alike.
    #[gpui::test]
    async fn a_single_ctrl_picked_cell_is_named_by_its_one_address(cx: &mut TestAppContext) {
        use crate::grid::{GridCell, SelectionChange};
        use crate::types::DisplayRow;

        let (_workspace, pane, _editor, mut cx) =
            open_grid_in_workspace("l,m,r\na,a,x\nb,B,y\nc,c,z", cx).await;
        pane.update(&mut cx, |pane, cx| {
            let cell = |row: usize, col: usize| {
                GridCell::new(GridRow::Body(DisplayRow(row)), AnyColumn(col))
            };
            pane.change_cell_selection(cell(0, 1), SelectionChange::Replace, cx);
            pane.change_cell_selection(cell(1, 1), SelectionChange::Add, cx);
            pane.change_cell_selection(cell(2, 1), SelectionChange::Add, cx);
            pane.change_cell_selection(cell(0, 2), SelectionChange::Add, cx);
            pane.change_cell_selection(cell(2, 2), SelectionChange::Extend, cx);
        });

        let comparison = pane
            .read_with(&cx, |pane, _| cell_comparison(pane, ""))
            .expect("four ranges should be comparable");

        assert_eq!(pane_titles(&comparison), ["B2", "B3", "B4", "C2:C4"]);
        assert_eq!(comparison.title(), "セルの比較 (B2 ↔ B3)");
    }

    /// Selects every row from `first` to `last` by the row numbers.
    fn select_whole_rows(
        pane: &Entity<TabularDataPreviewPane>,
        first: GridRow,
        last: GridRow,
        cx: &mut VisualTestContext,
    ) {
        pane.update(cx, |pane, cx| {
            pane.select_row(first, false, cx);
            pane.select_row(last, true, cx);
        });
    }

    /// Every candidate's values, one row of cells per candidate.
    fn candidate_rows(comparison: &CellComparison) -> Vec<Vec<&str>> {
        comparison
            .candidates
            .iter()
            .map(|candidate| candidate.rows.iter().flat_map(|row| cells(row)).collect())
            .collect()
    }

    /// Rows picked by their numbers are compared row against row. The shape of
    /// the block alone would have read them as its columns: a whole row is
    /// every column wide.
    #[gpui::test]
    async fn whole_rows_picked_by_their_numbers_are_compared_row_by_row(cx: &mut TestAppContext) {
        use crate::types::DisplayRow;

        let (workspace, pane, _editor, mut cx) =
            open_grid_in_workspace("l,m,r\na,b,x\na,B,x\nc,c,z", cx).await;

        select_whole_rows(
            &pane,
            GridRow::Body(DisplayRow(0)),
            GridRow::Body(DisplayRow(1)),
            &mut cx,
        );
        let comparison = pane
            .read_with(&cx, |pane, _| cell_comparison(pane, ""))
            .expect("two whole rows should be comparable");
        assert_eq!(pane_titles(&comparison), ["行 2", "行 3"]);
        assert_eq!(
            candidate_rows(&comparison),
            [["a", "b", "x"], ["a", "B", "x"]]
        );
        assert_eq!(comparison.title(), "セルの比較 (行 2 ↔ 行 3)");
        let diff = cell_diff(&comparison);
        assert_eq!(diff.modified_rows, 1);
        assert_eq!(
            diff.rows.first().map(|row| row.changed_columns.clone()),
            Some(vec![1])
        );

        select_whole_rows(
            &pane,
            GridRow::Body(DisplayRow(0)),
            GridRow::Body(DisplayRow(2)),
            &mut cx,
        );
        workspace.update_in(&mut cx, |workspace, window, cx| {
            TableDiffView::diff_selected_cells(workspace, pane.clone(), window, cx);
        });
        cx.run_until_parked();
        let view = workspace
            .read_with(&cx, |workspace, cx| {
                workspace.items_of_type::<TableDiffView>(cx).next()
            })
            .expect("the comparison of three rows should have opened as a tab");
        view.read_with(&cx, |view, cx| {
            let DiffSource::Cells { comparison, .. } = &view.source else {
                panic!("a comparison of rows is a comparison of cells");
            };
            assert_eq!(pane_titles(comparison), ["行 2", "行 3", "行 4"]);
            assert_eq!(view.tab_content_text(0, cx), "セルの比較 (行 2 ↔ 行 3)");
            assert_eq!(cells(view.left.row(0)), ["a", "b", "x"]);
            assert_eq!(cells(view.right.row(0)), ["a", "B", "x"]);
        });
        assert!(cx.debug_bounds(DIFF_RIGHT_PICK_DEBUG_SELECTOR).is_some());
    }

    /// The rows are named the way the toolbar's address names them, with the
    /// header's line among them or not.
    #[gpui::test]
    async fn whole_rows_are_named_the_way_the_toolbar_does(cx: &mut TestAppContext) {
        use crate::grid::{GridCell, SelectionChange};
        use crate::types::DisplayRow;

        let (_workspace, pane, _editor, mut cx) =
            open_grid_in_workspace("l,m,r\na,a,x\nb,B,y", cx).await;
        let names_and_toolbar_names =
            |first: GridRow, last: GridRow, cx: &mut VisualTestContext| {
                select_whole_rows(&pane, first, last, cx);
                let (comparison, rows) = pane.read_with(cx, |pane, _| {
                    (
                        cell_comparison(pane, "").expect("two whole rows should be comparable"),
                        pane.selection_rect().rows(),
                    )
                });
                let toolbar_names: Vec<String> = rows
                    .map(|row| {
                        let address = pane.update(cx, |pane, cx| {
                            pane.change_cell_selection(
                                GridCell::new(GridRow::from_index(row), AnyColumn(0)),
                                SelectionChange::Replace,
                                cx,
                            );
                            pane.cursor_address()
                        });
                        format!(
                            "行 {}",
                            address.trim_start_matches(
                                |character: char| character.is_ascii_alphabetic()
                            )
                        )
                    })
                    .collect();
                let names: Vec<String> = pane_titles(&comparison)
                    .into_iter()
                    .map(str::to_owned)
                    .collect();
                let values: Vec<Vec<String>> = candidate_rows(&comparison)
                    .into_iter()
                    .map(|row| row.into_iter().map(str::to_owned).collect())
                    .collect();
                (names, toolbar_names, values)
            };

        let (names, toolbar_names, values) =
            names_and_toolbar_names(GridRow::Header, GridRow::Body(DisplayRow(0)), &mut cx);
        assert_eq!(names, ["行 1", "行 2"]);
        assert_eq!(names, toolbar_names);
        assert_eq!(values, [["l", "m", "r"], ["a", "a", "x"]]);

        pane.update(&mut cx, |pane, cx| pane.toggle_header_row(cx));
        cx.run_until_parked();
        assert!(
            !pane.read_with(&cx, |pane, _| pane.has_header_row()),
            "the first line has to be a data row for this half to be a test"
        );
        let (names, toolbar_names, values) = names_and_toolbar_names(
            GridRow::Body(DisplayRow(0)),
            GridRow::Body(DisplayRow(1)),
            &mut cx,
        );
        assert_eq!(names, ["行 1", "行 2"]);
        assert_eq!(names, toolbar_names);
        assert_eq!(values, [["l", "m", "r"], ["a", "a", "x"]]);
    }

    /// One whole row has nothing of its own to be compared with, so it is cut
    /// into its columns as any other block one row tall is.
    #[gpui::test]
    async fn a_single_whole_row_is_compared_column_by_column(cx: &mut TestAppContext) {
        use crate::types::DisplayRow;

        let (_workspace, pane, _editor, mut cx) =
            open_grid_in_workspace("l,m,r\na,a,x\nb,B,y", cx).await;
        pane.update(&mut cx, |pane, cx| {
            pane.select_row(GridRow::Body(DisplayRow(1)), false, cx)
        });

        let comparison = pane
            .read_with(&cx, |pane, _| cell_comparison(pane, ""))
            .expect("a whole row of three cells should be comparable");
        assert_eq!(pane_titles(&comparison), ["列 A", "列 B", "列 C"]);
        assert_eq!(candidate_rows(&comparison), [["b"], ["B"], ["y"]]);
    }

    /// Whole rows are as many candidates as there are rows, and the pick lists
    /// hold no more of them than of anything else.
    #[gpui::test]
    async fn whole_rows_past_the_limit_are_turned_away(cx: &mut TestAppContext) {
        use crate::types::DisplayRow;

        let mut table = "a,b".to_owned();
        for row in 0..MAX_CELL_CANDIDATES + 10 {
            table.push_str(&format!("\n{row},{row}"));
        }
        let (_workspace, pane, _editor, mut cx) = open_grid_in_workspace(&table, cx).await;

        select_whole_rows(
            &pane,
            GridRow::Body(DisplayRow(0)),
            GridRow::Body(DisplayRow(MAX_CELL_CANDIDATES - 1)),
            &mut cx,
        );
        let comparison = pane
            .read_with(&cx, |pane, _| cell_comparison(pane, ""))
            .expect("as many rows as the lists offer should be comparable");
        assert_eq!(comparison.candidates.len(), MAX_CELL_CANDIDATES);

        select_whole_rows(
            &pane,
            GridRow::Body(DisplayRow(0)),
            GridRow::Body(DisplayRow(MAX_CELL_CANDIDATES)),
            &mut cx,
        );
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                cell_comparison(pane, "").err(),
                Some(CellComparisonRefusal::TooManyCandidates(
                    MAX_CELL_CANDIDATES + 1
                ))
            );
        });
    }

    /// Clicks the right pane's pick list open. The marked strip is the whole
    /// title, and the button sits at its start.
    fn open_the_right_pick_list(cx: &mut VisualTestContext) {
        let title = cx
            .debug_bounds(DIFF_RIGHT_PICK_DEBUG_SELECTOR)
            .expect("with three candidates the right pane's title is a pick list");
        cx.simulate_click(
            gpui::point(title.origin.x + px(16.), title.center().y),
            gpui::Modifiers::none(),
        );
        cx.run_until_parked();
        // A popover menu takes the focus two frames after it opens, once it is
        // in the dispatch tree, and a test has no frame loop to get there.
        for _ in 0..2 {
            cx.update(|window, cx| window.simulate_next_frame(cx));
            cx.run_until_parked();
        }
    }

    /// The pick list is wired to the comparison, not just drawn: choosing an
    /// entry in it puts that candidate on its side.
    #[gpui::test]
    async fn choosing_from_the_pick_list_puts_that_candidate_on_its_side(cx: &mut TestAppContext) {
        let (_workspace, _pane, _buffer, view, mut cx) = open_comparison_of_three_columns(cx).await;

        // The list opens on the entry its side already shows, the second.
        open_the_right_pick_list(&mut cx);
        cx.simulate_keystrokes("down enter");
        cx.run_until_parked();

        view.read_with(&cx, |view, cx| {
            assert_eq!(view.tab_content_text(0, cx), "セルの比較 (A2:A4 ↔ C2:C4)");
            assert_eq!(cells(view.right.row(0)), ["x"]);
        });
    }

    /// A pick list left open as the tab closes must not keep the comparison
    /// alive, which a menu holding the view it picks for would.
    #[gpui::test]
    async fn closing_a_comparison_with_its_pick_list_open_releases_it(cx: &mut TestAppContext) {
        let (workspace, _pane, _buffer, view, mut cx) = open_comparison_of_three_columns(cx).await;

        open_the_right_pick_list(&mut cx);
        cx.update(|window, cx| {
            let focus_handle = view.read(cx).focus_handle.clone();
            assert!(
                !focus_handle.is_focused(window) && focus_handle.contains_focused(window, cx),
                "the pick list has to be open, holding the focus inside the comparison"
            );
        });

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
        let view = cx.update(|_window, cx| {
            cx.new(|cx| {
                TableDiffView::from_cells(
                    WeakEntity::new_invalid(),
                    clipboard_candidates(&["a", "b"]),
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
        let view = cx.update(|_window, cx| {
            cx.new(|cx| {
                TableDiffView::from_cells(
                    WeakEntity::new_invalid(),
                    clipboard_candidates(&["a", "b"]),
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
            row_number_digits(&long, &long) > row_number_digits(&short, &short),
            "a comparison numbering a row 202 needs more room than one numbering it 2"
        );
    }
}
