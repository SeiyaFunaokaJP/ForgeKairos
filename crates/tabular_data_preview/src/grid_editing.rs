//! Cell editing, the clipboard, row and column operations, and the keyboard
//! and action wiring that drive them.

use editor::{
    EditorMode, MultiBufferOffset,
    actions::{
        AcceptEditPrediction, AcceptNextLineEditPrediction, AcceptNextWordEditPrediction,
        AddSelectionAbove, AddSelectionBelow, AutoIndent, Backspace, Backtab, Cancel,
        ComposeCompletion, ConfirmCodeAction, ConfirmCompletion, ConfirmCompletionReplace,
        ConfirmRename, ConvertToKebabCase, ConvertToLowerCamelCase, ConvertToLowerCase,
        ConvertToOppositeCase, ConvertToSnakeCase, ConvertToTitleCase, ConvertToUpperCamelCase,
        ConvertToUpperCase, Copy, Cut, CutToEndOfLine, Delete, DeleteLine, DeleteToBeginningOfLine,
        DeleteToEndOfLine, DeleteToNextSubwordEnd, DeleteToNextWordEnd,
        DeleteToPreviousSubwordStart, DeleteToPreviousWordStart, DuplicateLineDown,
        DuplicateLineUp, Format, FormatSelections, Indent, JoinLines, KillRingCut, KillRingYank,
        LineDown, LineUp, MoveLineDown, MoveLineUp, MovePageDown, MovePageUp, MoveToBeginning,
        MoveToBeginningOfLine, MoveToEnd, MoveToEndOfLine, MoveToNextWordEnd,
        MoveToPreviousWordStart, Newline, NewlineAbove, NewlineBelow, OpenContextMenu,
        OrganizeImports, Outdent, Paste, Redo, Rename, Rewrap, SelectAll, SelectAllMatches,
        SelectDown, SelectEnclosingSymbol, SelectLargerSyntaxNode, SelectLeft, SelectLine,
        SelectNext, SelectPageDown, SelectPageUp, SelectPrevious, SelectRight,
        SelectSmallerSyntaxNode, SelectToBeginning, SelectToBeginningOfLine, SelectToEnd,
        SelectToEndOfLine, SelectToNextWordEnd, SelectToPreviousWordStart, SelectUp,
        ShowCharacterPalette, SortLinesCaseInsensitive, SortLinesCaseSensitive, Tab,
        ToggleBlockComments, ToggleComments, Transpose, Undo,
    },
};
use gpui::{ClipboardItem, DismissEvent, Div, Focusable as _, MouseButton, Point, point};
use ui::{ContextMenu, prelude::*};
use zed_actions::editor::{MoveDown, MoveUp};

use crate::{
    CancelEditing, ClearCells, CommitAndMoveDown, CommitAndMoveLeft, CommitAndMoveRight,
    CommitAndMoveUp, CommitEditing, CopyCells, CutCells, DeleteColumns, DeleteRows,
    DeleteRowsOrColumns, InsertColumnLeft, InsertColumnRight, InsertLineBreak, InsertRowAbove,
    InsertRowBelow, InsertRowsOrColumns, PasteCells, SelectAllCells, SelectColumn, SelectRow,
    StartEditing, TabularDataPreviewPane, ToggleHeaderRow,
    edit_engine::{
        BufferEdit, CellBlock, EditEngine, RowInsertPosition, RowRef, block_to_clipboard_text,
        clipboard_text_to_block,
    },
    grid::{CellEditMode, GridCell, GridMotion, GridRow, GridSelectionKind},
    types::AnyColumn,
};

/// The tallest a cell editor grows to before it scrolls its own text.
const MAX_CELL_EDITOR_LINES: usize = 8;

/// Registers a capture handler per action that drops it while the grid is not
/// editing a cell, and hands it to the cell editor while it is.
macro_rules! swallow_when_idle {
    ($element:expr, $cx:expr, $($action:ty),+ $(,)?) => {{
        let element = $element;
        $(
            let element = element.capture_action($cx.listener(
                |this: &mut TabularDataPreviewPane, _: &$action, _window: &mut Window, cx| {
                    if this.grid.editing.is_none() {
                        cx.stop_propagation();
                    } else {
                        cx.propagate();
                    }
                },
            ));
        )+
        element
    }};
}

/// Repeats a single pasted value across the selection, the way a spreadsheet
/// fills a range from one cell. Any larger block is pasted as it is.
pub(crate) fn expand_block_to_selection(block: CellBlock, rows: usize, cols: usize) -> CellBlock {
    let single_value = match block.as_slice() {
        [row] => match row.as_slice() {
            [value] => Some(value.clone()),
            _ => None,
        },
        _ => None,
    };
    match single_value {
        Some(value) if rows > 1 || cols > 1 => vec![vec![value; cols.max(1)]; rows.max(1)],
        _ => block,
    }
}

impl TabularDataPreviewPane {
    ///// Cell editor /////

    pub(crate) fn start_editing(
        &mut self,
        mode: CellEditMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // F2 while typing over a cell switches to editing in place, keeping what
        // has been typed so far, exactly as a spreadsheet does.
        if self.grid.editing.is_some() {
            if mode == CellEditMode::Edit {
                self.grid.editing = Some(CellEditMode::Edit);
                cx.notify();
            }
            return;
        }
        let cursor = self.grid.cursor;
        let Some(row_ref) = self.row_ref(cursor.row) else {
            return;
        };
        self.grid.editing = Some(mode);
        self.grid.editing_target = Some((row_ref, cursor.col));
        self.grid.editing_cell = Some(cursor);
        let text = match mode {
            CellEditMode::Edit => self.cell_value_at(row_ref, cursor.col).to_string(),
            CellEditMode::Replace => String::new(),
        };
        self.set_cell_editor_text(&text, window, cx);
        self.focus_cell_editor(window, cx);
        self.scroll_cursor_into_view(window, cx);
        cx.notify();
    }

    pub(crate) fn commit_editing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.grid.editing.is_none() {
            return;
        }
        let target = self.grid.editing_target;
        let text = self.grid.cell_editor.read(cx).text(cx);
        self.grid.editing = None;
        self.grid.editing_target = None;
        self.grid.editing_cell = None;
        self.set_cell_editor_text("", window, cx);

        // The write goes to the row resolved when the edit started, so that a
        // sort or filter applied while the cell was open cannot redirect it.
        if let Some((row_ref, col)) = target
            && text.as_str() != self.cell_value_at(row_ref, col).as_ref()
        {
            self.write_cell(row_ref, col, &text, cx);
        }
        cx.notify();
    }

    pub(crate) fn cancel_editing(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.grid.editing.is_none() {
            return false;
        }
        self.grid.editing = None;
        self.grid.editing_target = None;
        self.grid.editing_cell = None;
        self.set_cell_editor_text("", window, cx);
        cx.notify();
        true
    }

    fn commit_and_move(&mut self, motion: GridMotion, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_editing(window, cx);
        self.move_cursor(motion, false, window, cx);
    }

    fn set_cell_editor_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.grid.cell_editor.clone();
        editor.update(cx, |editor, cx| {
            editor.set_text(text.to_owned(), window, cx);
            let end = MultiBufferOffset(editor.buffer().read(cx).read(cx).len().0);
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_ranges([end..end]);
            });
        });
        self.sync_cell_editor_mode(cx);
    }

    /// A single line editor would hide everything past the first line of a cell
    /// that holds quoted line breaks, so such a cell is edited in an auto
    /// height editor instead.
    fn sync_cell_editor_mode(&mut self, cx: &mut Context<Self>) {
        let editor = self.grid.cell_editor.clone();
        editor.update(cx, |editor, cx| {
            let multiline = editor.text(cx).contains('\n');
            let mode = if multiline {
                EditorMode::AutoHeight {
                    min_lines: 1,
                    max_lines: Some(MAX_CELL_EDITOR_LINES),
                }
            } else {
                EditorMode::SingleLine
            };
            if editor.mode() != &mode {
                editor.set_mode(mode);
                cx.notify();
            }
        });
    }

    /// The overlay editor is always mounted and always focused, so text
    /// appearing in it while no cell is open is how a typed character (or an
    /// input method's preedit) starts an edit.
    pub(crate) fn on_cell_editor_edited(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.grid.editing.is_some() {
            self.sync_cell_editor_mode(cx);
            cx.notify();
            return;
        }
        let text = self.grid.cell_editor.read(cx).text(cx);
        if text.is_empty() {
            return;
        }
        // Nothing typed, and no input method, can put a line break into a single
        // line editor. Text that holds one came from an editor action that the
        // grid did not take over, and committing it would write a line break
        // into a cell the user never opened.
        if text.bytes().any(|byte| byte == b'\n' || byte == b'\r') {
            self.set_cell_editor_text("", window, cx);
            return;
        }
        let cursor = self.grid.cursor;
        let Some(row_ref) = self.row_ref(cursor.row) else {
            self.set_cell_editor_text("", window, cx);
            return;
        };
        self.grid.editing = Some(CellEditMode::Replace);
        self.grid.editing_target = Some((row_ref, cursor.col));
        self.grid.editing_cell = Some(cursor);
        self.scroll_cursor_into_view(window, cx);
        cx.notify();
    }

    fn insert_line_break(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.grid.editing.is_none() {
            self.start_editing(CellEditMode::Edit, window, cx);
        }
        let editor = self.grid.cell_editor.clone();
        editor.update(cx, |editor, cx| {
            editor.insert("\n", window, cx);
        });
        self.sync_cell_editor_mode(cx);
        cx.notify();
    }

    ///// Buffer writes /////

    fn with_edit_engine<R>(&self, cx: &App, build: impl FnOnce(&EditEngine) -> R) -> Option<R> {
        let snapshot = self
            .editor_state()
            .editor
            .read(cx)
            .buffer()
            .read(cx)
            .as_singleton()?
            .read(cx)
            .text_snapshot();
        let delimiter = self.current_delimiter(cx);
        let contents = self.engine.contents.clone();
        Some(build(&EditEngine::new(&contents, &snapshot, delimiter)))
    }

    /// Applies one operation's edits as a single transaction, so that one grid
    /// operation is one undo step.
    fn apply_buffer_edits(&mut self, edits: Vec<BufferEdit>, cx: &mut Context<Self>) {
        if edits.is_empty() {
            return;
        }
        let Some(buffer) = self
            .editor_state()
            .editor
            .read(cx)
            .buffer()
            .read(cx)
            .as_singleton()
        else {
            log::error!("tabular data preview: the previewed editor has no singleton buffer");
            return;
        };
        buffer.update(cx, |buffer, cx| {
            buffer.edit(edits, None, cx);
            buffer.finalize_last_transaction();
        });
        self.reparse_now(cx);
    }

    fn write_cell(&mut self, row: RowRef, col: AnyColumn, value: &str, cx: &mut Context<Self>) {
        let edits = self
            .with_edit_engine(cx, |engine| engine.set_cell(row, col, value))
            .unwrap_or_default();
        self.write_cached_cell(row, col, value);
        self.apply_buffer_edits(edits, cx);
    }

    fn undo_buffer(&mut self, cx: &mut Context<Self>) {
        let Some(buffer) = self
            .editor_state()
            .editor
            .read(cx)
            .buffer()
            .read(cx)
            .as_singleton()
        else {
            return;
        };
        buffer.update(cx, |buffer, cx| {
            buffer.undo(cx);
        });
        self.reparse_now(cx);
        cx.notify();
    }

    fn redo_buffer(&mut self, cx: &mut Context<Self>) {
        let Some(buffer) = self
            .editor_state()
            .editor
            .read(cx)
            .buffer()
            .read(cx)
            .as_singleton()
        else {
            return;
        };
        buffer.update(cx, |buffer, cx| {
            buffer.redo(cx);
        });
        self.reparse_now(cx);
        cx.notify();
    }

    ///// Clipboard /////

    pub(crate) fn copy_cells(&mut self, cx: &mut Context<Self>) -> bool {
        let rect = self.selection_rect();
        if !self.within_cell_budget(rect.cell_count(), cx) {
            return false;
        }
        let rows = self.selected_row_refs();
        let Some(block) = self.with_edit_engine(cx, |engine| engine.copy_block(&rows, rect.cols()))
        else {
            return false;
        };
        if block.is_empty() {
            return false;
        }
        self.clear_status_message();
        cx.write_to_clipboard(ClipboardItem::new_string(block_to_clipboard_text(&block)));
        true
    }

    pub(crate) fn cut_cells(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.copy_cells(cx) {
            self.clear_selected_cells(window, cx);
        }
    }

    pub(crate) fn clear_selected_cells(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let rect = self.selection_rect();
        if !self.within_cell_budget(rect.cell_count(), cx) {
            return;
        }
        let mut cells = Vec::new();
        for row_index in rect.first_row..=rect.last_row {
            let Some(row_ref) = self.row_ref(GridRow::from_index(row_index)) else {
                continue;
            };
            for col in rect.cols() {
                cells.push((row_ref, AnyColumn(col)));
            }
        }
        if cells.is_empty() {
            return;
        }

        let edits = self
            .with_edit_engine(cx, |engine| engine.clear_cells(&cells))
            .unwrap_or_default();
        for (row, col) in &cells {
            self.write_cached_cell(*row, *col, "");
        }
        self.clear_status_message();
        self.apply_buffer_edits(edits, cx);
        cx.notify();
    }

    /// Writes the clipboard over the selection, starting at its top left cell.
    ///
    /// The rows the block lands on are resolved through the display mapping
    /// first, so that a sorted or filtered table is written the way it is read.
    /// The engine turns the whole thing - rows it has to append, cells it has to
    /// grow a row into - into one batch of edits, so the paste is one undo step.
    pub(crate) fn paste_cells(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let rect = self.selection_rect();
        let block = expand_block_to_selection(
            clipboard_text_to_block(&text),
            rect.last_row - rect.first_row + 1,
            rect.last_col - rect.first_col + 1,
        );
        let cell_count: usize = block.iter().map(Vec::len).sum();
        if cell_count == 0 || !self.within_cell_budget(cell_count, cx) {
            return;
        }

        // Only the rows the table already has are named; the mapping is dense,
        // so the first display row that does not resolve is the end of it, and
        // every block row from there on becomes a new row.
        let mut rows = Vec::with_capacity(block.len());
        for offset in 0..block.len() {
            let Some(row_ref) = self.row_ref(GridRow::from_index(rect.first_row + offset)) else {
                break;
            };
            rows.push(row_ref);
        }

        let first_col = AnyColumn(rect.first_col);
        let edits = self
            .with_edit_engine(cx, |engine| {
                engine.paste_block_rows(&rows, first_col, &block)
            })
            .unwrap_or_default();

        // Only the rows that already exist are worth showing early; the ones the
        // engine appends arrive with the reparse that follows.
        for (values, row_ref) in block.iter().zip(&rows) {
            for (col_offset, value) in values.iter().enumerate() {
                self.write_cached_cell(*row_ref, AnyColumn(rect.first_col + col_offset), value);
            }
        }
        self.clear_status_message();
        self.apply_buffer_edits(edits, cx);
        cx.notify();
    }

    ///// Rows and columns /////

    /// Inserts as many rows as the selection covers, all at one place: above the
    /// first selected row or below the last one, which is what a spreadsheet
    /// does. Under a sort the new rows land next to the data row the selected
    /// display row stands for, so they appear where the user pointed.
    fn insert_rows(&mut self, above: bool, cx: &mut Context<Self>) {
        let rect = self.selection_rect();
        let count = rect.last_row - rect.first_row + 1;
        let anchor_row = GridRow::from_index(if above { rect.first_row } else { rect.last_row });
        if above && anchor_row == GridRow::Header {
            self.set_status_message("見出し行の上には行を挿入できません", cx);
            return;
        }
        let position = match self.row_ref(anchor_row) {
            Some(row_ref) if above => RowInsertPosition::Before(row_ref),
            Some(row_ref) => RowInsertPosition::After(row_ref),
            None => RowInsertPosition::AtEnd,
        };
        let edits = self
            .with_edit_engine(cx, |engine| engine.insert_rows(position, count))
            .unwrap_or_default();
        self.clear_status_message();
        self.apply_buffer_edits(edits, cx);

        let target = if above {
            rect.first_row
        } else {
            rect.last_row + 1
        };
        self.grid.cursor = GridCell::new(GridRow::from_index(target), self.grid.cursor.col);
        self.grid.anchor = None;
        cx.notify();
    }

    fn delete_selected_rows(&mut self, cx: &mut Context<Self>) {
        let rect = self.selection_rect();
        let rows = self.selected_row_refs();
        if rows.is_empty() {
            return;
        }
        let edits = self
            .with_edit_engine(cx, |engine| engine.delete_rows(&rows))
            .unwrap_or_default();
        self.apply_buffer_edits(edits, cx);

        self.grid.cursor = GridCell::new(
            GridRow::from_index(rect.first_row.max(self.first_row_index())),
            self.grid.cursor.col,
        );
        self.grid.anchor = None;
        self.grid.selection_kind = GridSelectionKind::Cells;
        cx.notify();
    }

    fn insert_columns(&mut self, left: bool, cx: &mut Context<Self>) {
        let rect = self.selection_rect();
        let count = rect.last_col - rect.first_col + 1;
        let index = if left {
            rect.first_col
        } else {
            rect.last_col + 1
        };
        let edits = self
            .with_edit_engine(cx, |engine| engine.insert_columns(index, count))
            .unwrap_or_default();
        self.apply_buffer_edits(edits, cx);

        self.grid.cursor = GridCell::new(self.grid.cursor.row, AnyColumn(index));
        self.grid.anchor = None;
        cx.notify();
    }

    fn delete_selected_columns(&mut self, cx: &mut Context<Self>) {
        let rect = self.selection_rect();
        let cols: Vec<AnyColumn> = rect.cols().map(AnyColumn).collect();
        if cols.is_empty() {
            return;
        }
        let edits = self
            .with_edit_engine(cx, |engine| engine.delete_columns(&cols))
            .unwrap_or_default();
        self.apply_buffer_edits(edits, cx);

        self.grid.cursor = GridCell::new(self.grid.cursor.row, AnyColumn(rect.first_col));
        self.grid.anchor = None;
        self.grid.selection_kind = GridSelectionKind::Cells;
        cx.notify();
    }

    pub(crate) fn toggle_header_row(&mut self, cx: &mut Context<Self>) {
        self.settings.first_row_is_header = !self.settings.first_row_is_header;
        self.reparse_now(cx);
        cx.notify();
    }

    ///// Context menu /////

    pub(crate) fn deploy_grid_context_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dispatch_context = self.grid.cell_editor.focus_handle(cx);
        let menu = ContextMenu::build(window, cx, move |menu, _window, _cx| {
            menu.context(dispatch_context)
                .action("切り取り", Box::new(CutCells))
                .action("コピー", Box::new(CopyCells))
                .action("貼り付け", Box::new(PasteCells))
                .separator()
                .action("上に行を挿入", Box::new(InsertRowAbove))
                .action("下に行を挿入", Box::new(InsertRowBelow))
                .action("行を削除", Box::new(DeleteRows))
                .separator()
                .action("左に列を挿入", Box::new(InsertColumnLeft))
                .action("右に列を挿入", Box::new(InsertColumnRight))
                .action("列を削除", Box::new(DeleteColumns))
                .separator()
                .action("内容を消去", Box::new(ClearCells))
        });

        window.focus(&menu.focus_handle(cx), cx);
        let subscription = cx.subscribe_in(
            &menu,
            window,
            |this, _menu, _event: &DismissEvent, window, cx| {
                this.grid.context_menu.take();
                this.focus_cell_editor(window, cx);
                cx.notify();
            },
        );
        self.grid.context_menu = Some((menu, position, subscription));
        cx.notify();
    }

    ///// Keyboard /////

    pub(crate) fn cell_editor_is_multiline(&self, cx: &App) -> bool {
        matches!(
            self.grid.cell_editor.read(cx).mode(),
            EditorMode::AutoHeight { .. }
        )
    }

    /// Opens the grid's menu from the keyboard, under the caret's cell.
    fn deploy_context_menu_at_caret(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let generation = self.grid.render_generation;
        let position = self
            .grid
            .active_cell_bounds
            .measured(generation)
            .map(|bounds| point(bounds.origin.x, bounds.origin.y + bounds.size.height))
            .or_else(|| {
                self.grid
                    .root_bounds
                    .measured(generation)
                    .map(|bounds| bounds.origin)
            })
            .unwrap_or_else(|| point(px(0.), px(0.)));
        self.deploy_grid_context_menu(position, window, cx);
    }

    /// True while an in place edit should keep a left/right style key for its
    /// own caret instead of leaving the cell.
    fn editing_keeps_caret(&self) -> bool {
        self.grid.editing == Some(CellEditMode::Edit)
    }

    /// Up and down only stay inside a cell that actually has more than one line
    /// to move between.
    fn editing_keeps_vertical_caret(&self, cx: &App) -> bool {
        self.editing_keeps_caret() && self.grid.cell_editor.read(cx).text(cx).contains('\n')
    }

    fn grid_move(
        &mut self,
        motion: GridMotion,
        extend: bool,
        keep_in_cell: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if keep_in_cell {
            cx.propagate();
            return;
        }
        self.commit_editing(window, cx);
        self.move_cursor(motion, extend, window, cx);
        cx.stop_propagation();
    }

    pub(crate) fn register_grid_actions(&self, element: Div, cx: &mut Context<Self>) -> Div {
        element
            .on_action(cx.listener(|this, _: &StartEditing, window, cx| {
                this.start_editing(CellEditMode::Edit, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CommitEditing, window, cx| {
                this.commit_editing(window, cx);
            }))
            .on_action(cx.listener(|this, _: &CommitAndMoveDown, window, cx| {
                this.commit_and_move(GridMotion::Down, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CommitAndMoveUp, window, cx| {
                this.commit_and_move(GridMotion::Up, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CommitAndMoveRight, window, cx| {
                this.commit_and_move(GridMotion::Right, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CommitAndMoveLeft, window, cx| {
                this.commit_and_move(GridMotion::Left, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CancelEditing, window, cx| {
                this.cancel_editing(window, cx);
            }))
            .on_action(cx.listener(|this, _: &InsertLineBreak, window, cx| {
                this.insert_line_break(window, cx);
            }))
            .on_action(cx.listener(|this, _: &InsertRowAbove, _window, cx| {
                this.insert_rows(true, cx);
            }))
            .on_action(cx.listener(|this, _: &InsertRowBelow, _window, cx| {
                this.insert_rows(false, cx);
            }))
            .on_action(cx.listener(|this, _: &DeleteRows, _window, cx| {
                this.delete_selected_rows(cx);
            }))
            .on_action(cx.listener(|this, _: &InsertColumnLeft, _window, cx| {
                this.insert_columns(true, cx);
            }))
            .on_action(cx.listener(|this, _: &InsertColumnRight, _window, cx| {
                this.insert_columns(false, cx);
            }))
            .on_action(cx.listener(|this, _: &DeleteColumns, _window, cx| {
                this.delete_selected_columns(cx);
            }))
            .on_action(cx.listener(|this, _: &InsertRowsOrColumns, _window, cx| {
                match this.grid.selection_kind {
                    GridSelectionKind::Columns => this.insert_columns(true, cx),
                    _ => this.insert_rows(true, cx),
                }
            }))
            .on_action(cx.listener(|this, _: &DeleteRowsOrColumns, _window, cx| {
                match this.grid.selection_kind {
                    GridSelectionKind::Columns => this.delete_selected_columns(cx),
                    _ => this.delete_selected_rows(cx),
                }
            }))
            .on_action(cx.listener(|this, _: &ClearCells, window, cx| {
                this.clear_selected_cells(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectRow, _window, cx| {
                let row = this.grid.cursor.row;
                this.select_row(row, false, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectColumn, _window, cx| {
                let col = this.grid.cursor.col;
                this.select_column(col, false, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectAllCells, _window, cx| {
                this.select_all_cells(cx);
            }))
            .on_action(cx.listener(|this, _: &CopyCells, _window, cx| {
                this.copy_cells(cx);
            }))
            .on_action(cx.listener(|this, _: &CutCells, window, cx| {
                this.cut_cells(window, cx);
            }))
            .on_action(cx.listener(|this, _: &PasteCells, window, cx| {
                this.paste_cells(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleHeaderRow, _window, cx| {
                this.toggle_header_row(cx);
            }))
    }

    /// The editor keeps focus so that input methods work, which means every key
    /// the grid wants arrives as an editor action first and has to be taken
    /// from it before the editor sees it.
    pub(crate) fn capture_editor_actions(&self, element: Div, cx: &mut Context<Self>) -> Div {
        let element = element
            .capture_action(cx.listener(|this, _: &MoveUp, window, cx| {
                let keep = this.editing_keeps_vertical_caret(cx);
                this.grid_move(GridMotion::Up, false, keep, window, cx);
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, window, cx| {
                let keep = this.editing_keeps_vertical_caret(cx);
                this.grid_move(GridMotion::Down, false, keep, window, cx);
            }))
            .capture_action(
                cx.listener(|this, _: &editor::actions::MoveLeft, window, cx| {
                    let keep = this.editing_keeps_caret();
                    this.grid_move(GridMotion::Left, false, keep, window, cx);
                }),
            )
            .capture_action(
                cx.listener(|this, _: &editor::actions::MoveRight, window, cx| {
                    let keep = this.editing_keeps_caret();
                    this.grid_move(GridMotion::Right, false, keep, window, cx);
                }),
            )
            .capture_action(cx.listener(|this, _: &SelectUp, window, cx| {
                let keep = this.editing_keeps_vertical_caret(cx);
                this.grid_move(GridMotion::Up, true, keep, window, cx);
            }))
            .capture_action(cx.listener(|this, _: &SelectDown, window, cx| {
                let keep = this.editing_keeps_vertical_caret(cx);
                this.grid_move(GridMotion::Down, true, keep, window, cx);
            }))
            .capture_action(cx.listener(|this, _: &SelectLeft, window, cx| {
                let keep = this.editing_keeps_caret();
                this.grid_move(GridMotion::Left, true, keep, window, cx);
            }))
            .capture_action(cx.listener(|this, _: &SelectRight, window, cx| {
                let keep = this.editing_keeps_caret();
                this.grid_move(GridMotion::Right, true, keep, window, cx);
            }))
            .capture_action(cx.listener(|this, _: &MoveToBeginningOfLine, window, cx| {
                let keep = this.editing_keeps_caret();
                this.grid_move(GridMotion::RowStart, false, keep, window, cx);
            }))
            .capture_action(cx.listener(|this, _: &MoveToEndOfLine, window, cx| {
                let keep = this.editing_keeps_caret();
                this.grid_move(GridMotion::RowEnd, false, keep, window, cx);
            }))
            .capture_action(
                cx.listener(|this, _: &SelectToBeginningOfLine, window, cx| {
                    let keep = this.editing_keeps_caret();
                    this.grid_move(GridMotion::RowStart, true, keep, window, cx);
                }),
            )
            .capture_action(cx.listener(|this, _: &SelectToEndOfLine, window, cx| {
                let keep = this.editing_keeps_caret();
                this.grid_move(GridMotion::RowEnd, true, keep, window, cx);
            }))
            .capture_action(cx.listener(|this, _: &MoveToBeginning, window, cx| {
                let keep = this.editing_keeps_caret();
                this.grid_move(GridMotion::GridStart, false, keep, window, cx);
            }))
            .capture_action(cx.listener(|this, _: &MoveToEnd, window, cx| {
                let keep = this.editing_keeps_caret();
                this.grid_move(GridMotion::GridEnd, false, keep, window, cx);
            }))
            .capture_action(cx.listener(|this, _: &SelectToBeginning, window, cx| {
                let keep = this.editing_keeps_caret();
                this.grid_move(GridMotion::GridStart, true, keep, window, cx);
            }))
            .capture_action(cx.listener(|this, _: &SelectToEnd, window, cx| {
                let keep = this.editing_keeps_caret();
                this.grid_move(GridMotion::GridEnd, true, keep, window, cx);
            }))
            .capture_action(cx.listener(|this, _: &MovePageUp, window, cx| {
                let keep = this.editing_keeps_vertical_caret(cx);
                this.grid_move(GridMotion::PageUp, false, keep, window, cx);
            }))
            .capture_action(cx.listener(|this, _: &MovePageDown, window, cx| {
                let keep = this.editing_keeps_vertical_caret(cx);
                this.grid_move(GridMotion::PageDown, false, keep, window, cx);
            }))
            .capture_action(cx.listener(|this, _: &SelectPageUp, window, cx| {
                let keep = this.editing_keeps_vertical_caret(cx);
                this.grid_move(GridMotion::PageUp, true, keep, window, cx);
            }))
            .capture_action(cx.listener(|this, _: &SelectPageDown, window, cx| {
                let keep = this.editing_keeps_vertical_caret(cx);
                this.grid_move(GridMotion::PageDown, true, keep, window, cx);
            }))
            .capture_action(cx.listener(|this, _: &LineUp, window, cx| {
                let keep = this.editing_keeps_vertical_caret(cx);
                this.grid_move(GridMotion::ColumnStart, false, keep, window, cx);
            }))
            .capture_action(cx.listener(|this, _: &LineDown, window, cx| {
                let keep = this.editing_keeps_vertical_caret(cx);
                this.grid_move(GridMotion::ColumnEnd, false, keep, window, cx);
            }))
            .capture_action(
                cx.listener(|this, _: &MoveToPreviousWordStart, window, cx| {
                    let keep = this.editing_keeps_caret();
                    this.grid_move(GridMotion::RowStart, false, keep, window, cx);
                }),
            )
            .capture_action(cx.listener(|this, _: &MoveToNextWordEnd, window, cx| {
                let keep = this.editing_keeps_caret();
                this.grid_move(GridMotion::RowEnd, false, keep, window, cx);
            }))
            .capture_action(
                cx.listener(|this, _: &SelectToPreviousWordStart, window, cx| {
                    let keep = this.editing_keeps_caret();
                    this.grid_move(GridMotion::RowStart, true, keep, window, cx);
                }),
            )
            .capture_action(cx.listener(|this, _: &SelectToNextWordEnd, window, cx| {
                let keep = this.editing_keeps_caret();
                this.grid_move(GridMotion::RowEnd, true, keep, window, cx);
            }))
            .capture_action(cx.listener(|this, _: &Tab, window, cx| {
                this.grid_move(GridMotion::Right, false, false, window, cx);
            }))
            .capture_action(cx.listener(|this, _: &Backtab, window, cx| {
                this.grid_move(GridMotion::Left, false, false, window, cx);
            }))
            // A cell that grew a line break is edited in an auto height editor,
            // where `shift-enter` and `ctrl-enter` are bound to `editor::Newline`
            // in a context deeper than the grid's. Both mean "confirm", and
            // shift-enter's direction is up; a line break inside a cell is
            // Alt+Enter only.
            .capture_action(cx.listener(|this, _: &Newline, window, cx| {
                let motion = if this.cell_editor_is_multiline(cx) {
                    GridMotion::Up
                } else {
                    GridMotion::Down
                };
                this.commit_and_move(motion, window, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &Backspace, window, cx| {
                if this.grid.editing.is_some() {
                    cx.propagate();
                    return;
                }
                this.clear_selected_cells(window, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &Delete, window, cx| {
                if this.grid.editing.is_some() {
                    cx.propagate();
                    return;
                }
                this.clear_selected_cells(window, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &Copy, _window, cx| {
                if this.grid.editing.is_some() {
                    cx.propagate();
                    return;
                }
                this.copy_cells(cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &Cut, window, cx| {
                if this.grid.editing.is_some() {
                    cx.propagate();
                    return;
                }
                this.cut_cells(window, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &Paste, window, cx| {
                if this.grid.editing.is_some() {
                    cx.propagate();
                    return;
                }
                this.paste_cells(window, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &Undo, _window, cx| {
                if this.grid.editing.is_some() {
                    cx.propagate();
                    return;
                }
                this.undo_buffer(cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &Redo, _window, cx| {
                if this.grid.editing.is_some() {
                    cx.propagate();
                    return;
                }
                this.redo_buffer(cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &SelectAll, _window, cx| {
                if this.grid.editing.is_some() {
                    cx.propagate();
                    return;
                }
                this.select_all_cells(cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &Cancel, window, cx| {
                if this.cancel_editing(window, cx) {
                    cx.stop_propagation();
                } else {
                    cx.propagate();
                }
            }))
            .capture_action(cx.listener(|this, _: &Rename, window, cx| {
                this.start_editing(CellEditMode::Edit, window, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(
                |this, _: &editor::actions::ShowCompletions, _window, cx| {
                    let col = this.grid.cursor.col;
                    this.select_column(col, false, cx);
                    cx.stop_propagation();
                },
            ))
            .capture_action(cx.listener(|this, _: &OpenContextMenu, window, cx| {
                this.deploy_context_menu_at_caret(window, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &workspace::Save, window, cx| {
                this.commit_editing(window, cx);
                cx.propagate();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _event, _window, _cx| this.end_drag()),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _event, _window, _cx| this.end_drag()),
            );
        Self::swallow_text_actions(element, cx)
    }

    /// Takes every remaining editor action that could rewrite the overlay editor
    /// out of its hands while no cell is open.
    ///
    /// The overlay editor always holds focus so that input methods work, which
    /// means the whole `"Editor"` keymap is live over the grid. Anything that
    /// changes text would silently fill the hidden editor and be committed into
    /// a cell on the next click, so while the grid is not editing the only thing
    /// the editor is allowed to act on is typed input.
    fn swallow_text_actions(element: Div, cx: &mut Context<Self>) -> Div {
        swallow_when_idle!(
            element,
            cx,
            AcceptEditPrediction,
            AcceptNextLineEditPrediction,
            AcceptNextWordEditPrediction,
            AddSelectionAbove,
            AddSelectionBelow,
            AutoIndent,
            ComposeCompletion,
            ConfirmCodeAction,
            ConfirmCompletion,
            ConfirmCompletionReplace,
            ConfirmRename,
            ConvertToKebabCase,
            ConvertToLowerCamelCase,
            ConvertToLowerCase,
            ConvertToOppositeCase,
            ConvertToSnakeCase,
            ConvertToTitleCase,
            ConvertToUpperCamelCase,
            ConvertToUpperCase,
            CutToEndOfLine,
            DeleteLine,
            DeleteToBeginningOfLine,
            DeleteToEndOfLine,
            DeleteToNextSubwordEnd,
            DeleteToNextWordEnd,
            DeleteToPreviousSubwordStart,
            DeleteToPreviousWordStart,
            DuplicateLineDown,
            DuplicateLineUp,
            Format,
            FormatSelections,
            Indent,
            JoinLines,
            KillRingCut,
            KillRingYank,
            MoveLineDown,
            MoveLineUp,
            NewlineAbove,
            NewlineBelow,
            OrganizeImports,
            Outdent,
            Rewrap,
            SelectAllMatches,
            SelectEnclosingSymbol,
            SelectLargerSyntaxNode,
            SelectLine,
            SelectNext,
            SelectPrevious,
            SelectSmallerSyntaxNode,
            ShowCharacterPalette,
            SortLinesCaseInsensitive,
            SortLinesCaseSensitive,
            ToggleBlockComments,
            ToggleComments,
            Transpose,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{grid::GridRow, types::DisplayRow};
    use editor::Editor;
    use fs::FakeFs;
    use gpui::{Entity, KeyBinding, TestAppContext, VisualTestContext};
    use project::Project;
    use serde_json::json;
    use util::path;
    use workspace::{AppState, MultiWorkspace};

    #[test]
    fn test_single_value_fills_the_selection() {
        let block = vec![vec!["x".to_owned()]];
        assert_eq!(
            expand_block_to_selection(block.clone(), 2, 3),
            vec![
                vec!["x".to_owned(), "x".to_owned(), "x".to_owned()],
                vec!["x".to_owned(), "x".to_owned(), "x".to_owned()],
            ]
        );
        assert_eq!(expand_block_to_selection(block.clone(), 1, 1), block);
    }

    #[test]
    fn test_larger_blocks_are_pasted_unchanged() {
        let block = vec![
            vec!["a".to_owned(), "b".to_owned()],
            vec!["c".to_owned(), "d".to_owned()],
        ];
        assert_eq!(expand_block_to_selection(block.clone(), 4, 4), block);
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            editor::init(cx);
            cx.bind_keys([
                KeyBinding::new("down", zed_actions::editor::MoveDown, Some("Editor")),
                KeyBinding::new("up", zed_actions::editor::MoveUp, Some("Editor")),
                KeyBinding::new("right", editor::actions::MoveRight, Some("Editor")),
                KeyBinding::new("left", editor::actions::MoveLeft, Some("Editor")),
                KeyBinding::new("tab", editor::actions::Tab, Some("Editor")),
                KeyBinding::new("shift-tab", editor::actions::Backtab, Some("Editor")),
                KeyBinding::new("escape", editor::actions::Cancel, Some("Editor")),
                KeyBinding::new("enter", CommitAndMoveDown, Some("TabularDataGrid")),
            ]);
        });
    }

    /// Opens `contents` as a CSV preview that is the active item of a real
    /// workspace, so that focus and key dispatch behave as they do in the app.
    async fn open_grid(
        contents: &str,
        cx: &mut TestAppContext,
    ) -> (
        Entity<TabularDataPreviewPane>,
        Entity<Editor>,
        VisualTestContext,
    ) {
        init_test(cx);
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
        cx.run_until_parked();
        (pane, editor, cx)
    }

    fn buffer_text(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> String {
        editor.read_with(cx, |editor, cx| editor.text(cx))
    }

    #[gpui::test]
    async fn test_arrow_keys_walk_the_grid(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("a,b\nc,d\ne,f", cx).await;

        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.grid.cursor.row, GridRow::Header);
            assert_eq!(pane.grid.cursor.col, AnyColumn(0));
        });

        cx.simulate_keystrokes("down right");
        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.grid.cursor.row, GridRow::Body(DisplayRow(0)));
            assert_eq!(pane.grid.cursor.col, AnyColumn(1));
        });

        // The caret stops at the edges of the table instead of wrapping.
        cx.simulate_keystrokes("right right down down down");
        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.grid.cursor.row, GridRow::Body(DisplayRow(1)));
            assert_eq!(pane.grid.cursor.col, AnyColumn(1));
        });

        cx.simulate_keystrokes("up up up up left left");
        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.grid.cursor.row, GridRow::Header);
            assert_eq!(pane.grid.cursor.col, AnyColumn(0));
        });
    }

    #[gpui::test]
    async fn test_typing_replaces_a_cell_and_undo_restores_it(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\nc,d", cx).await;

        cx.simulate_keystrokes("down");
        cx.simulate_input("x");
        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.grid.editing, Some(CellEditMode::Replace));
        });

        cx.simulate_keystrokes("enter");
        assert_eq!(buffer_text(&editor, &mut cx), "a,b\nx,d");
        pane.read_with(&cx, |pane, _| assert_eq!(pane.grid.editing, None));

        cx.dispatch_action(editor::actions::Undo);
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &mut cx), "a,b\nc,d");
    }

    #[gpui::test]
    async fn test_escape_abandons_the_edit(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\nc,d", cx).await;

        cx.simulate_keystrokes("down");
        cx.simulate_input("z");
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        pane.read_with(&cx, |pane, _| assert_eq!(pane.grid.editing, None));
        assert_eq!(buffer_text(&editor, &mut cx), "a,b\nc,d");
    }

    #[gpui::test]
    async fn test_losing_focus_commits_the_edit(cx: &mut TestAppContext) {
        let (_pane, editor, mut cx) = open_grid("a,b\nc,d", cx).await;

        cx.simulate_keystrokes("down");
        cx.simulate_input("y");

        let elsewhere = cx.update(|_window, cx| cx.focus_handle());
        cx.update(|window, cx| window.focus(&elsewhere, cx));
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "a,b\ny,d");
    }

    #[gpui::test]
    async fn test_delete_rows_removes_the_selected_row(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\nc,d\ne,f", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.select_row(GridRow::Body(DisplayRow(0)), false, cx);
        });
        cx.dispatch_action(DeleteRows);
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "a,b\ne,f");
    }

    #[gpui::test]
    async fn test_pasting_a_block_writes_every_cell(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\nc,d\ne,f", cx).await;

        cx.update(|_window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string("1\t2\n3\t4".to_owned()));
        });
        pane.update(&mut cx, |pane, cx| {
            pane.select_cell(
                GridCell::new(GridRow::Body(DisplayRow(0)), AnyColumn(0)),
                false,
                cx,
            );
        });
        cx.dispatch_action(PasteCells);
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "a,b\n1,2\n3,4");
    }

    #[gpui::test]
    async fn test_a_single_clipboard_value_fills_the_selection(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\nc,d\ne,f", cx).await;

        cx.update(|_window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string("z".to_owned()));
        });
        pane.update(&mut cx, |pane, cx| {
            pane.select_cell(
                GridCell::new(GridRow::Body(DisplayRow(0)), AnyColumn(0)),
                false,
                cx,
            );
            pane.select_cell(
                GridCell::new(GridRow::Body(DisplayRow(1)), AnyColumn(1)),
                true,
                cx,
            );
        });
        cx.dispatch_action(PasteCells);
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "a,b\nz,z\nz,z");
    }

    #[gpui::test]
    async fn test_clearing_a_selection_empties_its_cells(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\nc,d", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.select_cell(
                GridCell::new(GridRow::Body(DisplayRow(0)), AnyColumn(0)),
                false,
                cx,
            );
            pane.select_cell(
                GridCell::new(GridRow::Body(DisplayRow(0)), AnyColumn(1)),
                true,
                cx,
            );
        });
        cx.dispatch_action(ClearCells);
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "a,b\n,");
    }

    #[gpui::test]
    async fn test_typing_into_an_empty_file_creates_the_first_row(cx: &mut TestAppContext) {
        let (_pane, editor, mut cx) = open_grid("", cx).await;

        cx.simulate_input("q");
        cx.dispatch_action(CommitEditing);
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "q");
    }

    #[gpui::test]
    async fn test_cutting_a_range_copies_it_and_empties_it(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\nc,d\ne,f", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.select_cell(
                GridCell::new(GridRow::Body(DisplayRow(0)), AnyColumn(0)),
                false,
                cx,
            );
            pane.select_cell(
                GridCell::new(GridRow::Body(DisplayRow(1)), AnyColumn(1)),
                true,
                cx,
            );
        });
        cx.dispatch_action(CutCells);
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "a,b\n,\n,");
        let clipboard = cx.update(|_window, cx| cx.read_from_clipboard());
        assert_eq!(
            clipboard.and_then(|item| item.text()),
            Some("c\td\ne\tf".to_owned())
        );
    }

    #[gpui::test]
    async fn test_redo_restores_an_undone_edit(cx: &mut TestAppContext) {
        let (_pane, editor, mut cx) = open_grid("a,b\nc,d", cx).await;

        cx.simulate_keystrokes("down");
        cx.simulate_input("x");
        cx.simulate_keystrokes("enter");
        assert_eq!(buffer_text(&editor, &mut cx), "a,b\nx,d");

        cx.dispatch_action(editor::actions::Undo);
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &mut cx), "a,b\nc,d");

        cx.dispatch_action(editor::actions::Redo);
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &mut cx), "a,b\nx,d");
    }

    #[gpui::test]
    async fn test_tab_and_backtab_walk_the_row(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("a,b,c\nd,e,f", cx).await;

        cx.simulate_keystrokes("tab tab");
        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.grid.cursor.col, AnyColumn(2));
        });
        cx.simulate_keystrokes("shift-tab");
        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.grid.cursor.col, AnyColumn(1));
        });
    }

    #[gpui::test]
    async fn test_the_header_row_is_editable(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\nc,d", cx).await;

        pane.read_with(&cx, |pane, _| assert!(pane.has_header_row()));
        cx.simulate_input("h");
        cx.dispatch_action(CommitEditing);
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "h,b\nc,d");
    }

    #[gpui::test]
    async fn test_the_first_line_is_data_without_a_header_row(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\nc,d", cx).await;

        cx.dispatch_action(ToggleHeaderRow);
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert!(!pane.has_header_row());
            // Line 1 is now the first body row, and the caret starts on it.
            assert_eq!(pane.body_row_count(), 2);
            assert_eq!(pane.first_row_index(), 1);
        });

        pane.update(&mut cx, |pane, cx| {
            pane.select_cell(
                GridCell::new(GridRow::Body(DisplayRow(0)), AnyColumn(0)),
                false,
                cx,
            );
        });
        cx.simulate_input("z");
        cx.dispatch_action(CommitEditing);
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "z,b\nc,d");
    }

    #[gpui::test]
    async fn test_inserting_and_deleting_columns(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\nc,d", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.select_column(AnyColumn(1), false, cx);
        });
        cx.dispatch_action(InsertColumnLeft);
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &mut cx), "a,,b\nc,,d");

        pane.update(&mut cx, |pane, cx| {
            pane.select_column(AnyColumn(1), false, cx);
        });
        cx.dispatch_action(DeleteColumns);
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &mut cx), "a,b\nc,d");
    }

    ///// Sorting and filtering /////

    fn sort_descending_by_first_column(
        pane: &Entity<TabularDataPreviewPane>,
        cx: &mut VisualTestContext,
    ) {
        use crate::table_data_engine::sorting_by_column::{AppliedSorting, SortDirection};
        pane.update(cx, |pane, cx| {
            pane.engine.applied_sorting = Some(AppliedSorting {
                col_idx: AnyColumn(0),
                direction: SortDirection::Desc,
            });
            pane.apply_sort(cx);
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_committing_writes_the_row_the_user_sees_when_sorted(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("k,v\na,1\nb,2\nc,3", cx).await;
        sort_descending_by_first_column(&pane, &mut cx);

        // Sorted descending the first body row is `c`, the last data row.
        pane.update(&mut cx, |pane, cx| {
            pane.select_cell(
                GridCell::new(GridRow::Body(DisplayRow(0)), AnyColumn(1)),
                false,
                cx,
            );
        });
        cx.simulate_input("9");
        cx.dispatch_action(CommitEditing);
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "k,v\na,1\nb,2\nc,9");
    }

    #[gpui::test]
    async fn test_pasting_writes_the_rows_the_user_sees_when_sorted(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("k,v\na,1\nb,2\nc,3", cx).await;
        sort_descending_by_first_column(&pane, &mut cx);

        cx.update(|_window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string("x\ny".to_owned()));
        });
        pane.update(&mut cx, |pane, cx| {
            pane.select_cell(
                GridCell::new(GridRow::Body(DisplayRow(0)), AnyColumn(1)),
                false,
                cx,
            );
        });
        cx.dispatch_action(PasteCells);
        cx.run_until_parked();

        // Display rows 0 and 1 are `c` and `b`, so the block lands on them and
        // not on the two rows that happen to follow `c` in the file.
        assert_eq!(buffer_text(&editor, &mut cx), "k,v\na,1\nb,y\nc,x");
    }

    #[gpui::test]
    async fn test_deleting_rows_removes_the_rows_the_user_sees_when_sorted(
        cx: &mut TestAppContext,
    ) {
        let (pane, editor, mut cx) = open_grid("k,v\na,1\nb,2\nc,3", cx).await;
        sort_descending_by_first_column(&pane, &mut cx);

        pane.update(&mut cx, |pane, cx| {
            pane.select_row(GridRow::Body(DisplayRow(0)), false, cx);
        });
        cx.dispatch_action(DeleteRows);
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "k,v\na,1\nb,2");
    }

    #[gpui::test]
    async fn test_pasting_into_a_filtered_table_writes_the_visible_rows(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("k,v\na,1\nb,2\na,3", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.toggle_filter(AnyColumn(0), Some("a".into()), cx);
        });
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| assert_eq!(pane.body_row_count(), 2));

        cx.update(|_window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string("x\ny".to_owned()));
        });
        pane.update(&mut cx, |pane, cx| {
            pane.select_cell(
                GridCell::new(GridRow::Body(DisplayRow(0)), AnyColumn(1)),
                false,
                cx,
            );
        });
        cx.dispatch_action(PasteCells);
        cx.run_until_parked();

        // The two visible rows are the first and third data rows; the filtered
        // out row in between keeps its value.
        assert_eq!(buffer_text(&editor, &mut cx), "k,v\na,x\nb,2\na,y");

        // The whole paste is one batch of edits, so one undo takes it all back.
        cx.dispatch_action(editor::actions::Undo);
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &mut cx), "k,v\na,1\nb,2\na,3");
    }

    #[gpui::test]
    async fn test_pasting_past_the_last_row_appends_rows(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("k,v\na,1", cx).await;

        cx.update(|_window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string("x\ty\np\tq".to_owned()));
        });
        pane.update(&mut cx, |pane, cx| {
            pane.select_cell(
                GridCell::new(GridRow::Body(DisplayRow(0)), AnyColumn(0)),
                false,
                cx,
            );
        });
        cx.dispatch_action(PasteCells);
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "k,v\nx,y\np,q");

        // Growing the table is part of the same batch of edits, so a paste that
        // appended a row is still a single undo step.
        cx.dispatch_action(editor::actions::Undo);
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &mut cx), "k,v\na,1");
    }

    #[gpui::test]
    async fn test_copying_and_pasting_a_cell_round_trips_its_quoting(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\n\"he \"\"q\"\"\",d", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.select_cell(
                GridCell::new(GridRow::Body(DisplayRow(0)), AnyColumn(0)),
                false,
                cx,
            );
        });
        cx.dispatch_action(CopyCells);
        cx.run_until_parked();

        pane.update(&mut cx, |pane, cx| {
            pane.select_cell(
                GridCell::new(GridRow::Body(DisplayRow(0)), AnyColumn(1)),
                false,
                cx,
            );
        });
        cx.dispatch_action(PasteCells);
        cx.run_until_parked();

        // The pasted cell holds the value, not the clipboard's quoting of it.
        assert_eq!(
            buffer_text(&editor, &mut cx),
            "a,b\n\"he \"\"q\"\"\",\"he \"\"q\"\"\""
        );
    }

    #[gpui::test]
    async fn test_copying_and_pasting_a_block_round_trips(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\n1,2\n3,4\n,\n,", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.select_cell(
                GridCell::new(GridRow::Body(DisplayRow(0)), AnyColumn(0)),
                false,
                cx,
            );
            pane.select_cell(
                GridCell::new(GridRow::Body(DisplayRow(1)), AnyColumn(1)),
                true,
                cx,
            );
        });
        cx.dispatch_action(CopyCells);
        cx.run_until_parked();

        pane.update(&mut cx, |pane, cx| {
            pane.select_cell(
                GridCell::new(GridRow::Body(DisplayRow(2)), AnyColumn(0)),
                false,
                cx,
            );
        });
        cx.dispatch_action(PasteCells);
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "a,b\n1,2\n3,4\n1,2\n3,4");
    }

    #[gpui::test]
    async fn test_an_oversized_selection_is_refused(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\nc,d", cx).await;

        pane.update_in(&mut cx, |pane, window, cx| {
            // A rectangle the table does not really have, so that the budget is
            // the only thing that can stop the operation.
            pane.grid.cursor = GridCell::new(GridRow::Header, AnyColumn(0));
            pane.grid.anchor = Some(GridCell::new(
                GridRow::Body(DisplayRow(400_000)),
                AnyColumn(0),
            ));
            pane.clear_selected_cells(window, cx);
        });
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "a,b\nc,d");
        pane.read_with(&cx, |pane, _| {
            assert!(pane.grid.status_message.is_some());
        });
    }

    #[gpui::test]
    async fn test_a_row_the_mapping_outran_renders_blank(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("a,b\nc,d\ne,f", cx).await;

        // Shrink the contents behind the mapping's back, the way a reparse does
        // for a moment on a file too big to reparse synchronously.
        pane.update(&mut cx, |pane, _cx| {
            let contents = std::sync::Arc::make_mut(&mut pane.engine.contents);
            contents.rows.truncate(1);
            contents.row_spans.truncate(1);
            contents.line_numbers.truncate(1);
            assert_eq!(pane.body_row_count(), 1);
            assert!(
                pane.row_ref(GridRow::Body(DisplayRow(5))).is_none(),
                "a display row past the contents must not resolve"
            );
        });
        // Drawing the stale frame must not bring the window down.
        cx.simulate_resize(gpui::size(px(800.), px(600.)));
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_a_text_editor_action_cannot_start_an_edit(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid(
            "a,b
c,d", cx,
        )
        .await;
        cx.update(|_window, cx| {
            cx.bind_keys([KeyBinding::new(
                "shift-alt-down",
                editor::actions::DuplicateLineDown,
                Some("Editor"),
            )]);
        });

        cx.simulate_keystrokes("shift-alt-down");
        cx.run_until_parked();
        pane.read_with(&cx, |pane, cx| {
            assert_eq!(pane.grid.editing, None);
            assert!(pane.grid.cell_editor.read(cx).is_empty(cx));
        });

        // A click on another cell would have committed whatever the action left
        // behind; nothing was left behind, so the file is untouched.
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        assert_eq!(
            buffer_text(&editor, &mut cx),
            "a,b
c,d"
        );
    }

    #[test]
    fn test_the_grid_keymap_is_installed() {
        let keymap = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../assets/keymaps/default-windows.json"
        ));
        let block = keymap
            .split(r#""context": "TabularDataGrid""#)
            .nth(1)
            .expect("the default keymap should carry a TabularDataGrid block");
        for (keystroke, action) in [
            ("enter", "tabular_data::CommitAndMoveDown"),
            ("shift-enter", "tabular_data::CommitAndMoveUp"),
            ("alt-enter", "tabular_data::InsertLineBreak"),
            ("f2", "tabular_data::StartEditing"),
        ] {
            let binding = format!(r#""{keystroke}": "{action}""#);
            assert!(
                block.contains(&binding),
                "the TabularDataGrid block should bind {binding}"
            );
        }
    }
}
