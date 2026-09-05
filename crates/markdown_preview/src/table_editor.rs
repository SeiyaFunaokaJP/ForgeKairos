//! The modal behind `markdown::EditTable`.
//!
//! Pipe tables are painful to keep aligned by hand, and pasting a block of
//! spreadsheet cells into one is not something an editor can do. This modal
//! edits the table as a grid of single line editors and writes it back as
//! Markdown, so both stay a matter of typing rather than of counting pipes.

use editor::actions::{Backtab, MoveLeft, MoveRight, Paste, Tab};
use editor::markdown_actions::{
    MarkdownTable, MarkdownTableAlignment, MarkdownTableTarget, parse_clipboard_table,
};
use editor::{Editor, MultiBufferOffset};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, ScrollHandle,
    WeakEntity, Window,
};
use ui::{
    IconButtonShape, Modal, ModalFooter, ModalHeader, Section, Switch, ToggleState, Tooltip,
    prelude::*,
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
            self.message = Some("クリップボードに表として読み取れる内容がありません。".into());
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

    fn add_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let row = (0..self.column_count())
            .map(|_| new_cell_editor("", window, cx))
            .collect();
        self.cells.push(row);
        cx.notify();
    }

    fn add_column(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.alignments.push(MarkdownTableAlignment::Unspecified);
        for row in 0..self.cells.len() {
            let cell = new_cell_editor("", window, cx);
            if let Some(cells) = self.cells.get_mut(row) {
                cells.push(cell);
            }
        }
        cx.notify();
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
                Button::new("markdown-table-paste-all", "表全体を貼り付け")
                    .tooltip(Tooltip::text(
                        "クリップボードの Excel / CSV の内容で表全体を置き換えます。",
                    ))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.paste_whole_table(window, cx);
                    })),
            )
            .child(
                Switch::new("markdown-table-has-header", self.has_header.into())
                    .label("ヘッダー行")
                    .on_click(cx.listener(|this, state: &ToggleState, _window, cx| {
                        this.has_header = state.selected();
                        cx.notify();
                    })),
            )
            .child(div().flex_1())
            .child(
                Label::new("Tab / 矢印キーでセルを移動。Ctrl+Enter で書き出し")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element()
    }

    fn render_column_controls(&self, cx: &mut Context<Self>) -> AnyElement {
        let last_column = self.column_count().saturating_sub(1);
        let mut row = h_flex().gap_1();
        for column in 0..self.column_count() {
            let alignment = self
                .alignments
                .get(column)
                .copied()
                .unwrap_or(MarkdownTableAlignment::Unspecified);
            row = row.child(
                h_flex()
                    .w(COLUMN_WIDTH)
                    .gap_px()
                    .child(
                        Button::new(
                            SharedString::from(format!("markdown-table-align-{column}")),
                            alignment.label(),
                        )
                        .label_size(LabelSize::Small)
                        .tooltip(Tooltip::text("列の配置 (クリックで 既定 → 左 → 中央 → 右)"))
                        .on_click(cx.listener(
                            move |this, _, _window, cx| {
                                this.cycle_alignment(column, cx);
                            },
                        )),
                    )
                    .child(
                        IconButton::new(
                            SharedString::from(format!("markdown-table-column-left-{column}")),
                            IconName::ArrowLeft,
                        )
                        .shape(IconButtonShape::Square)
                        .icon_size(IconSize::XSmall)
                        .disabled(column == 0)
                        .tooltip(Tooltip::text("列を左へ移動"))
                        .on_click(cx.listener(
                            move |this, _, _window, cx| {
                                this.swap_columns(column, column.saturating_sub(1), cx);
                            },
                        )),
                    )
                    .child(
                        IconButton::new(
                            SharedString::from(format!("markdown-table-column-right-{column}")),
                            IconName::ArrowRight,
                        )
                        .shape(IconButtonShape::Square)
                        .icon_size(IconSize::XSmall)
                        .disabled(column == last_column)
                        .tooltip(Tooltip::text("列を右へ移動"))
                        .on_click(cx.listener(
                            move |this, _, _window, cx| {
                                this.swap_columns(column, column + 1, cx);
                            },
                        )),
                    )
                    .child(
                        IconButton::new(
                            SharedString::from(format!("markdown-table-column-remove-{column}")),
                            IconName::Trash,
                        )
                        .shape(IconButtonShape::Square)
                        .icon_size(IconSize::XSmall)
                        .disabled(self.column_count() <= 1)
                        .tooltip(Tooltip::text("列を削除"))
                        .on_click(cx.listener(
                            move |this, _, _window, cx| {
                                this.remove_column(column, cx);
                            },
                        )),
                    ),
            );
        }
        row.child(div().w(ROW_CONTROLS_WIDTH)).into_any_element()
    }

    fn render_row(&self, row: usize, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let is_header = self.has_header && row == 0;
        let last_row = self.cells.len().saturating_sub(1);
        let border = cx.theme().colors().border;
        let border_focused = cx.theme().colors().border_focused;
        let header_background = cx.theme().colors().element_background;

        let mut line = h_flex().gap_1().mb_1();
        for column in 0..self.column_count() {
            let Some(cell) = self.cell(row, column) else {
                continue;
            };
            let focused = cell.focus_handle(cx).contains_focused(window, cx);
            line = line.child(
                div()
                    .w(COLUMN_WIDTH)
                    .px_1()
                    .py_0p5()
                    .rounded_sm()
                    .border_1()
                    .border_color(if focused { border_focused } else { border })
                    .when(is_header, |this| this.bg(header_background))
                    .child(cell.clone()),
            );
        }

        line.child(
            h_flex()
                .w(ROW_CONTROLS_WIDTH)
                .gap_px()
                .child(
                    IconButton::new(
                        SharedString::from(format!("markdown-table-row-up-{row}")),
                        IconName::ArrowUp,
                    )
                    .shape(IconButtonShape::Square)
                    .icon_size(IconSize::XSmall)
                    .disabled(row == 0)
                    .tooltip(Tooltip::text("行を上へ移動"))
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
                    .tooltip(Tooltip::text("行を下へ移動"))
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
                    .tooltip(Tooltip::text("行を削除"))
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.remove_row(row, cx);
                    })),
                ),
        )
        .into_any_element()
    }

    fn render_grid(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let column_controls = self.render_column_controls(cx);
        let mut rows = Vec::with_capacity(self.cells.len());
        for row in 0..self.cells.len() {
            rows.push(self.render_row(row, window, cx));
        }

        v_flex()
            .id("markdown-table-grid")
            .mt_2()
            .max_h(rems(22.))
            .overflow_scroll()
            .track_scroll(&self.scroll_handle)
            .child(column_controls)
            .child(div().h_1())
            .children(rows)
            .into_any_element()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> ModalFooter {
        let confirm_label = if self.target.replaces_existing {
            "更新"
        } else {
            "挿入"
        };

        ModalFooter::new()
            .start_slot::<Button>(
                Button::new("markdown-table-add-row", "行を追加")
                    .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
                    .on_click(cx.listener(|this, _, window, cx| this.add_row(window, cx))),
            )
            .end_slot::<AnyElement>(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("markdown-table-add-column", "列を追加")
                            .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.add_column(window, cx)),
                            ),
                    )
                    .child(Button::new("markdown-table-cancel", "キャンセル").on_click(
                        cx.listener(|this, _, window, cx| this.cancel(&menu::Cancel, window, cx)),
                    ))
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
const COLUMN_WIDTH: Rems = Rems(9.5);
/// Holds the three per-row buttons.
const ROW_CONTROLS_WIDTH: Rems = Rems(5.);

fn new_cell_editor(text: &str, window: &mut Window, cx: &mut App) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_text(text.to_string(), window, cx);
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
        let toolbar = self.render_toolbar(cx);
        let grid = self.render_grid(window, cx);
        let footer = self.render_footer(cx);
        let message = self.message.clone();

        div()
            .key_context("MarkdownTableEditor")
            .track_focus(&self.focus_handle)
            .elevation_3(cx)
            .w(rems(56.))
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
                    .header(ModalHeader::new().headline("表を編集").description(
                        "セルを直接編集できます。Excel やスプレッドシートの範囲をそのまま貼り付けると、表に展開します。",
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
