//! The modal behind `markdown::EditTable`.
//!
//! Pipe tables are painful to keep aligned by hand, and pasting a block of
//! spreadsheet cells into one is not something an editor can do. This modal
//! edits the table as a grid of single line editors and writes it back as
//! Markdown, so both stay a matter of typing rather than of counting pipes.

use editor::actions::{Backtab, MoveLeft, MoveRight, Paste, Tab};
use editor::markdown_actions::{
    MarkdownTable, MarkdownTableAlignment, MarkdownTableTarget, display_width,
    parse_clipboard_table,
};
use editor::{Editor, MultiBufferOffset};
use gpui::{
    App, ClickEvent, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    ScrollHandle, TextStyleRefinement, WeakEntity, Window,
};
use settings::Settings as _;
use theme_settings::ThemeSettings;
use ui::{
    ButtonLike, IconButtonShape, Modal, ModalFooter, ModalHeader, Section, Switch,
    SwitchLabelPosition, ToggleState, Tooltip, prelude::*,
};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};
use zed_actions::editor::{MoveDown, MoveUp};
use zed_actions::preview::markdown::EditTable;

use crate::markdown_preview_view::MarkdownPreviewView;

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &EditTable, window, cx| {
        let Some(editor) =
            MarkdownPreviewView::resolve_active_item_as_markdown_editor(workspace, cx)
        else {
            return;
        };
        let Some(target) = editor.update(cx, |editor, cx| editor.markdown_table_target(cx)) else {
            return;
        };
        workspace.toggle_modal(window, cx, move |window, cx| {
            TableEditor::new(editor.downgrade(), target, window, cx)
        });
    });
}

pub struct TableEditor {
    editor: WeakEntity<Editor>,
    target: MarkdownTableTarget,
    /// The cell editors, row major. Every row holds `alignments.len()` cells.
    cells: Vec<Vec<Entity<Editor>>>,
    alignments: Vec<MarkdownTableAlignment>,
    has_header: bool,
    /// The cell keyboard navigation last left focus in. Only a fallback: the
    /// handlers ask the window which cell is really focused, since the mouse
    /// can move focus without going through them.
    active_cell: (usize, usize),
    message: Option<SharedString>,
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
}

impl TableEditor {
    fn new(
        editor: WeakEntity<Editor>,
        target: MarkdownTableTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let has_header = target.table.has_header;
        let alignments = target.table.alignments.clone();
        let cells = target
            .table
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|cell| new_cell_editor(cell, window, cx))
                    .collect()
            })
            .collect();

        Self {
            editor,
            target,
            cells,
            alignments,
            has_header,
            active_cell: (0, 0),
            message: None,
            focus_handle: cx.focus_handle(),
            scroll_handle: ScrollHandle::new(),
        }
    }

    fn column_count(&self) -> usize {
        self.alignments.len()
    }

    fn cell(&self, row: usize, column: usize) -> Option<&Entity<Editor>> {
        self.cells.get(row)?.get(column)
    }

    /// The cell that currently holds focus, falling back to the last one
    /// keyboard navigation moved to.
    fn focused_cell(&self, window: &Window, cx: &App) -> (usize, usize) {
        for (row, cells) in self.cells.iter().enumerate() {
            for (column, cell) in cells.iter().enumerate() {
                if cell.focus_handle(cx).contains_focused(window, cx) {
                    return (row, column);
                }
            }
        }
        self.active_cell
    }

    fn focus_cell(&mut self, row: usize, column: usize, window: &mut Window, cx: &mut App) {
        let Some(cell) = self.cell(row, column).cloned() else {
            return;
        };
        self.active_cell = (row, column);
        cell.focus_handle(cx).focus(window, cx);
    }

    /// Moves focus one cell along the grid in reading order, wrapping at both
    /// ends.
    fn step_cell(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let columns = self.column_count();
        let rows = self.cells.len();
        if columns == 0 || rows == 0 {
            return;
        }
        let (row, column) = self.focused_cell(window, cx);
        let index = row * columns + column;
        let count = rows * columns;
        let next = if forward {
            (index + 1) % count
        } else {
            (index + count - 1) % count
        };
        self.focus_cell(next / columns, next % columns, window, cx);
        cx.notify();
    }

    fn focus_next_cell(&mut self, _: &Tab, window: &mut Window, cx: &mut Context<Self>) {
        self.step_cell(true, window, cx);
        cx.stop_propagation();
    }

    fn focus_previous_cell(&mut self, _: &Backtab, window: &mut Window, cx: &mut Context<Self>) {
        self.step_cell(false, window, cx);
        cx.stop_propagation();
    }

    fn focus_cell_above(&mut self, _: &MoveUp, window: &mut Window, cx: &mut Context<Self>) {
        let (row, column) = self.focused_cell(window, cx);
        if row > 0 {
            self.focus_cell(row - 1, column, window, cx);
            cx.notify();
        }
        cx.stop_propagation();
    }

    fn focus_cell_below(&mut self, _: &MoveDown, window: &mut Window, cx: &mut Context<Self>) {
        let (row, column) = self.focused_cell(window, cx);
        if row + 1 < self.cells.len() {
            self.focus_cell(row + 1, column, window, cx);
            cx.notify();
        }
        cx.stop_propagation();
    }

    /// Enter walks down the column, which is how a spreadsheet behaves. The
    /// table is written back with `menu::SecondaryConfirm` instead.
    fn confirm_moves_down(
        &mut self,
        _: &menu::Confirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_cell_below(&MoveDown, window, cx);
    }

    fn move_left(&mut self, _: &MoveLeft, window: &mut Window, cx: &mut Context<Self>) {
        let (row, column) = self.focused_cell(window, cx);
        let at_start = self
            .cell(row, column)
            .cloned()
            .is_some_and(|cell| cell.update(cx, |cell, cx| collapsed_cursor(cell, cx) == Some(0)));
        if !at_start {
            return;
        }
        self.step_cell(false, window, cx);
        self.move_cursor_to_edge(false, window, cx);
        cx.stop_propagation();
    }

    fn move_right(&mut self, _: &MoveRight, window: &mut Window, cx: &mut Context<Self>) {
        let (row, column) = self.focused_cell(window, cx);
        let at_end = self.cell(row, column).cloned().is_some_and(|cell| {
            cell.update(cx, |cell, cx| {
                collapsed_cursor(cell, cx) == Some(cell_length(cell, cx))
            })
        });
        if !at_end {
            return;
        }
        self.step_cell(true, window, cx);
        self.move_cursor_to_edge(true, window, cx);
        cx.stop_propagation();
    }

    /// Puts the cursor at the end of the cell focus just arrived in when moving
    /// left, and at its start when moving right, so the caret keeps travelling
    /// in the direction the arrow key points.
    fn move_cursor_to_edge(&mut self, to_start: bool, window: &mut Window, cx: &mut App) {
        let (row, column) = self.active_cell;
        let Some(cell) = self.cell(row, column).cloned() else {
            return;
        };
        cell.update(cx, |cell, cx| {
            let offset = if to_start {
                MultiBufferOffset(0)
            } else {
                MultiBufferOffset(cell_length(cell, cx))
            };
            cell.change_selections(Default::default(), window, cx, |selections| {
                selections.select_ranges([offset..offset]);
            });
        });
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let Some(values) = parse_clipboard_table(&text) else {
            return;
        };
        let (row, column) = self.focused_cell(window, cx);
        self.write_values(values, row, column, window, cx);
        self.message = None;
        cx.notify();
        cx.stop_propagation();
    }

    fn paste_whole_table(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let values = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .and_then(|text| parse_clipboard_table(&text));
        let Some(values) = values else {
            self.message =
                Some(i18n::t!("The clipboard has nothing that can be read as a table.").into());
            cx.notify();
            return;
        };

        let columns = values.iter().map(Vec::len).max().unwrap_or(1).max(1);
        self.alignments = vec![MarkdownTableAlignment::Unspecified; columns];
        self.cells = values
            .iter()
            .map(|row| {
                (0..columns)
                    .map(|column| {
                        let text = row.get(column).map(String::as_str).unwrap_or("");
                        new_cell_editor(text, window, cx)
                    })
                    .collect()
            })
            .collect();
        if self.cells.is_empty() {
            self.cells.push(Vec::new());
        }
        self.message = None;
        self.focus_cell(0, 0, window, cx);
        cx.notify();
    }

    /// Writes a pasted block of cells with its top left corner at
    /// `row`/`column`, growing the grid when the block runs past its edge.
    fn write_values(
        &mut self,
        values: Vec<Vec<String>>,
        row: usize,
        column: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let needed_columns = column + values.iter().map(Vec::len).max().unwrap_or(0);
        while self.column_count() < needed_columns {
            self.add_column(window, cx);
        }
        while self.cells.len() < row + values.len() {
            self.add_row(window, cx);
        }

        for (row_offset, value_row) in values.iter().enumerate() {
            for (column_offset, value) in value_row.iter().enumerate() {
                let Some(cell) = self.cell(row + row_offset, column + column_offset) else {
                    continue;
                };
                cell.update(cx, |cell, cx| {
                    cell.set_text(value.clone(), window, cx);
                });
            }
        }
    }

    /// Inserts a blank row, returning the index it actually landed at.
    fn insert_row(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) -> usize {
        let index = index.min(self.cells.len());
        let row = (0..self.column_count())
            .map(|_| new_cell_editor("", window, cx))
            .collect();
        self.cells.insert(index, row);
        cx.notify();
        index
    }

    /// Inserts a blank column, returning the index it actually landed at.
    fn insert_column(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let index = index.min(self.column_count());
        self.alignments
            .insert(index, MarkdownTableAlignment::Unspecified);
        for row in 0..self.cells.len() {
            let cell = new_cell_editor("", window, cx);
            if let Some(cells) = self.cells.get_mut(row) {
                let index = index.min(cells.len());
                cells.insert(index, cell);
            }
        }
        cx.notify();
        index
    }

    /// Asking a row for a neighbour is the start of typing into that neighbour,
    /// so the caret follows the new row and keeps the column it was in.
    fn insert_row_and_focus(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let (_, column) = self.focused_cell(window, cx);
        let index = self.insert_row(index, window, cx);
        self.focus_cell(index, column, window, cx);
    }

    fn insert_column_and_focus(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (row, _) = self.focused_cell(window, cx);
        let index = self.insert_column(index, window, cx);
        self.focus_cell(row, index, window, cx);
    }

    fn add_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.insert_row(self.cells.len(), window, cx);
    }

    fn add_column(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.insert_column(self.column_count(), window, cx);
    }

    fn remove_row(&mut self, row: usize, cx: &mut Context<Self>) {
        if self.cells.len() <= 1 || row >= self.cells.len() {
            return;
        }
        self.cells.remove(row);
        self.active_cell.0 = self.active_cell.0.min(self.cells.len().saturating_sub(1));
        cx.notify();
    }

    fn remove_column(&mut self, column: usize, cx: &mut Context<Self>) {
        if self.column_count() <= 1 || column >= self.column_count() {
            return;
        }
        self.alignments.remove(column);
        for cells in &mut self.cells {
            if column < cells.len() {
                cells.remove(column);
            }
        }
        self.active_cell.1 = self
            .active_cell
            .1
            .min(self.column_count().saturating_sub(1));
        cx.notify();
    }

    fn swap_rows(&mut self, row: usize, other: usize, cx: &mut Context<Self>) {
        if row < self.cells.len() && other < self.cells.len() {
            self.cells.swap(row, other);
            cx.notify();
        }
    }

    fn swap_columns(&mut self, column: usize, other: usize, cx: &mut Context<Self>) {
        if column >= self.column_count() || other >= self.column_count() {
            return;
        }
        self.alignments.swap(column, other);
        for cells in &mut self.cells {
            if column < cells.len() && other < cells.len() {
                cells.swap(column, other);
            }
        }
        cx.notify();
    }

    fn cycle_alignment(&mut self, column: usize, cx: &mut Context<Self>) {
        if let Some(alignment) = self.alignments.get_mut(column) {
            *alignment = alignment.next_alignment();
            cx.notify();
        }
    }

    /// The sizes the grid is drawn with, which follow the window as well as the
    /// content: a table in a 4K window gets wide cells and a tall grid instead
    /// of a dialog stranded in the middle of the screen, while a small window
    /// keeps the sideways scrolling it had.
    fn grid_metrics(&self, window: &Window, cx: &App) -> GridMetrics {
        let settings = ThemeSettings::get_global(cx);
        let font_size = settings.buffer_font_size(cx);
        let font_id = window.text_system().resolve_font(&settings.buffer_font);
        // A column width is only a layout hint, so a font the text system cannot
        // measure falls back to a rough advance rather than failing the render.
        let em_width = window
            .text_system()
            .em_width(font_id, font_size)
            .unwrap_or(font_size * 0.6);

        let rem_size = window.rem_size();
        let viewport = window.viewport_size();
        let minimum_column_width = MINIMUM_COLUMN_WIDTH.to_pixels(rem_size);
        let maximum_column_width = MAXIMUM_COLUMN_WIDTH.to_pixels(rem_size);
        let column_padding = COLUMN_PADDING.to_pixels(rem_size);

        let mut column_widths = (0..self.column_count())
            .map(|column| {
                let widest_cell = self
                    .cells
                    .iter()
                    .filter_map(|cells| cells.get(column))
                    .map(|cell| display_width(&cell.read(cx).text(cx)))
                    .max()
                    .unwrap_or(0);
                (em_width * widest_cell as f32 + column_padding)
                    .max(minimum_column_width)
                    .min(maximum_column_width)
            })
            .collect::<Vec<_>>();

        let chrome_width = CELL_GAP.to_pixels(rem_size) * column_widths.len() as f32
            + ROW_CONTROLS_WIDTH.to_pixels(rem_size)
            + MODAL_HORIZONTAL_PADDING.to_pixels(rem_size);
        let maximum_modal_width = viewport.width * MAXIMUM_WIDTH_FRACTION;
        let minimum_modal_width = MINIMUM_MODAL_WIDTH
            .to_pixels(rem_size)
            .min(maximum_modal_width);
        let modal_width = (column_widths.iter().sum::<Pixels>() + chrome_width)
            .max(minimum_modal_width)
            .min(maximum_modal_width);

        // Room the content did not ask for goes to the cells rather than to a
        // margin around them, which is what makes the grid feel bigger on a
        // large display instead of only wider.
        let available_for_columns = modal_width - chrome_width;
        let requested_by_content = column_widths.iter().sum::<Pixels>();
        if !column_widths.is_empty() && available_for_columns > requested_by_content {
            let spare = (available_for_columns - requested_by_content) / column_widths.len() as f32;
            for width in &mut column_widths {
                *width = (*width + spare).min(maximum_column_width);
            }
        }

        let grid_width = column_widths.iter().sum::<Pixels>()
            + CELL_GAP.to_pixels(rem_size) * column_widths.len() as f32
            + ROW_CONTROLS_WIDTH.to_pixels(rem_size);

        let height_budget = viewport.height * MAXIMUM_HEIGHT_FRACTION;
        let maximum_grid_height = (height_budget - MODAL_CHROME_HEIGHT.to_pixels(rem_size))
            .max(MINIMUM_GRID_HEIGHT.to_pixels(rem_size).min(height_budget));

        GridMetrics {
            column_widths,
            grid_width,
            modal_width,
            maximum_grid_height,
        }
    }

    fn table(&self, cx: &App) -> MarkdownTable {
        let mut table = MarkdownTable {
            has_header: self.has_header,
            alignments: self.alignments.clone(),
            rows: self
                .cells
                .iter()
                .map(|row| row.iter().map(|cell| cell.read(cx).text(cx)).collect())
                .collect(),
        };
        table.normalize();
        table
    }

    fn confirm(&mut self, _: &menu::SecondaryConfirm, window: &mut Window, cx: &mut Context<Self>) {
        let table = self.table(cx);
        let target = &self.target;
        self.editor
            .update(cx, |editor, cx| {
                editor.write_markdown_table(target, &table, window, cx);
            })
            .log_err();
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .w_full()
            .gap_2()
            .child(
                Button::new("markdown-table-paste-all", i18n::t!("Paste Whole Table"))
                    .style(ButtonStyle::Outlined)
                    .size(ButtonSize::Medium)
                    .start_icon(Icon::new(IconName::Table))
                    .tooltip(Tooltip::text(i18n::t!(
                        "Replaces the whole table with the Excel / CSV contents of the clipboard."
                    )))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.paste_whole_table(window, cx);
                    })),
            )
            .child(
                Switch::new("markdown-table-has-header", self.has_header.into())
                    .label(i18n::t!("Header Row"))
                    // Without a position the label is built but never rendered.
                    .label_position(SwitchLabelPosition::Start)
                    .on_click(cx.listener(|this, state: &ToggleState, _window, cx| {
                        this.has_header = state.selected();
                        cx.notify();
                    })),
            )
            .child(div().flex_1())
            .child(
                Label::new(i18n::t!(
                    "Tab / arrow keys move between cells. Ctrl+Enter applies the table to the document."
                ))
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .into_any_element()
    }

    fn render_column_controls(&self, metrics: &GridMetrics, cx: &mut Context<Self>) -> AnyElement {
        let last_column = self.column_count().saturating_sub(1);
        let mut row = h_flex().w(metrics.grid_width).flex_none().gap(CELL_GAP);
        for (column, width) in metrics.column_widths.iter().copied().enumerate() {
            let alignment = self
                .alignments
                .get(column)
                .copied()
                .unwrap_or(MarkdownTableAlignment::Unspecified);
            row = row.child(
                v_flex()
                    .w(width)
                    .flex_none()
                    .gap_px()
                    // The two insertion buttons sit against the edges of the
                    // column they would insert next to, so their arrows point at
                    // where the new column lands.
                    .child(
                        h_flex()
                            .w_full()
                            .justify_between()
                            .child(insert_button(
                                SharedString::from(format!(
                                    "markdown-table-column-insert-left-{column}"
                                )),
                                IconName::ArrowLeft,
                                i18n::t!("Insert Column to the Left"),
                                false,
                                cx.listener(move |this, _, window, cx| {
                                    let index = column_insertion_index(column, InsertSide::Before);
                                    this.insert_column_and_focus(index, window, cx);
                                }),
                            ))
                            .child(
                                Button::new(
                                    SharedString::from(format!("markdown-table-align-{column}")),
                                    alignment.label(),
                                )
                                .label_size(LabelSize::Small)
                                .tooltip(Tooltip::text(
                                    i18n::t!("Column alignment (click to cycle: default → left → center → right)"),
                                ))
                                .on_click(cx.listener(
                                    move |this, _, _window, cx| {
                                        this.cycle_alignment(column, cx);
                                    },
                                )),
                            )
                            .child(insert_button(
                                SharedString::from(format!(
                                    "markdown-table-column-insert-right-{column}"
                                )),
                                IconName::ArrowRight,
                                i18n::t!("Insert Column to the Right"),
                                false,
                                cx.listener(move |this, _, window, cx| {
                                    let index = column_insertion_index(column, InsertSide::After);
                                    this.insert_column_and_focus(index, window, cx);
                                }),
                            )),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .justify_center()
                            .gap_px()
                            .child(
                                IconButton::new(
                                    SharedString::from(format!(
                                        "markdown-table-column-left-{column}"
                                    )),
                                    IconName::ArrowLeft,
                                )
                                .shape(IconButtonShape::Square)
                                .icon_size(IconSize::XSmall)
                                .disabled(column == 0)
                                .tooltip(Tooltip::text(i18n::t!("Move Column Left")))
                                .on_click(cx.listener(
                                    move |this, _, _window, cx| {
                                        this.swap_columns(column, column.saturating_sub(1), cx);
                                    },
                                )),
                            )
                            .child(
                                IconButton::new(
                                    SharedString::from(format!(
                                        "markdown-table-column-right-{column}"
                                    )),
                                    IconName::ArrowRight,
                                )
                                .shape(IconButtonShape::Square)
                                .icon_size(IconSize::XSmall)
                                .disabled(column == last_column)
                                .tooltip(Tooltip::text(i18n::t!("Move Column Right")))
                                .on_click(cx.listener(
                                    move |this, _, _window, cx| {
                                        this.swap_columns(column, column + 1, cx);
                                    },
                                )),
                            )
                            .child(
                                IconButton::new(
                                    SharedString::from(format!(
                                        "markdown-table-column-remove-{column}"
                                    )),
                                    IconName::Trash,
                                )
                                .shape(IconButtonShape::Square)
                                .icon_size(IconSize::XSmall)
                                .disabled(self.column_count() <= 1)
                                .tooltip(Tooltip::text(i18n::t!("Delete Column")))
                                .on_click(cx.listener(
                                    move |this, _, _window, cx| {
                                        this.remove_column(column, cx);
                                    },
                                )),
                            ),
                    ),
            );
        }
        row.child(div().w(ROW_CONTROLS_WIDTH).flex_none())
            .into_any_element()
    }

    fn render_row(
        &self,
        row: usize,
        metrics: &GridMetrics,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_header = self.has_header && row == 0;
        let last_row = self.cells.len().saturating_sub(1);
        let header_background = cx.theme().colors().element_background;
        let insert_above = row_insertion_index(row, InsertSide::Before, self.has_header);
        let insert_below = row_insertion_index(row, InsertSide::After, self.has_header);

        let mut line = h_flex()
            .w(metrics.grid_width)
            .flex_none()
            .gap(CELL_GAP)
            .mb_1();
        for (column, width) in metrics.column_widths.iter().copied().enumerate() {
            let Some(cell) = self.cell(row, column) else {
                continue;
            };
            let focused = cell.focus_handle(cx).contains_focused(window, cx);
            line = line.child(
                ui::input_frame_sized(ui::InputFrameSize::Compact, focused, cx)
                    .w(width)
                    .flex_none()
                    // The header row keeps its own tint so it stays distinguishable
                    // from the body cells that share the editor background.
                    .when(is_header, |this| this.bg(header_background))
                    .child(cell.clone()),
            );
        }

        line.child(
            h_flex()
                .w(ROW_CONTROLS_WIDTH)
                .flex_none()
                .gap_px()
                .child(insert_button(
                    SharedString::from(format!("markdown-table-row-insert-above-{row}")),
                    IconName::ArrowUp,
                    if insert_above.is_some() {
                        i18n::t!("Insert Row Above")
                    } else {
                        i18n::t!("A row cannot be inserted above the header row")
                    },
                    insert_above.is_none(),
                    cx.listener(move |this, _, window, cx| {
                        if let Some(index) = insert_above {
                            this.insert_row_and_focus(index, window, cx);
                        }
                    }),
                ))
                .child(insert_button(
                    SharedString::from(format!("markdown-table-row-insert-below-{row}")),
                    IconName::ArrowDown,
                    i18n::t!("Insert Row Below"),
                    insert_below.is_none(),
                    cx.listener(move |this, _, window, cx| {
                        if let Some(index) = insert_below {
                            this.insert_row_and_focus(index, window, cx);
                        }
                    }),
                ))
                // Keeps the insertion buttons from reading as a fifth arrow in
                // the run of move buttons.
                .child(div().w_1())
                .child(
                    IconButton::new(
                        SharedString::from(format!("markdown-table-row-up-{row}")),
                        IconName::ArrowUp,
                    )
                    .shape(IconButtonShape::Square)
                    .icon_size(IconSize::XSmall)
                    .disabled(row == 0)
                    .tooltip(Tooltip::text(i18n::t!("Move Row Up")))
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.swap_rows(row, row.saturating_sub(1), cx);
                    })),
                )
                .child(
                    IconButton::new(
                        SharedString::from(format!("markdown-table-row-down-{row}")),
                        IconName::ArrowDown,
                    )
                    .shape(IconButtonShape::Square)
                    .icon_size(IconSize::XSmall)
                    .disabled(row == last_row)
                    .tooltip(Tooltip::text(i18n::t!("Move Row Down")))
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.swap_rows(row, row + 1, cx);
                    })),
                )
                .child(
                    IconButton::new(
                        SharedString::from(format!("markdown-table-row-remove-{row}")),
                        IconName::Trash,
                    )
                    .shape(IconButtonShape::Square)
                    .icon_size(IconSize::XSmall)
                    .disabled(self.cells.len() <= 1)
                    .tooltip(Tooltip::text(i18n::t!("Delete Row")))
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.remove_row(row, cx);
                    })),
                ),
        )
        .into_any_element()
    }

    fn render_grid(
        &self,
        metrics: &GridMetrics,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let column_controls = self.render_column_controls(metrics, cx);
        let mut rows = Vec::with_capacity(self.cells.len());
        for row in 0..self.cells.len() {
            rows.push(self.render_row(row, metrics, window, cx));
        }

        v_flex()
            .id("markdown-table-grid")
            .mt_2()
            .max_h(metrics.maximum_grid_height)
            .overflow_scroll()
            .track_scroll(&self.scroll_handle)
            .child(column_controls)
            .child(div().h_1())
            .children(rows)
            .into_any_element()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> ModalFooter {
        let confirm_label = if self.target.replaces_existing {
            i18n::t!("Update")
        } else {
            i18n::t!("Insert")
        };

        ModalFooter::new()
            .start_slot::<Button>(
                Button::new("markdown-table-add-row", i18n::t!("Add Row at End"))
                    .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
                    .on_click(cx.listener(|this, _, window, cx| this.add_row(window, cx))),
            )
            .end_slot::<AnyElement>(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("markdown-table-add-column", i18n::t!("Add Column at End"))
                            .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.add_column(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("markdown-table-cancel", i18n::t!("Cancel")).on_click(
                            cx.listener(|this, _, window, cx| {
                                this.cancel(&menu::Cancel, window, cx)
                            }),
                        ),
                    )
                    .child(
                        Button::new("markdown-table-confirm", confirm_label)
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&menu::SecondaryConfirm, window, cx)
                            })),
                    )
                    .into_any_element(),
            )
    }
}

/// Wide enough for a handful of Japanese characters without the grid needing to
/// scroll sideways for an ordinary three column table.
const MINIMUM_COLUMN_WIDTH: Rems = Rems(9.5);
/// Past this a long cell scrolls inside its own editor, rather than pushing
/// every other column out of the window.
const MAXIMUM_COLUMN_WIDTH: Rems = Rems(24.);
/// Room for the cell's own padding and the caret past its last glyph.
const COLUMN_PADDING: Rems = Rems(1.5);
/// The gap between two cells. Held as a constant because the grid's width is
/// computed from it as well as laid out with it.
const CELL_GAP: Rems = Rems(0.25);
/// Holds the two insertion buttons and the three move and delete buttons.
const ROW_CONTROLS_WIDTH: Rems = Rems(9.);
/// The width the modal always had, kept as a floor so that a table of short
/// cells does not open narrower than it used to.
const MINIMUM_MODAL_WIDTH: Rems = Rems(56.);
/// The modal's own horizontal padding, which the grid does not get to use.
const MODAL_HORIZONTAL_PADDING: Rems = Rems(2.5);
/// The header, toolbar and footer stacked around the scrolling grid.
const MODAL_CHROME_HEIGHT: Rems = Rems(12.);
/// Rather than show a single row, a window too short for the chrome lets the
/// modal run past its height budget.
const MINIMUM_GRID_HEIGHT: Rems = Rems(12.);
/// How much of the window the modal may take.
const MAXIMUM_WIDTH_FRACTION: f32 = 0.8;
const MAXIMUM_HEIGHT_FRACTION: f32 = 0.7;

/// The sizes [`TableEditor::grid_metrics`] works out for one frame.
struct GridMetrics {
    column_widths: Vec<Pixels>,
    /// One line of the grid: every column, the gaps between them, and the
    /// per-row controls.
    grid_width: Pixels,
    modal_width: Pixels,
    maximum_grid_height: Pixels,
}

/// Which side of an existing row or column a new one is inserted on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InsertSide {
    Before,
    After,
}

/// The index a row inserted on `side` of `row` takes, or `None` when the
/// insertion is not offered.
///
/// A header is simply the table's first row, so a row inserted above it would
/// become the header and demote the one the user wrote. That is refused rather
/// than quietly turned into an insertion below the header.
fn row_insertion_index(row: usize, side: InsertSide, has_header: bool) -> Option<usize> {
    match side {
        InsertSide::Before if has_header && row == 0 => None,
        InsertSide::Before => Some(row),
        InsertSide::After => Some(row + 1),
    }
}

/// The index a column inserted on `side` of `column` takes. Columns have no
/// header of their own, so both sides are always available.
fn column_insertion_index(column: usize, side: InsertSide) -> usize {
    match side {
        InsertSide::Before => column,
        InsertSide::After => column + 1,
    }
}

/// A button whose plus sign and arrow read as "add one here, on this side", so
/// that it cannot be mistaken for the move buttons beside it, which carry the
/// same arrow on its own.
fn insert_button(
    id: impl Into<ElementId>,
    direction: IconName,
    tooltip: &'static str,
    disabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> ButtonLike {
    let color = if disabled {
        Color::Disabled
    } else {
        Color::Muted
    };
    ButtonLike::new(id)
        .size(ButtonSize::Compact)
        .disabled(disabled)
        .tooltip(Tooltip::text(tooltip))
        .child(
            h_flex()
                .gap_px()
                .child(
                    Icon::new(IconName::Plus)
                        .size(IconSize::XSmall)
                        .color(color),
                )
                .child(Icon::new(direction).size(IconSize::XSmall).color(color)),
        )
        .on_click(on_click)
}

fn new_cell_editor(text: &str, window: &mut Window, cx: &mut App) -> Entity<Editor> {
    let settings = ThemeSettings::get_global(cx);
    // A single line editor is styled with the UI font at a fixed 0.875rem. Cells
    // hold buffer text and the user already sized the buffer font to be
    // readable, so follow that instead of the smaller UI text.
    let text_style = TextStyleRefinement {
        font_family: Some(settings.buffer_font.family.clone()),
        font_features: Some(settings.buffer_font.features.clone()),
        font_size: Some(settings.buffer_font_size(cx).into()),
        font_weight: Some(settings.buffer_font.weight),
        ..Default::default()
    };
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_text(text.to_string(), window, cx);
        editor.set_text_style_refinement(text_style);
        editor
    })
}

/// The byte offset of the sole cursor, when this editor holds one collapsed
/// selection. `None` for a selection that spans text, where an arrow key should
/// collapse it rather than leave the cell.
fn collapsed_cursor(editor: &Editor, cx: &mut App) -> Option<usize> {
    let selection = editor
        .selections
        .newest::<MultiBufferOffset>(&editor.display_snapshot(cx));
    if selection.start == selection.end {
        Some(selection.start.0)
    } else {
        None
    }
}

fn cell_length(editor: &Editor, cx: &App) -> usize {
    editor.buffer().read(cx).read(cx).len().0
}

impl Render for TableEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let metrics = self.grid_metrics(window, cx);
        let toolbar = self.render_toolbar(cx);
        let grid = self.render_grid(&metrics, window, cx);
        let footer = self.render_footer(cx);
        let message = self.message.clone();

        div()
            .key_context("MarkdownTableEditor")
            .track_focus(&self.focus_handle)
            .elevation_3(cx)
            .w(metrics.modal_width)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::confirm_moves_down))
            .capture_action(cx.listener(Self::focus_next_cell))
            .capture_action(cx.listener(Self::focus_previous_cell))
            .capture_action(cx.listener(Self::focus_cell_above))
            .capture_action(cx.listener(Self::focus_cell_below))
            .capture_action(cx.listener(Self::move_left))
            .capture_action(cx.listener(Self::move_right))
            .capture_action(cx.listener(Self::paste))
            .child(
                Modal::new("markdown-table-editor", None)
                    .header(ModalHeader::new().headline(i18n::t!("Edit Table")).description(
                        i18n::t!("Edit the cells directly. Pasting a range from Excel or a spreadsheet spreads it across the table."),
                    ))
                    .section(
                        Section::new()
                            .child(toolbar)
                            .children(message.map(|message| {
                                Label::new(message).size(LabelSize::Small).color(Color::Warning)
                            }))
                            .child(grid),
                    )
                    .footer(footer),
            )
    }
}

impl Focusable for TableEditor {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        let (row, column) = self.active_cell;
        match self.cell(row, column) {
            Some(cell) => cell.focus_handle(cx),
            None => self.focus_handle.clone(),
        }
    }
}

impl EventEmitter<DismissEvent> for TableEditor {}

impl ModalView for TableEditor {
    /// The modal owns a live handle on the editor it will write to and a grid
    /// of buffers of its own; reviving a stale copy later would write a table
    /// against row anchors the buffer has moved on from.
    fn stash_when_dismissed(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_row_insertion_index() {
        assert_eq!(row_insertion_index(0, InsertSide::Before, false), Some(0));
        assert_eq!(row_insertion_index(0, InsertSide::After, false), Some(1));
        assert_eq!(row_insertion_index(2, InsertSide::Before, false), Some(2));
        assert_eq!(row_insertion_index(2, InsertSide::After, false), Some(3));
    }

    #[test]
    fn test_row_insertion_index_keeps_the_header_first() {
        assert_eq!(row_insertion_index(0, InsertSide::Before, true), None);
        assert_eq!(row_insertion_index(0, InsertSide::After, true), Some(1));
        assert_eq!(row_insertion_index(1, InsertSide::Before, true), Some(1));
        assert_eq!(row_insertion_index(1, InsertSide::After, true), Some(2));
    }

    #[test]
    fn test_column_insertion_index() {
        assert_eq!(column_insertion_index(0, InsertSide::Before), 0);
        assert_eq!(column_insertion_index(0, InsertSide::After), 1);
        assert_eq!(column_insertion_index(3, InsertSide::Before), 3);
        assert_eq!(column_insertion_index(3, InsertSide::After), 4);
    }
}
