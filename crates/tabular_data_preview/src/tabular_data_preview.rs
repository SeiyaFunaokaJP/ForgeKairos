use editor::{Editor, EditorEvent};
use gpui::{
    Action, AppContext, Entity, EventEmitter, FocusHandle, Focusable, ListAlignment, Subscription,
    Task, actions,
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use crate::table_data_engine::{DisplayToDataMapping, TableDataEngine};
use ui::{
    AbsoluteLength, ResizableColumnsState, SharedString, TableInteractionState,
    TableResizeBehavior, prelude::*,
};
use workspace::{Pane, Workspace, searchable::SearchEvent};

use crate::{
    parser::EditorState,
    renderer::render_table::{APPEND_COLUMN_WIDTH, APPEND_ROWS},
    settings::TabularDataPreviewSettings,
    tabular_data_settings::TabularDataSettings,
    types::TableLikeContent,
};
use ::settings::Settings as _;

mod edit_engine;
mod grid;
mod grid_editing;
mod grid_fill;
mod grid_search;
mod grid_styles;
mod parser;
mod renderer;
mod settings;
mod table_data_engine;
mod table_diff;
pub mod tabular_data_settings;
mod types;
mod workspace_item;

pub use crate::grid_fill::{FillDown, FillRight};
pub use crate::grid_search::GridMatch;
pub use crate::grid_styles::{CellColor, ClearCellColors, SetFillColor, SetTextColor};
pub use crate::workspace_item::TabularBuffer;

/// 同じファイルをテキストとして開きます。既定では隣のペインに開き、
/// `in_same_pane` を指定するとグリッドのタブをテキストのタブに差し替えます。
#[derive(PartialEq, Clone, Default, Deserialize, JsonSchema, Action)]
#[action(namespace = tabular_data)]
#[serde(deny_unknown_fields)]
pub struct OpenAsText {
    /// グリッドのタブをそのままテキストのタブに差し替えます。
    #[serde(default)]
    pub in_same_pane: bool,
}

actions!(
    tabular_data,
    [
        OpenPreview,
        OpenPreviewToTheSide,
        StartEditing,
        CommitEditing,
        CommitAndMoveDown,
        CommitAndMoveUp,
        CommitAndMoveRight,
        CommitAndMoveLeft,
        CancelEditing,
        InsertLineBreak,
        InsertRowAbove,
        InsertRowBelow,
        DeleteRows,
        InsertColumnLeft,
        InsertColumnRight,
        DeleteColumns,
        InsertRowsOrColumns,
        DeleteRowsOrColumns,
        ClearCells,
        SelectRow,
        SelectColumn,
        SelectAllCells,
        CopyCells,
        CutCells,
        PasteCells,
        ToggleHeaderRow,
    ]
);

pub struct TabularDataPreviewPane {
    pub(crate) engine: TableDataEngine,

    pub(crate) grid: grid::GridState,
    /// The fill and text colors painted onto this file, addressed by physical
    /// row and column. Kept out of the file itself, which has nowhere for them,
    /// and shared with every other grid open on the same file.
    pub(crate) styles: Entity<grid_styles::GridStylesModel>,
    pub(crate) focus_handle: FocusHandle,
    active_editor_state: EditorState,
    pub(crate) table_interaction_state: Entity<TableInteractionState>,
    pub(crate) column_widths: ColumnWidths,
    pub(crate) parsing_task: Option<Task<anyhow::Result<()>>>,
    pub(crate) is_parsing: bool,
    /// Background task computing the display-to-data mapping after a filter/sort change.
    /// Stored here so that a new change cancels the previous in-flight computation.
    pub(crate) filter_sort_task: Option<Task<()>>,
    pub(crate) settings: TabularDataPreviewSettings,
    /// Performance metrics for debugging and monitoring tabular data operations.
    pub(crate) performance_metrics: PerformanceMetrics,
    pub(crate) list_state: gpui::ListState,
    /// Cached row height, refreshed from the actual text line height on every render.
    /// Used to size not-yet-rendered rows for the scrollbar without a full `.measure_all()`
    /// pass, so it tracks the real row height instead of a hardcoded guess.
    pub(crate) row_height: Pixels,
    /// Time when the last parsing operation ended, used for smart debouncing
    pub(crate) last_parse_end_time: Option<std::time::Instant>,
    /// Set by the pane; leaving the tab pushes an entry so that "go back" and "reopen
    /// closed tab" reach the grid the same way they reach an editor.
    nav_history: Option<workspace::ItemNavHistory>,
    /// Opens the caret's column filter popover from the keyboard. Only that one
    /// column's heading is rendered with the handle, so it always deploys the
    /// menu of the column the caret is standing in.
    pub(crate) filter_menu_handle:
        ui::PopoverMenuHandle<picker::Picker<renderer::table_header::ColumnFilterDelegate>>,
    /// Redraws the grid when another grid on the same file repaints a cell.
    _styles_subscription: Subscription,
}

pub fn init(cx: &mut App) {
    // Registered after `editor::init`, so that the registry - which tries the most
    // recently registered project item first - offers tabular files to the grid before
    // the text editor.
    workspace::register_project_item::<TabularDataPreviewPane>(cx);
    workspace::register_serializable_item::<TabularDataPreviewPane>(cx);
    grid_styles::hide_color_actions_from_the_command_palette(cx);

    cx.observe_new(|workspace: &mut Workspace, _, _| {
        TabularDataPreviewPane::register(workspace);
    })
    .detach()
}

impl TabularDataPreviewPane {
    pub(crate) fn sync_column_widths(&self, cx: &mut Context<Self>) {
        // The row identifier column before the data columns, and the "+" column
        // after them.
        let cols = self.table_column_count();
        let line_number_width = self.calculate_row_identifier_column_width();

        let mut widths: Vec<AbsoluteLength> = self.calculate_data_column_widths();
        widths.insert(0, AbsoluteLength::Pixels(px(line_number_width)));
        widths.push(AbsoluteLength::Pixels(px(APPEND_COLUMN_WIDTH)));

        let mut resize_behaviors = vec![TableResizeBehavior::Resizable; cols];
        resize_behaviors[0] = TableResizeBehavior::None;
        // The "+" column holds no value that could want more room, and dragging
        // it would only take room away from the last data column.
        if let Some(last) = resize_behaviors.last_mut() {
            *last = TableResizeBehavior::None;
        }

        self.column_widths.widths.update(cx, |state, _cx| {
            if state.cols() != cols {
                *state = ResizableColumnsState::new(cols, widths, resize_behaviors);
            } else {
                state.set_column_configuration(
                    0,
                    AbsoluteLength::Pixels(px(line_number_width)),
                    TableResizeBehavior::None,
                );
            }
        });
    }

    pub fn register(workspace: &mut Workspace) {
        Self::register_diff_actions(workspace);
        workspace.register_action_renderer(|div, _, _, cx| {
            div.on_action(cx.listener(|workspace, _: &OpenPreview, window, cx| {
                if let Some(editor) =
                    Self::resolve_active_item_as_tabular_data_editor(workspace, cx)
                {
                    let pane = workspace.active_pane().clone();
                    Self::open_preview_in_pane(editor, pane, window, cx);
                }
            }))
            .on_action(
                cx.listener(|workspace, _: &OpenPreviewToTheSide, window, cx| {
                    if let Some(editor) =
                        Self::resolve_active_item_as_tabular_data_editor(workspace, cx)
                    {
                        let pane = workspace.active_pane().clone();
                        Self::open_preview_to_the_side_of_pane(workspace, editor, pane, window, cx);
                    }
                }),
            )
            .on_action(cx.listener(|workspace, action: &OpenAsText, window, cx| {
                let Some(grid) = workspace
                    .active_item(cx)
                    .and_then(|item| item.downcast::<TabularDataPreviewPane>())
                else {
                    return;
                };
                let pane = workspace.active_pane().clone();
                Self::open_as_text(workspace, grid, pane, action.in_same_pane, window, cx);
            }))
        });
    }

    /// Opens the grid's buffer in a text editor, by default beside the grid so that both
    /// views of the same file stay visible. In the grid's own pane the two cannot coexist,
    /// so there the grid tab is swapped for the text tab.
    pub fn open_as_text(
        workspace: &mut Workspace,
        grid: Entity<TabularDataPreviewPane>,
        origin_pane: Entity<Pane>,
        open_in_same_pane: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(buffer) = grid.read(cx).buffer(cx) else {
            return;
        };
        let entry_id = grid.read(cx).entry_id(cx);
        let project = workspace.project().clone();
        let target_pane = if open_in_same_pane {
            origin_pane
        } else {
            if workspace_item::activate_existing_item::<Editor>(workspace, entry_id, window, cx) {
                return;
            }
            workspace.adjacent_pane_of(&origin_pane, window, cx)
        };

        workspace_item::activate_or_replace_in_pane::<Editor>(
            target_pane,
            entry_id,
            true,
            move |window, cx| cx.new(|cx| Editor::for_buffer(buffer, Some(project), window, cx)),
            window,
            cx,
        );
        cx.notify();
    }

    pub fn open_preview_in_pane(
        editor: Entity<Editor>,
        pane: Entity<Pane>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        Self::activate_or_add_preview(editor, pane, true, window, cx);
    }

    pub fn open_preview_to_the_side_of_pane(
        workspace: &mut Workspace,
        editor: Entity<Editor>,
        origin_pane: Entity<Pane>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let entry_id = Self::entry_id_for_editor(&editor, cx);
        if workspace_item::activate_existing_item::<Self>(workspace, entry_id, window, cx) {
            return;
        }
        let target_pane = workspace.adjacent_pane_of(&origin_pane, window, cx);
        Self::activate_or_add_preview(editor, target_pane, false, window, cx);
    }

    fn activate_or_add_preview(
        editor: Entity<Editor>,
        pane: Entity<Pane>,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        // Matching on the file rather than on the editor entity: a grid opened as the
        // file's own workspace item owns a different, never-rendered editor, so comparing
        // the handles would open a second grid for a file that already has one.
        let entry_id = Self::entry_id_for_editor(&editor, cx);
        workspace_item::activate_or_replace_in_pane::<TabularDataPreviewPane>(
            pane,
            entry_id,
            focus,
            move |window, cx| Self::new(&editor, window, cx),
            window,
            cx,
        );
        cx.notify();
    }

    fn entry_id_for_editor(editor: &Entity<Editor>, cx: &App) -> Option<project::ProjectEntryId> {
        let buffer = editor.read(cx).buffer().read(cx).as_singleton()?;
        project::ProjectItem::entry_id(buffer.read(cx), cx)
    }

    fn new(editor: &Entity<Editor>, window: &mut Window, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| Self::build(editor.clone(), window, cx))
    }

    /// Shared by the preview opened beside a text editor and by the grid opened as the
    /// file's own workspace item, so that both are wired to their editor identically.
    pub(crate) fn build(
        editor: Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let contents = TableLikeContent::default();
        let table_interaction_state = cx.new(|cx| {
            TableInteractionState::new(cx).with_custom_scrollbar(ui::Scrollbars::for_settings::<
                editor::EditorSettingsScrollbarProxy,
            >())
        });

        let subscription = cx.subscribe(
            &editor,
            |this: &mut TabularDataPreviewPane, _editor, event: &EditorEvent, cx| {
                match event {
                    // `Edited` only fires for transactions this editor itself performed, so
                    // edits made in the file's text tab arrive as `BufferEdited`.
                    EditorEvent::Edited { .. }
                    | EditorEvent::BufferEdited
                    | EditorEvent::DirtyChanged => {
                        this.parse_from_active_editor(true, cx);
                    }
                    // "Save as" gave the buffer a new path, and the colors and
                    // the filters are stored under the path rather than in the
                    // file.
                    EditorEvent::FileHandleChanged => {
                        this.on_file_path_changed(cx);
                        this.on_filters_path_changed(cx);
                    }
                    _ => {}
                };
                // Forwarded so that the tab reacts to the buffer exactly as an editor tab
                // does: unsaved dot, title, save prompts and serialization.
                cx.emit(event.clone());
            },
        );

        // Shared with any other grid already open on this file, so that the two
        // paint and store the same colors instead of overwriting each other's.
        let styles = grid_styles::GridStylesModel::for_editor(&editor, cx);
        let styles_subscription = cx.observe(&styles, |_this, _styles, cx| cx.notify());

        // The same height the first render will settle on, so that the list is
        // not seeded with hints from the window's UI text and then re-measured.
        let row_height = grid::cell_line_height(cx);
        let mut view = TabularDataPreviewPane {
            grid: grid::GridState::new(window, cx),
            styles,
            focus_handle: cx.focus_handle(),
            active_editor_state: EditorState {
                editor,
                _subscription: subscription,
            },
            table_interaction_state,
            column_widths: ColumnWidths::new(cx, 1),
            parsing_task: None,
            is_parsing: false,
            filter_sort_task: None,
            performance_metrics: PerformanceMetrics::default(),
            list_state: gpui::ListState::new(contents.rows.len(), ListAlignment::Top, px(1.))
                .with_uniform_item_height(row_height),
            row_height,
            settings: TabularDataPreviewSettings {
                multiline_cells_enabled: TabularDataSettings::get_global(cx).multiline_cells,
                ..TabularDataPreviewSettings::default()
            },
            last_parse_end_time: None,
            engine: TableDataEngine::default(),
            nav_history: None,
            filter_menu_handle: ui::PopoverMenuHandle::default(),
            _styles_subscription: styles_subscription,
        };

        view.parse_from_active_editor(false, cx);
        view
    }

    pub(crate) fn editor_state(&self) -> &EditorState {
        &self.active_editor_state
    }
    pub(crate) fn apply_sort(&mut self, cx: &mut Context<Self>) {
        self.apply_filter_sort(cx);
    }

    pub fn clear_filters(&mut self, col: types::AnyColumn, cx: &mut Context<Self>) {
        self.engine.clear_filters_for_col(col);
        self.apply_filter_sort(cx);
    }

    pub fn toggle_filter(
        &mut self,
        col: types::AnyColumn,
        value: Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.engine.toggle_filter(col, value);
        self.apply_filter_sort(cx);
    }

    /// Spawns a background task to recompute the display-to-data mapping after a filter or sort
    /// change. Storing the task cancels any previous in-flight computation automatically.
    pub(crate) fn apply_filter_sort(&mut self, cx: &mut Context<Self>) {
        // The one place every change to the filters and the sorting - and every
        // reparse that may have brought a stored one back - passes through.
        self.on_view_changed(cx);

        let contents = self.engine.contents.clone();
        let filter_stack = self.engine.filter_stack.clone();
        let sorting = self.engine.applied_sorting;

        // A grid edit must not leave the table one tick behind the buffer, so anything the parse
        // itself handled synchronously is mapped synchronously too. Sorting is O(n log n), but on
        // a buffer this small the foreground can afford it, and a mapping that lands a frame after
        // the contents would point display rows at rows that no longer exist.
        if contents.source_byte_len <= parser::MAX_SYNC_PARSE_BYTES {
            self.filter_sort_task = None;
            self.engine.set_d2d_mapping(DisplayToDataMapping::compute(
                &contents,
                &filter_stack,
                sorting,
            ));
            self.sync_list_state();
            self.invalidate_search_matches(cx);
            cx.notify();
            return;
        }

        self.filter_sort_task = Some(cx.spawn(async move |this, cx| {
            let mapping = cx
                .background_spawn(async move {
                    DisplayToDataMapping::compute(&contents, &filter_stack, sorting)
                })
                .await;

            this.update(cx, |view, cx| {
                view.engine.set_d2d_mapping(mapping);
                view.sync_list_state();
                view.invalidate_search_matches(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Re-syncs the list with the current number of visible rows while holding on to the scroll
    /// position. `ListState::reset` throws `logical_scroll_top` away, so calling it after every
    /// edit would snap the viewport back to the first row on each keystroke.
    fn sync_list_state(&self) {
        // One item more than the table has rows: the list also holds the
        // trailing "+" row, which is not a row of the file. See
        // [`crate::renderer::render_table::APPEND_ROWS`].
        let list_items = self.engine.d2d_mapping().visible_row_count() + APPEND_ROWS;
        if self.list_state.item_count() == list_items {
            // The list re-renders its visible items on notify anyway. Only variable-height rows can
            // have a stale measurement, and re-measuring costs one pass over every row, which is
            // too much to pay after each keystroke on a large file.
            if self.settings.multiline_cells_effectively_enabled() {
                self.list_state.remeasure_items(0..list_items);
            }
            return;
        }

        let scroll_top = self.list_state.logical_scroll_top();
        // Uses the row height measured on the last render. Cheaper than a full `.measure_all()`
        // pass; exact row heights are re-measured on scrolling.
        self.list_state
            .reset_with_uniform_height(list_items, self.row_height);
        // `scroll_to` clamps to the new row count, so a shrinking table lands on its last row
        // instead of jumping to the top.
        self.list_state.scroll_to(scroll_top);
    }

    pub fn resolve_active_item_as_tabular_data_editor(
        workspace: &Workspace,
        cx: &mut Context<Workspace>,
    ) -> Option<Entity<Editor>> {
        let editor = workspace
            .active_item(cx)
            .and_then(|item| item.act_as::<Editor>(cx))?;
        Self::is_tabular_data_file(&editor, cx).then_some(editor)
    }

    pub fn is_tabular_data_file(editor: &Entity<Editor>, cx: &App) -> bool {
        editor
            .read(cx)
            .buffer()
            .read(cx)
            .as_singleton()
            .and_then(|buffer| buffer.read(cx).file())
            .and_then(|file| file.path().extension())
            .and_then(parser::TabularFormat::from_extension)
            .is_some()
    }
}

impl Focusable for TabularDataPreviewPane {
    /// The cell editor is what actually receives keys and input method events,
    /// so activating the tab has to land focus there rather than on the pane.
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.grid.cell_editor.focus_handle(cx)
    }
}

impl EventEmitter<EditorEvent> for TabularDataPreviewPane {}

impl EventEmitter<SearchEvent> for TabularDataPreviewPane {}

#[derive(Debug, Default)]
pub struct PerformanceMetrics {
    /// Map of timing metrics with their duration and measurement time.
    pub timings: HashMap<&'static str, (Duration, Instant)>,
    /// List of display indices that were rendered in the current frame.
    pub rendered_indices: Vec<usize>,
}
impl PerformanceMetrics {
    pub fn record<F, R>(&mut self, name: &'static str, mut f: F) -> R
    where
        F: FnMut() -> R,
    {
        let start_time = Instant::now();
        let ret = f();
        let duration = start_time.elapsed();
        self.timings.insert(name, (duration, Instant::now()));
        ret
    }

    /// Displays all metrics sorted A-Z in format: `{name}: {took}ms {ago}s ago`
    pub fn display(&self) -> String {
        let mut metrics = self.timings.iter().collect::<Vec<_>>();
        metrics.sort_by_key(|&(name, _)| *name);
        metrics
            .iter()
            .map(|(name, (duration, time))| {
                let took = duration.as_secs_f32() * 1000.;
                let ago = time.elapsed().as_secs();
                format!("{name}: {took:.3}ms {ago}s ago")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Get timing for a specific metric
    pub fn get_timing(&self, name: &str) -> Option<Duration> {
        self.timings.get(name).map(|(duration, _)| *duration)
    }
}

/// Holds state of column widths for a table component in the tabular data preview.
pub(crate) struct ColumnWidths {
    pub widths: Entity<ResizableColumnsState>,
}

impl ColumnWidths {
    pub(crate) fn new(cx: &mut Context<TabularDataPreviewPane>, cols: usize) -> Self {
        Self {
            widths: cx.new(|_cx| {
                ResizableColumnsState::new(
                    cols,
                    vec![AbsoluteLength::Pixels(px(150.)); cols],
                    vec![ui::TableResizeBehavior::Resizable; cols],
                )
            }),
        }
    }
}
