//! The side by side table comparison: one tab holding two panes of cells, the
//! left one what the table is being compared against and the right one what it
//! is now, with the rows of the two lined up against each other.
//!
//! The comparison itself lives in [`crate::table_diff`]; this module is the
//! view over it, and the three entry points that build one.

use std::ops::Range;

use anyhow::Result;
use buffer_diff::BufferDiff;
use editor::markdown_actions::display_width;
use gpui::{
    Action as _, AnyElement, App, AppContext as _, Entity, EventEmitter, FocusHandle, Focusable,
    HighlightStyle, PathPromptOptions, ScrollHandle, ScrollStrategy, StyledText, Subscription,
    Task, UniformListScrollHandle, actions, uniform_list,
};
use language::{Buffer, BufferEvent};
use project::{DirectoryLister, Project};
use settings::Settings as _;
use theme_settings::ThemeSettings;
use ui::{
    ActiveTheme as _, ButtonSize, Checkbox, ContextMenu, DropdownMenu, ElementId, Icon, IconButton,
    IconName, IconSize, Label, LabelCommon as _, LabelSize, ScrollAxes, Scrollbars, SharedString,
    StyledTypography as _, ToggleState, Tooltip, WithScrollbar as _, div, h_flex, prelude::*,
    v_flex,
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
        CellCharDiff, DiffRow, DiffRowKind, DiffSide, TableDiff, cell_at, cell_char_diff,
    },
    types::AnyColumn,
};

actions!(
    tabular_data,
    [DiffWithHead, DiffWithFile, DiffSelectedCells]
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

/// Where the two sides of a comparison come from, which is what decides whether
/// it follows the file as the user edits it.
enum DiffSource {
    /// Two whole tables as text. The right hand one is re-read from the buffer
    /// whenever it changes, so the comparison keeps up with the grid beside it;
    /// the left hand one is a committed version or another file and stands.
    Files {
        left_text: SharedString,
        buffer: Entity<Buffer>,
        /// Read separately per side: the version committed for this same file is
        /// laid out with the same delimiter, but another file the user picked is
        /// only a table at all when it is read with its own one, so comparing a
        /// `.csv` against a `.tsv` has to split each side its own way.
        left_delimiter: char,
        right_delimiter: char,
        first_row_is_header: bool,
    },
    /// Two blocks of cells lifted out of one grid, compared as they were when
    /// the comparison was opened.
    Cells,
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
    Sides {
        left: DiffSide,
        right: DiffSide,
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
            DiffInput::Sides { left, right } => (left, right),
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
    left: DiffSide,
    right: DiffSide,
    diff: TableDiff,
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
    _buffer_subscription: Option<Subscription>,
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
            let committed_text = load_committed_text(committed, cx).await;
            let Some(committed_text) = committed_text else {
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
                        Self::from_texts(
                            format!("{name} ↔ HEAD").into(),
                            "HEAD".into(),
                            "現在".into(),
                            committed_text.into(),
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
                Ok(other_text) => {
                    workspace
                        .update_in(cx, |workspace, window, cx| {
                            let view = cx.new(|cx| {
                                Self::from_texts(
                                    format!("{name} ↔ {other_name}").into(),
                                    other_name.clone(),
                                    name.clone().into(),
                                    other_text.into(),
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
            Self::from_rows(
                comparison.title,
                comparison.left_title,
                comparison.right_title,
                comparison.left,
                comparison.right,
                cx,
            )
        });
        workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
    }

    ///// Construction /////

    #[allow(clippy::too_many_arguments)]
    fn from_texts(
        title: SharedString,
        left_title: SharedString,
        right_title: SharedString,
        left_text: SharedString,
        buffer: Entity<Buffer>,
        left_delimiter: char,
        right_delimiter: char,
        first_row_is_header: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.subscribe(&buffer, |this, _buffer, event, cx| {
            if matches!(
                event,
                BufferEvent::Edited { .. } | BufferEvent::Reloaded | BufferEvent::Saved
            ) {
                this.recompute(true, cx);
            }
        });
        let mut view = Self::empty(title, left_title, right_title, cx);
        view.source = DiffSource::Files {
            left_text,
            buffer,
            left_delimiter,
            right_delimiter,
            first_row_is_header,
        };
        view._buffer_subscription = Some(subscription);
        view.recompute(false, cx);
        view
    }

    fn from_rows(
        title: SharedString,
        left_title: SharedString,
        right_title: SharedString,
        left: DiffSide,
        right: DiffSide,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::empty(title, left_title, right_title, cx);
        view.left = left;
        view.right = right;
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
            source: DiffSource::Cells,
            left: DiffSide::default(),
            right: DiffSide::default(),
            diff: TableDiff::compute(&DiffSide::default(), &DiffSide::default()),
            is_computing: false,
            column_widths: Vec::new(),
            row_number_width: px(MINIMUM_COLUMN_WIDTH),
            shown_rows: None,
            current_change: None,
            row_height: cell_line_height(cx) + CELL_FRAME_HEIGHT,
            list_scroll: UniformListScrollHandle::new(),
            horizontal_scroll: ScrollHandle::new(),
            recompute_task: None,
            _buffer_subscription: None,
        }
    }

    /// The two sides as they are now, copied so that the alignment can run off
    /// the UI thread.
    ///
    /// Reading the buffer copies the whole file, so this is taken once the edits
    /// have settled rather than when the event arrives: an edit per keystroke
    /// would otherwise copy a large file on the UI thread on every one of them,
    /// which is exactly what the debounce is there to avoid.
    fn diff_input(&self, cx: &App) -> DiffInput {
        match &self.source {
            DiffSource::Files {
                left_text,
                buffer,
                left_delimiter,
                right_delimiter,
                first_row_is_header,
            } => DiffInput::Texts {
                left_text: left_text.clone(),
                right_text: buffer.read(cx).text().into(),
                left_delimiter: *left_delimiter,
                right_delimiter: *right_delimiter,
                first_row_is_header: *first_row_is_header,
            },
            DiffSource::Cells => DiffInput::Sides {
                left: self.left.clone(),
                right: self.right.clone(),
            },
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
        let blocks = self.diff.change_blocks();
        self.current_change = self
            .current_change
            .filter(|_| !blocks.is_empty())
            .map(|change| change.min(blocks.len().saturating_sub(1)));
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
        let blocks = self.diff.change_blocks();
        if blocks.is_empty() {
            return;
        }
        let change = match (self.current_change, forward) {
            (None, true) => 0,
            (None, false) => blocks.len() - 1,
            (Some(current), true) => (current + 1).min(blocks.len() - 1),
            (Some(current), false) => current.saturating_sub(1),
        };
        self.current_change = Some(change);
        if let Some(&row) = blocks.get(change) {
            self.list_scroll
                .scroll_to_item(self.shown_index_of(row), ScrollStrategy::Center);
        }
        cx.notify();
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
            .child(div().flex_1())
            .child(
                Label::new(counts)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
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
            .child(title(self.left_title.clone()))
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

fn row_number_width(left: &DiffSide, right: &DiffSide) -> Pixels {
    let rows = left.rows.len().max(right.rows.len()) + 1;
    let digits = rows.to_string().len().max(2);
    px(digits as f32 * COLUMN_WIDTH_PER_CHARACTER + ROW_NUMBER_PADDING)
}

fn file_name(buffer: &Entity<Buffer>, cx: &App) -> String {
    buffer
        .read(cx)
        .file()
        .and_then(|file| file.path().file_name().map(ToOwned::to_owned))
        .unwrap_or_else(|| "無題".to_owned())
}

async fn load_committed_text(
    committed: Task<Result<Entity<BufferDiff>>>,
    cx: &mut gpui::AsyncWindowContext,
) -> Option<String> {
    let diff = committed
        .await
        .inspect_err(|error| log::error!("failed to load the committed table: {error}"))
        .ok()?;
    diff.read_with(cx, |diff, cx| diff.base_text_string(cx))
}

async fn load_project_file(
    project: Entity<Project>,
    path: &std::path::Path,
    cx: &mut gpui::AsyncApp,
) -> Result<String> {
    let project_path = cx
        .update(|cx| Workspace::project_path_for_path(project.clone(), path, false, cx))
        .await?
        .1;
    let buffer = project
        .update(cx, |project, cx| project.open_buffer(project_path, cx))
        .await?;
    Ok(buffer.read_with(cx, |buffer, _cx| buffer.text()))
}

/// What a comparison of two blocks of cells inside one grid is made of.
struct CellComparison {
    title: SharedString,
    left_title: SharedString,
    right_title: SharedString,
    left: DiffSide,
    right: DiffSide,
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
        let cells_of = |block: &GridRect| {
            block
                .rows()
                .map(|row| {
                    block
                        .cols()
                        .map(|col| grid.cell_value(GridRow::from_index(row), AnyColumn(col)))
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        };
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
            left: DiffSide::from_rows(cells_of(left)),
            right: DiffSide::from_rows(cells_of(right)),
            left_title,
            right_title,
        });
    }

    let column_of = |column: usize| SharedString::from(format!("列 {}", column_label(column)));
    let row_of = |row: usize| SharedString::from(format!("行 {row}"));
    let cells_down = |column: usize| {
        (rect.first_row..=rect.last_row)
            .map(|row| vec![grid.cell_value(GridRow::from_index(row), AnyColumn(column))])
            .collect::<Vec<_>>()
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
            left: DiffSide::from_rows(cells_down(rect.first_col)),
            right: DiffSide::from_rows(cells_down(rect.last_col)),
        });
    }

    if single_column && rect.first_row != rect.last_row {
        let column = rect.first_col;
        let first = grid.cell_value(GridRow::from_index(rect.first_row), AnyColumn(column));
        let last = grid.cell_value(GridRow::from_index(rect.last_row), AnyColumn(column));
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
            left: DiffSide::from_rows(vec![vec![first]]),
            right: DiffSide::from_rows(vec![vec![last]]),
        });
    }

    if single_column && rect.first_row == rect.last_row {
        if clipboard.is_empty() {
            return None;
        }
        let address = grid.cursor_address();
        let value = grid.cell_value(
            GridRow::from_index(rect.first_row),
            AnyColumn(rect.first_col),
        );
        return Some(CellComparison {
            title: format!("セルの比較 (クリップボード ↔ {address})").into(),
            left_title: "クリップボード".into(),
            right_title: SharedString::from(address),
            left: DiffSide::from_rows(vec![vec![clipboard.to_owned().into()]]),
            right: DiffSide::from_rows(vec![vec![value]]),
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

    fn diff_view(left: &str, right: &str, cx: &mut VisualTestContext) -> Entity<TableDiffView> {
        let left = left.to_owned();
        let right = right.to_owned();
        cx.update(|_window, cx| {
            cx.new(|cx| {
                let mut view = TableDiffView::empty("比較".into(), "左".into(), "右".into(), cx);
                view.left = DiffSide::parse(&left, ',', true);
                view.right = DiffSide::parse(&right, ',', true);
                view.recompute(false, cx);
                view
            })
        })
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
            cx.new(|cx| {
                TableDiffView::from_texts(
                    "比較".into(),
                    "HEAD".into(),
                    "現在".into(),
                    "name,value\na,1\nb,2".into(),
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
        let diff = TableDiff::compute(&comparison.left, &comparison.right);
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
        let diff = TableDiff::compute(&comparison.left, &comparison.right);
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
        let diff = TableDiff::compute(&comparison.left, &comparison.right);
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
}
