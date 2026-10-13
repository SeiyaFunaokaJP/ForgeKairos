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
use gpui::{
    ClipboardItem, DismissEvent, Div, Focusable as _, MouseButton, MouseUpEvent, Point, point,
};
use ui::{ContextMenu, prelude::*};
use zed_actions::editor::{MoveDown, MoveUp};

use crate::{
    CancelEditing, ClearCellColors, ClearCells, CommitAndMoveDown, CommitAndMoveLeft,
    CommitAndMoveRight, CommitAndMoveUp, CommitEditing, CopyCells, CutCells, DeleteColumns,
    DeleteRows, DeleteRowsOrColumns, InsertColumnLeft, InsertColumnRight, InsertLineBreak,
    InsertRowAbove, InsertRowBelow, InsertRowsOrColumns, PasteCells, SelectAllCells, SelectColumn,
    SelectRow, SetFillColor, SetTextColor, StartEditing, TabularDataPreviewPane, ToggleHeaderRow,
    edit_engine::{
        BufferEdit, CellBlock, EditEngine, RowInsertPosition, RowRef, block_to_clipboard_text,
        clipboard_text_to_block,
    },
    grid::{CellEditMode, GridCell, GridMotion, GridRow, GridSelectionKind},
    grid_fill::{FillDown, FillRight},
    grid_styles::{CellColor, StyleField, StyleShift, extend_with_color_palette},
    renderer::table_diff_view::DiffSelectedCells,
    table_data_engine::{
        ColumnShift,
        sorting_by_column::{AppliedSorting, SortDirection},
    },
    types::AnyColumn,
};
use text::TransactionId;

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

/// What the pointer was on when the context menu was asked for.
///
/// A spreadsheet's row and column headings have menus of their own: one menu
/// offering every row *and* column operation is both long and ambiguous, since
/// "行を削除" means the selected rows whether or not the user clicked a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GridMenuTarget {
    Cell,
    RowHeader,
    /// The column whose heading was clicked, which the sorting entries act on.
    /// A right click inside the selection keeps that selection, so the caret's
    /// column is not necessarily the one under the pointer.
    ColumnHeader(AnyColumn),
}

/// One line of the grid's context menu.
pub(crate) enum GridMenuEntry {
    Separator,
    /// An entry that dispatches an action, which is also how the menu comes to
    /// show that action's keystroke.
    Action {
        label: &'static str,
        action: Box<dyn gpui::Action>,
    },
    /// A palette of cell colors, which is a submenu rather than an entry.
    Colors {
        label: &'static str,
        field: StyleField,
    },
    /// Sorting one column, which has no action to dispatch: sorting is held per
    /// column rather than as a command.
    Sort {
        label: &'static str,
        col: AnyColumn,
        direction: Option<SortDirection>,
    },
    /// Opening one column's filter popover. Not an action either: the popover is
    /// opened through the handle the column's own button registers, so the entry
    /// has to name the column rather than dispatch a command.
    Filter {
        label: &'static str,
        col: AnyColumn,
    },
}

/// What the grid's context menu offers for `target`.
///
/// Kept apart from the menu it builds so that the one thing about it worth
/// pinning down - which operations each target offers, and in which order - can
/// be checked without a window.
pub(crate) fn grid_context_menu_entries(target: GridMenuTarget) -> Vec<GridMenuEntry> {
    let action = |label: &'static str, action: Box<dyn gpui::Action>| GridMenuEntry::Action {
        label,
        action,
    };
    let rows = || {
        vec![
            action(
                i18n::tc!("table grid", "Insert Row Above"),
                Box::new(InsertRowAbove),
            ),
            action(
                i18n::tc!("table grid", "Insert Row Below"),
                Box::new(InsertRowBelow),
            ),
            action(i18n::t!("Delete Rows"), Box::new(DeleteRows)),
        ]
    };
    let columns = || {
        vec![
            action(
                i18n::tc!("table grid", "Insert Column Left"),
                Box::new(InsertColumnLeft),
            ),
            action(
                i18n::tc!("table grid", "Insert Column Right"),
                Box::new(InsertColumnRight),
            ),
            action(i18n::t!("Delete Columns"), Box::new(DeleteColumns)),
        ]
    };
    // A selected row or column is a perfectly good range to copy or clear, so
    // the clipboard is offered on every target rather than only on a cell.
    let clipboard = || {
        vec![
            action(i18n::t!("Cut"), Box::new(CutCells)),
            action(i18n::t!("Copy"), Box::new(CopyCells)),
            action(i18n::t!("Paste"), Box::new(PasteCells)),
            action(i18n::t!("Clear Contents"), Box::new(ClearCells)),
        ]
    };
    let colors = || {
        vec![
            GridMenuEntry::Colors {
                label: i18n::t!("Fill Color"),
                field: StyleField::Fill,
            },
            GridMenuEntry::Colors {
                label: i18n::tc!("cell color", "Text Color"),
                field: StyleField::Text,
            },
            action(i18n::t!("Clear Colors"), Box::new(ClearCellColors)),
        ]
    };
    let sorting = |col: AnyColumn| {
        vec![
            GridMenuEntry::Sort {
                label: i18n::t!("Sort Ascending"),
                col,
                direction: Some(SortDirection::Asc),
            },
            GridMenuEntry::Sort {
                label: i18n::t!("Sort Descending"),
                col,
                direction: Some(SortDirection::Desc),
            },
            GridMenuEntry::Sort {
                label: i18n::t!("Clear Sort"),
                col,
                direction: None,
            },
            GridMenuEntry::Filter {
                label: i18n::t!("Filter…"),
                col,
            },
        ]
    };

    let groups = match target {
        // The cell menu keeps the order it has always had: it is the one the
        // keyboard opens as well, where nothing was clicked.
        GridMenuTarget::Cell => vec![
            vec![
                action(i18n::t!("Cut"), Box::new(CutCells)),
                action(i18n::t!("Copy"), Box::new(CopyCells)),
                action(i18n::t!("Paste"), Box::new(PasteCells)),
            ],
            rows(),
            columns(),
            colors(),
            vec![action(i18n::t!("Clear Contents"), Box::new(ClearCells))],
            vec![action(
                i18n::t!("Compare Selected Cells"),
                Box::new(DiffSelectedCells),
            )],
        ],
        GridMenuTarget::RowHeader => vec![rows(), clipboard(), colors()],
        GridMenuTarget::ColumnHeader(col) => {
            vec![columns(), sorting(col), clipboard(), colors()]
        }
    };

    let mut entries = Vec::new();
    for group in groups {
        if !entries.is_empty() {
            entries.push(GridMenuEntry::Separator);
        }
        entries.extend(group);
    }
    entries
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
    /// Whether the file's format lets the grid write to it, telling the user
    /// where to edit it instead when it does not.
    pub(crate) fn refuse_edit_if_read_only_format(&mut self, cx: &mut Context<Self>) -> bool {
        if self.current_format(cx).is_editable_in_grid() {
            return true;
        }
        self.set_status_message(
            i18n::t!("JSON Lines files can't be edited in the grid. Open the text to edit them"),
            cx,
        );
        false
    }

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
        if !self.refuse_edit_if_read_only_format(cx) {
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
    ///
    /// That editor folds its lines at the width of the cell whatever the soft
    /// wrap setting says, because `calculate_wrap_width` clamps every mode's
    /// answer to the editor's own width, and `max_lines` counts the lines as
    /// they are drawn, folded ones included. A long value therefore stops
    /// growing at the same height as a value of as many quoted lines, and
    /// nothing has to be said here about soft wrap.
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

    pub(crate) fn with_edit_engine<R>(
        &self,
        cx: &App,
        build: impl FnOnce(&EditEngine) -> R,
    ) -> Option<R> {
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
    ///
    /// Reports that transaction, which is what a structural operation records
    /// its color shift against so that undoing the edit takes the colors back.
    pub(crate) fn apply_buffer_edits(
        &mut self,
        edits: Vec<BufferEdit>,
        cx: &mut Context<Self>,
    ) -> Option<TransactionId> {
        if edits.is_empty() || !self.refuse_edit_if_read_only_format(cx) {
            return None;
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
            return None;
        };
        let transaction_id = buffer.update(cx, |buffer, cx| {
            buffer.edit(edits, None, cx);
            buffer
                .finalize_last_transaction()
                .map(|transaction| transaction.id)
        });
        self.reparse_now(cx);
        self.clear_stale_status_message();
        transaction_id
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
        let transaction_id = buffer.update(cx, |buffer, cx| buffer.undo(cx));
        // Ahead of the reparse, which narrows the filters to the width the
        // table had *before* the undo brought a deleted column back.
        if let Some(transaction_id) = transaction_id {
            self.engine.undo_column_shift(transaction_id);
        }
        self.reparse_now(cx);
        if let Some(transaction_id) = transaction_id {
            self.undo_style_shift(transaction_id, cx);
        }
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
        let transaction_id = buffer.update(cx, |buffer, cx| buffer.redo(cx));
        // Ahead of the reparse, as in [`Self::undo_buffer`].
        if let Some(transaction_id) = transaction_id {
            self.engine.redo_column_shift(transaction_id);
        }
        self.reparse_now(cx);
        if let Some(transaction_id) = transaction_id {
            self.redo_style_shift(transaction_id, cx);
        }
        cx.notify();
    }

    ///// Clipboard /////

    /// Copies the selection as a block of tab separated text.
    ///
    /// Blocks of different shapes have no one block of text between them, so a
    /// multiple selection is refused outright, exactly as Excel refuses it.
    /// Excel's one exception - blocks that happen to line up along a shared set
    /// of rows or columns are joined - is deliberately not taken over.
    pub(crate) fn copy_cells(&mut self, cx: &mut Context<Self>) -> bool {
        if self.refuse_on_multiple_ranges(cx) {
            return false;
        }
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

    /// Empties every selected cell. Unlike copying, this has the same meaning
    /// on every block of a multiple selection, so it covers them all - with the
    /// overlap between two blocks counted and written once.
    pub(crate) fn clear_selected_cells(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.within_cell_budget(self.selection_area(), cx) {
            return;
        }
        let cells = self.all_selected_cells();
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
    ///
    /// Only the block the active cell is in counts, because the one place the
    /// rows go has to be one block's edge.
    fn insert_rows(&mut self, above: bool, window: &mut Window, cx: &mut Context<Self>) {
        // A cell left open for input is addressed by the data row it started on,
        // which the rows about to be inserted renumber: committing first puts
        // what was typed in the row the user typed it into.
        self.commit_editing(window, cx);
        let rect = self.selection_rect();
        let count = rect.last_row - rect.first_row + 1;
        let anchor_row = GridRow::from_index(if above { rect.first_row } else { rect.last_row });
        if above && anchor_row == GridRow::Header {
            self.set_status_message(i18n::t!("Rows can't be inserted above the header row"), cx);
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
        // Resolved before the edit, while the physical rows still mean what the
        // stored colors were addressed against.
        let at = self.physical_insert_position(position);
        let transaction_id = self.apply_buffer_edits(edits, cx);
        self.record_style_shift(transaction_id, StyleShift::InsertRows { at, count }, cx);

        let target = if above {
            rect.first_row
        } else {
            rect.last_row + 1
        };
        self.grid.cursor = GridCell::new(GridRow::from_index(target), self.grid.cursor.col);
        self.grid.anchor = None;
        self.grid.previous_ranges.clear();
        cx.notify();
    }

    /// Deletes every row the selection touches, whichever block it is in, as one
    /// undo step. Rows two overlapping blocks share are deleted once.
    fn delete_selected_rows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_editing(window, cx);
        let first_row = self.selection_bounding_rect().first_row;
        let rows = self.all_selected_row_refs();
        if rows.is_empty() {
            return;
        }
        let edits = self
            .with_edit_engine(cx, |engine| engine.delete_rows(&rows))
            .unwrap_or_default();
        let physical_rows = rows.iter().map(|&row| self.physical_row(row)).collect();
        let transaction_id = self.apply_buffer_edits(edits, cx);
        self.record_style_shift(transaction_id, StyleShift::delete_rows(physical_rows), cx);

        self.grid.cursor = GridCell::new(
            GridRow::from_index(first_row.max(self.first_row_index())),
            self.grid.cursor.col,
        );
        self.grid.anchor = None;
        self.grid.previous_ranges.clear();
        self.grid.selection_kind = GridSelectionKind::Cells;
        cx.notify();
    }

    /// Only the block the active cell is in counts, for the same reason as
    /// [`Self::insert_rows`].
    fn insert_columns(&mut self, left: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_editing(window, cx);
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
        // Moved before the edit: the reparse that edit triggers drops the
        // filters of every column the new width does not reach, so one left on
        // its old index would be forgotten there rather than moved.
        let mut shift = ColumnShift::Insert { at: index, count };
        self.engine.apply_column_shift(&mut shift);
        let transaction_id = self.apply_buffer_edits(edits, cx);
        self.engine.record_column_shift(transaction_id, shift);
        self.record_style_shift(
            transaction_id,
            StyleShift::InsertColumns { at: index, count },
            cx,
        );

        self.grid.cursor = GridCell::new(self.grid.cursor.row, AnyColumn(index));
        self.grid.anchor = None;
        self.grid.previous_ranges.clear();
        cx.notify();
    }

    /// Deletes every column the selection touches, whichever block it is in, as
    /// one undo step.
    fn delete_selected_columns(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_editing(window, cx);
        let first_col = self.selection_bounding_rect().first_col;
        let cols = self.all_selected_cols();
        if cols.is_empty() {
            return;
        }
        let edits = self
            .with_edit_engine(cx, |engine| engine.delete_columns(&cols))
            .unwrap_or_default();
        let physical_cols: Vec<usize> = cols.iter().map(|col| col.get()).collect();
        // Before the edit, for the same reason as in [`Self::insert_columns`].
        let mut shift = ColumnShift::delete(physical_cols.clone());
        self.engine.apply_column_shift(&mut shift);
        let transaction_id = self.apply_buffer_edits(edits, cx);
        self.engine.record_column_shift(transaction_id, shift);
        self.record_style_shift(
            transaction_id,
            StyleShift::delete_columns(physical_cols),
            cx,
        );

        self.grid.cursor = GridCell::new(self.grid.cursor.row, AnyColumn(first_col));
        self.grid.anchor = None;
        self.grid.previous_ranges.clear();
        self.grid.selection_kind = GridSelectionKind::Cells;
        cx.notify();
    }

    /// Sorts by `col_idx`, which is the column whose heading menu was opened
    /// rather than the caret's: a right click inside the selection keeps that
    /// selection, so the caret can be sitting in another column entirely.
    fn sort_column(
        &mut self,
        col_idx: AnyColumn,
        direction: Option<SortDirection>,
        cx: &mut Context<Self>,
    ) {
        self.engine.applied_sorting =
            direction.map(|direction| AppliedSorting { col_idx, direction });
        self.apply_sort(cx);
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
        target: GridMenuTarget,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dispatch_context = self.grid.cell_editor.focus_handle(cx);
        // Weak, because the menu this builds is stored back on the grid: a
        // strong handle inside one of its entries would close the loop, and a
        // menu left open when the tab is closed would keep the grid, its buffer
        // and its editor alive for as long as the window.
        let view = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |menu, _window, _cx| {
            let mut menu = menu.context(dispatch_context);
            for entry in grid_context_menu_entries(target) {
                menu = match entry {
                    GridMenuEntry::Separator => menu.separator(),
                    GridMenuEntry::Action { label, action } => menu.action(label, action),
                    GridMenuEntry::Colors { label, field } => {
                        let view = view.clone();
                        menu.submenu(label, move |menu, _window, _cx| {
                            extend_with_color_palette(menu, &view, field)
                        })
                    }
                    GridMenuEntry::Filter { label, col } => {
                        let view = view.clone();
                        menu.entry(label, None, move |window, cx| {
                            view.update(cx, |this, cx| {
                                this.open_filter_menu_for_column(col, window, cx);
                            })
                            .ok();
                        })
                    }
                    GridMenuEntry::Sort {
                        label,
                        col,
                        direction,
                    } => {
                        let view = view.clone();
                        menu.entry(label, None, move |_window, cx| {
                            view.update(cx, |this, cx| {
                                this.sort_column(col, direction, cx);
                            })
                            .ok();
                        })
                    }
                };
            }
            menu
        });

        window.focus(&menu.focus_handle(cx), cx);
        let subscription = cx.subscribe_in(
            &menu,
            window,
            |this, _menu, _event: &DismissEvent, window, cx| {
                this.grid.context_menu.take();
                // An entry of this menu may have opened the column's filter
                // popover on its way out, and that popover closes itself the
                // moment it loses the focus: taking the caret back here would
                // shut it before the user saw it.
                if !this.filter_menu_handle.is_deployed() {
                    this.focus_cell_editor(window, cx);
                }
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
    ///
    /// Nothing was clicked, so what the selection covers stands in for it: a
    /// selected row asks for the row menu just as its heading would.
    fn deploy_context_menu_at_caret(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = match self.grid.selection_kind {
            GridSelectionKind::Rows => GridMenuTarget::RowHeader,
            GridSelectionKind::Columns => GridMenuTarget::ColumnHeader(self.grid.cursor.col),
            GridSelectionKind::Cells | GridSelectionKind::All => GridMenuTarget::Cell,
        };
        let generation = self.grid.render_generation;
        let position = self
            .grid
            .active_cell_bounds
            .painted(generation)
            .map(|bounds| point(bounds.origin.x, bounds.origin.y + bounds.size.height))
            .or_else(|| {
                self.grid
                    .root_bounds
                    .painted(generation)
                    .map(|bounds| bounds.origin)
            })
            .unwrap_or_else(|| point(px(0.), px(0.)));
        self.deploy_grid_context_menu(target, position, window, cx);
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
            .on_action(cx.listener(|this, _: &InsertRowAbove, window, cx| {
                this.insert_rows(true, window, cx);
            }))
            .on_action(cx.listener(|this, _: &InsertRowBelow, window, cx| {
                this.insert_rows(false, window, cx);
            }))
            .on_action(cx.listener(|this, _: &DeleteRows, window, cx| {
                this.delete_selected_rows(window, cx);
            }))
            .on_action(cx.listener(|this, _: &InsertColumnLeft, window, cx| {
                this.insert_columns(true, window, cx);
            }))
            .on_action(cx.listener(|this, _: &InsertColumnRight, window, cx| {
                this.insert_columns(false, window, cx);
            }))
            .on_action(cx.listener(|this, _: &DeleteColumns, window, cx| {
                this.delete_selected_columns(window, cx);
            }))
            .on_action(cx.listener(|this, _: &InsertRowsOrColumns, window, cx| {
                match this.grid.selection_kind {
                    GridSelectionKind::Columns => this.insert_columns(true, window, cx),
                    _ => this.insert_rows(true, window, cx),
                }
            }))
            .on_action(cx.listener(|this, _: &DeleteRowsOrColumns, window, cx| {
                match this.grid.selection_kind {
                    GridSelectionKind::Columns => this.delete_selected_columns(window, cx),
                    _ => this.delete_selected_rows(window, cx),
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
            .on_action(cx.listener(|this, action: &SetFillColor, _window, cx| {
                this.set_selection_color(StyleField::Fill, action.color, cx);
            }))
            .on_action(cx.listener(|this, action: &SetTextColor, _window, cx| {
                this.set_selection_color(StyleField::Text, action.color, cx);
            }))
            .on_action(cx.listener(|this, _: &ClearCellColors, _window, cx| {
                this.set_selection_color(StyleField::Both, CellColor::None, cx);
            }))
            .on_action(cx.listener(|this, _: &FillDown, _window, cx| {
                this.fill_within_selection(true, cx);
            }))
            .on_action(cx.listener(|this, _: &FillRight, _window, cx| {
                this.fill_within_selection(false, cx);
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
                // Escape drops whatever the pointer had in flight as well as an
                // open cell; a fill preview left painted behind it would claim
                // the fill was still armed.
                let dropped_a_drag = this.cancel_drag(cx);
                if this.cancel_editing(window, cx) || dropped_a_drag {
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
            // The modifiers are read here rather than where the drag started,
            // because a fill or a border drag only becomes a copy from the
            // secondary key being held as the button comes up, the way Excel
            // reads it.
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _window, cx| {
                    this.end_drag(event.modifiers, cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _window, cx| {
                    this.end_drag(event.modifiers, cx);
                }),
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
pub(crate) mod tests {
    use super::*;
    use crate::{
        grid::{GridRow, MULTI_RANGE_REFUSAL, ROWS_PER_PAGE_BEFORE_LAYOUT, SelectionChange},
        types::DisplayRow,
    };
    use editor::Editor;
    use fs::FakeFs;
    use gpui::{Entity, KeyBinding, TestAppContext, VisualTestContext, actions, size};
    use project::Project;
    use serde_json::json;
    use std::{cell::Cell, rc::Rc};
    use util::path;
    use workspace::{AppState, MultiWorkspace, Workspace};

    // Stands in for the keymap's own context free bindings, whose actions this
    // test binary does not link and which are therefore skipped outright.
    actions!(tabular_data_test, [ContextFreeBinding]);

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

    /// The keymap the app itself ships on Windows.
    ///
    /// Hand written bindings would only prove that the handlers work when they
    /// are reached; the grid's keys are all shadowed by the `"Editor"` context
    /// of the overlay editor, so only the real file proves they are reached at
    /// all. Actions of crates this test binary does not link (`vim::`, `agent::`
    /// and so on) simply fail to resolve and are skipped.
    ///
    /// `global_bindings` are installed ahead of that keymap, which is where its
    /// own context free bindings sit relative to the grid's.
    fn init_test_with_global_bindings(cx: &mut TestAppContext, global_bindings: Vec<KeyBinding>) {
        cx.update(|cx| {
            AppState::test(cx);
            // Without a per-`App` database every test in the process shares one
            // connection, where the key value rows the grid writes are keyed by
            // a fixture path that all of them use.
            cx.set_global(db::AppDatabase::test_new());
            editor::init(cx);
            cx.bind_keys(global_bindings);
            let bindings =
                settings::KeymapFile::load_asset_cached("keymaps/default-windows.json", cx)
                    .expect("the shipped Windows keymap should load");
            cx.bind_keys(bindings);
        });
    }

    /// Opens `contents` as a CSV preview that is the active item of a real
    /// workspace, so that focus and key dispatch behave as they do in the app.
    pub(crate) async fn open_grid(
        contents: &str,
        cx: &mut TestAppContext,
    ) -> (
        Entity<TabularDataPreviewPane>,
        Entity<Editor>,
        VisualTestContext,
    ) {
        let (_workspace, pane, editor, cx) = open_grid_in_workspace(contents, cx).await;
        (pane, editor, cx)
    }

    /// The same, for a test that has to reach the workspace itself - to close
    /// the grid's tab, say.
    pub(crate) async fn open_grid_in_workspace(
        contents: &str,
        cx: &mut TestAppContext,
    ) -> (
        Entity<Workspace>,
        Entity<TabularDataPreviewPane>,
        Entity<Editor>,
        VisualTestContext,
    ) {
        open_grid_in_workspace_with(contents, Vec::new(), cx).await
    }

    async fn open_grid_in_workspace_with(
        contents: &str,
        global_bindings: Vec<KeyBinding>,
        cx: &mut TestAppContext,
    ) -> (
        Entity<Workspace>,
        Entity<TabularDataPreviewPane>,
        Entity<Editor>,
        VisualTestContext,
    ) {
        init_test_with_global_bindings(cx, global_bindings);
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
        (workspace, pane, editor, cx)
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

    #[gpui::test]
    async fn test_shift_arrows_extend_the_selection(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("a,b,c\nd,e,f\ng,h,i", cx).await;

        cx.simulate_keystrokes("down");
        pane.read_with(&cx, |pane, _| assert_eq!(pane.grid.anchor, None));

        cx.simulate_keystrokes("shift-right shift-down");
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.grid.anchor,
                Some(GridCell::new(GridRow::Body(DisplayRow(0)), AnyColumn(0)))
            );
            assert_eq!(pane.grid.cursor.row, GridRow::Body(DisplayRow(1)));
            assert_eq!(pane.grid.cursor.col, AnyColumn(1));
            let rect = pane.selection_rect();
            assert_eq!((rect.first_row, rect.last_row), (1, 2));
            assert_eq!((rect.first_col, rect.last_col), (0, 1));
        });

        cx.simulate_keystrokes("shift-up shift-left");
        pane.read_with(&cx, |pane, _| {
            let rect = pane.selection_rect();
            assert_eq!((rect.first_row, rect.last_row), (1, 1));
            assert_eq!((rect.first_col, rect.last_col), (0, 0));
        });

        // A plain arrow collapses the range again.
        cx.simulate_keystrokes("down");
        pane.read_with(&cx, |pane, _| assert_eq!(pane.grid.anchor, None));
    }

    #[gpui::test]
    async fn test_insert_and_delete_row_keys_reach_the_grid(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("a,b\nc,d\ne,f", cx).await;

        cx.simulate_keystrokes("down down");
        cx.simulate_keystrokes("ctrl-shift-=");
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &mut cx), "a,b\nc,d\n,\ne,f");
        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.grid.status_message, None);
        });

        cx.simulate_keystrokes("ctrl--");
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &mut cx), "a,b\nc,d\ne,f");
    }

    #[gpui::test]
    async fn test_ctrl_home_brings_the_list_back_to_the_top(cx: &mut TestAppContext) {
        let mut contents = String::from("a,b\n");
        for row in 0..500 {
            contents.push_str(&format!("{row},x\n"));
        }
        let (pane, _editor, mut cx) = open_grid(&contents, cx).await;

        pane.update(&mut cx, |pane, _| {
            pane.list_state.scroll_to(gpui::ListOffset {
                item_ix: 400,
                offset_in_item: px(0.),
            });
        });
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert!(
                pane.list_state.logical_scroll_top().item_ix > 0,
                "the fixture should start scrolled away from the top"
            );
        });

        cx.simulate_keystrokes("ctrl-home");
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.grid.cursor.row, GridRow::Header);
            assert_eq!(
                pane.list_state.logical_scroll_top().item_ix,
                0,
                "the caret went to the header row, so the list has to follow it to the top"
            );
        });
    }

    #[gpui::test]
    async fn test_a_refused_operation_stops_warning_once_the_caret_moves(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("a,b\nc,d", cx).await;

        // The caret starts on the header row, above which nothing can be inserted.
        cx.simulate_keystrokes("ctrl-shift-=");
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.grid.status_message.as_deref(),
                Some("Rows can't be inserted above the header row")
            );
        });

        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| assert_eq!(pane.grid.status_message, None));
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

    /// Ctrl+- and Ctrl+Shift+= are bound with no context at all by the same
    /// keymap, to the buffer font size. A binding without a context matches at
    /// the depth of the innermost element, which is the cell editor holding the
    /// focus, so a binding on `TabularDataGrid` - one of that editor's
    /// ancestors - is out-ranked by it however late in the file it is written.
    #[gpui::test]
    async fn test_the_row_keys_beat_a_context_free_binding(cx: &mut TestAppContext) {
        let (_workspace, pane, editor, mut cx) = open_grid_in_workspace_with(
            "a,b\nc,d\ne,f",
            vec![
                KeyBinding::new("ctrl--", ContextFreeBinding, None),
                KeyBinding::new("ctrl-shift-=", ContextFreeBinding, None),
            ],
            cx,
        )
        .await;

        // A binding whose action nothing handles falls through to the next
        // binding, so the shadowing only counts while something takes it.
        let taken = Rc::new(Cell::new(0usize));
        cx.update(|_window, cx| {
            cx.on_action({
                let taken = taken.clone();
                move |_: &ContextFreeBinding, _cx| taken.set(taken.get() + 1)
            });
        });

        cx.simulate_keystrokes("down down");
        cx.simulate_keystrokes("ctrl-shift-=");
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &mut cx), "a,b\nc,d\n,\ne,f");

        cx.simulate_keystrokes("ctrl--");
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &mut cx), "a,b\nc,d\ne,f");

        assert_eq!(
            taken.get(),
            0,
            "the context free binding must not take the keys off the grid"
        );
        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.grid.status_message, None);
        });
    }

    /// The other side of the same coin: the grid captures a long list of the
    /// text editor's actions and now holds a block of its own on the cell
    /// editor, and a key that belongs to the workspace has to get past both.
    #[gpui::test]
    async fn test_a_key_of_the_workspace_passes_through_the_grid(cx: &mut TestAppContext) {
        let (_workspace, _pane, _editor, mut cx) = open_grid_in_workspace_with(
            "a,b\nc,d",
            vec![KeyBinding::new("ctrl-,", ContextFreeBinding, None)],
            cx,
        )
        .await;

        let taken = Rc::new(Cell::new(0usize));
        cx.update(|_window, cx| {
            cx.on_action({
                let taken = taken.clone();
                move |_: &ContextFreeBinding, _cx| taken.set(taken.get() + 1)
            });
        });

        cx.simulate_keystrokes("ctrl-,");
        cx.run_until_parked();
        assert_eq!(
            taken.get(),
            1,
            "the grid swallowed the key on its way out of the pane"
        );
    }

    /// The menu key opens the grid's menu under the caret, and the only record
    /// of where the caret's cell was drawn is the one the frame that has just
    /// been painted left behind.
    #[gpui::test]
    async fn test_the_menu_key_opens_the_menu_under_the_caret(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("a,b\nc,d\ne,f", cx).await;

        cx.simulate_keystrokes("down down");
        cx.run_until_parked();
        cx.dispatch_action(OpenContextMenu);
        cx.run_until_parked();

        pane.read_with(&cx, |pane, _| {
            let (_menu, position, _subscription) = pane
                .grid
                .context_menu
                .as_ref()
                .expect("the menu key should have opened the grid's menu");
            assert!(
                position.y > px(0.),
                "the menu opened in the window's corner rather than under the caret: {position:?}"
            );
        });
    }

    /// The fixture is big enough that the row jumped to lies far outside every
    /// row the virtualized list has drawn, which is what breaks a scroll worked
    /// out from measured heights.
    async fn open_tall_grid(
        rows: usize,
        cx: &mut TestAppContext,
    ) -> (
        Entity<TabularDataPreviewPane>,
        Entity<Editor>,
        VisualTestContext,
    ) {
        let mut contents = String::from("a,b,c,d\n");
        for row in 0..rows {
            contents.push_str(&format!("{row},x,y,z\n"));
        }
        let (pane, editor, cx) = open_grid(&contents, cx).await;
        // Changing the list's width makes it drop every height it has not
        // measured itself, which is how the app arrives at a list that knows
        // the height of the drawn rows and nothing else.
        cx.simulate_resize(size(px(1600.), px(1053.)));
        cx.run_until_parked();
        (pane, editor, cx)
    }

    /// Whether the list is showing the whole of `row`, which is the only thing
    /// the caret's row has to satisfy: how far the view scrolled to get there is
    /// the list's business.
    pub(crate) fn body_row_is_visible(pane: &TabularDataPreviewPane, row: usize) -> bool {
        let viewport = pane.list_state.viewport_bounds();
        pane.list_state.bounds_for_item(row).is_some_and(|bounds| {
            bounds.top() >= viewport.top() && bounds.bottom() <= viewport.bottom()
        })
    }

    #[gpui::test]
    async fn test_ctrl_end_scrolls_to_the_last_row_of_a_large_file(cx: &mut TestAppContext) {
        const ROWS: usize = 50_000;
        let (pane, _editor, mut cx) = open_tall_grid(ROWS, cx).await;

        cx.simulate_keystrokes("ctrl-end");
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.grid.cursor.row, GridRow::Body(DisplayRow(ROWS - 1)));
            let page = pane.rows_per_page();
            assert!(page > 1, "the body should have measured more than one row");
            assert!(
                body_row_is_visible(pane, ROWS - 1),
                "the caret's row is the last one, and it has to be on screen; \
                 the view sits at {:?}",
                pane.list_state.logical_scroll_top()
            );
            assert!(
                !body_row_is_visible(pane, 0),
                "the view never left the top of the file"
            );
        });

        cx.simulate_keystrokes("ctrl-home");
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.grid.cursor.row, GridRow::Header);
            assert_eq!(pane.list_state.logical_scroll_top().item_ix, 0);
        });
    }

    #[gpui::test]
    async fn test_a_page_key_covers_the_rows_the_body_shows(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_tall_grid(500, cx).await;

        let page = pane.read_with(&cx, |pane, _| pane.rows_per_page());
        assert!(
            page > ROWS_PER_PAGE_BEFORE_LAYOUT,
            "the fixture window should show more rows than the unmeasured default, \
             or this proves nothing; it showed {page}"
        );
        // A page worked out from the height of a line of text rather than of a
        // drawn row counts rows that are not on screen, and the caret then lands
        // below the last one the user can see.
        pane.read_with(&cx, |pane, _| {
            assert!(
                body_row_is_visible(pane, page - 1),
                "a page is {page} rows, and the last of them is not on screen"
            );
            assert!(
                !body_row_is_visible(pane, page),
                "a page is {page} rows, and one past them is still on screen"
            );
        });

        cx.simulate_keystrokes("down");
        cx.simulate_keystrokes("pagedown");
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.grid.cursor.row, GridRow::Body(DisplayRow(page)));
            assert!(
                body_row_is_visible(pane, page),
                "the caret went a page down and has to have come with the view"
            );
        });

        cx.simulate_keystrokes("pageup");
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.grid.cursor.row, GridRow::Body(DisplayRow(0)));
            assert!(body_row_is_visible(pane, 0));
        });
    }

    /// Walking off the bottom of the view with an arrow key scrolls a row at a
    /// time, rather than the page a far away jump moves.
    #[gpui::test]
    async fn test_walking_off_the_bottom_scrolls_one_row(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_tall_grid(500, cx).await;
        let page = pane.read_with(&cx, |pane, _| pane.rows_per_page());

        // The caret starts on the header row, so this leaves it on the last body
        // row the view shows.
        for _ in 0..page {
            cx.simulate_keystrokes("down");
        }
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.grid.cursor.row, GridRow::Body(DisplayRow(page - 1)));
            assert_eq!(
                pane.list_state.logical_scroll_top().item_ix,
                0,
                "the caret reached the bottom of the view without it scrolling"
            );
        });

        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.list_state.logical_scroll_top().item_ix,
                1,
                "one row past the bottom is one row of scrolling"
            );
            assert!(body_row_is_visible(pane, page));
        });
    }

    ///// Multiple selections /////

    fn body_cell(row: usize, col: usize) -> GridCell {
        GridCell::new(GridRow::Body(DisplayRow(row)), AnyColumn(col))
    }

    /// Selects `blocks`, the first one on its own and every later one added
    /// beside it, which is what Ctrl+dragging over each of them does.
    fn select_blocks(
        pane: &Entity<TabularDataPreviewPane>,
        blocks: &[(GridCell, GridCell)],
        cx: &mut VisualTestContext,
    ) {
        pane.update(cx, |pane, cx| {
            for (index, (from, to)) in blocks.iter().enumerate() {
                let start = if index == 0 {
                    SelectionChange::Replace
                } else {
                    SelectionChange::Add
                };
                pane.change_cell_selection(*from, start, cx);
                pane.change_cell_selection(*to, SelectionChange::Extend, cx);
            }
        });
    }

    #[gpui::test]
    async fn test_copying_a_multiple_selection_is_refused(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid(
            "a,b,c
1,2,3
4,5,6",
            cx,
        )
        .await;

        cx.update(|_window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string("untouched".to_owned()));
        });
        select_blocks(
            &pane,
            &[
                (body_cell(0, 0), body_cell(0, 0)),
                (body_cell(1, 2), body_cell(1, 2)),
            ],
            &mut cx,
        );
        cx.dispatch_action(CopyCells);
        cx.run_until_parked();

        let clipboard = cx.update(|_window, cx| cx.read_from_clipboard());
        assert_eq!(
            clipboard.and_then(|item| item.text()),
            Some("untouched".to_owned()),
            "two blocks have no one block of text to put on the clipboard"
        );
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.grid.status_message.as_deref(),
                Some(MULTI_RANGE_REFUSAL)
            );
        });
    }

    /// Cutting copies first, so the refusal has to stop it before it empties
    /// anything.
    #[gpui::test]
    async fn test_cutting_a_multiple_selection_changes_nothing(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid(
            "a,b,c
1,2,3
4,5,6",
            cx,
        )
        .await;

        select_blocks(
            &pane,
            &[
                (body_cell(0, 0), body_cell(0, 0)),
                (body_cell(1, 2), body_cell(1, 2)),
            ],
            &mut cx,
        );
        cx.dispatch_action(CutCells);
        cx.run_until_parked();

        assert_eq!(
            buffer_text(&editor, &mut cx),
            "a,b,c
1,2,3
4,5,6"
        );
    }

    #[gpui::test]
    async fn test_clearing_empties_every_selected_block(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid(
            "a,b,c
1,2,3
4,5,6
7,8,9",
            cx,
        )
        .await;

        select_blocks(
            &pane,
            &[
                (body_cell(0, 0), body_cell(0, 1)),
                (body_cell(2, 2), body_cell(2, 2)),
            ],
            &mut cx,
        );
        cx.dispatch_action(ClearCells);
        cx.run_until_parked();

        assert_eq!(
            buffer_text(&editor, &mut cx),
            "a,b,c
,,3
4,5,6
7,8,"
        );

        // Both blocks went in as one batch of edits, so one undo takes both back.
        cx.dispatch_action(editor::actions::Undo);
        cx.run_until_parked();
        assert_eq!(
            buffer_text(&editor, &mut cx),
            "a,b,c
1,2,3
4,5,6
7,8,9"
        );
    }

    /// The cells two blocks share are written once, so the whole operation is
    /// still a single undo step and still counts once against the budget.
    #[gpui::test]
    async fn test_clearing_overlapping_blocks_is_one_undo_step(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid(
            "a,b,c
1,2,3
4,5,6
7,8,9",
            cx,
        )
        .await;

        select_blocks(
            &pane,
            &[
                (body_cell(0, 0), body_cell(1, 1)),
                (body_cell(1, 1), body_cell(2, 2)),
            ],
            &mut cx,
        );
        cx.dispatch_action(ClearCells);
        cx.run_until_parked();
        assert_eq!(
            buffer_text(&editor, &mut cx),
            "a,b,c
,,3
,,
7,,"
        );

        cx.dispatch_action(editor::actions::Undo);
        cx.run_until_parked();
        assert_eq!(
            buffer_text(&editor, &mut cx),
            "a,b,c
1,2,3
4,5,6
7,8,9"
        );
    }

    #[gpui::test]
    async fn test_deleting_rows_takes_every_selected_block(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid(
            "h
a
b
c
d
e",
            cx,
        )
        .await;

        select_blocks(
            &pane,
            &[
                (body_cell(0, 0), body_cell(0, 0)),
                (body_cell(2, 0), body_cell(3, 0)),
            ],
            &mut cx,
        );
        cx.dispatch_action(DeleteRows);
        cx.run_until_parked();

        assert_eq!(
            buffer_text(&editor, &mut cx),
            "h
b
e"
        );
        pane.read_with(&cx, |pane, _| {
            assert!(!pane.has_multiple_selection_ranges());
        });

        cx.dispatch_action(editor::actions::Undo);
        cx.run_until_parked();
        assert_eq!(
            buffer_text(&editor, &mut cx),
            "h
a
b
c
d
e"
        );
    }

    #[gpui::test]
    async fn test_deleting_columns_takes_every_selected_block(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid(
            "a,b,c,d,e
1,2,3,4,5",
            cx,
        )
        .await;

        select_blocks(
            &pane,
            &[
                (body_cell(0, 0), body_cell(0, 0)),
                (body_cell(0, 3), body_cell(0, 4)),
            ],
            &mut cx,
        );
        cx.dispatch_action(DeleteColumns);
        cx.run_until_parked();

        assert_eq!(
            buffer_text(&editor, &mut cx),
            "b,c
2,3"
        );

        cx.dispatch_action(editor::actions::Undo);
        cx.run_until_parked();
        assert_eq!(
            buffer_text(&editor, &mut cx),
            "a,b,c,d,e
1,2,3,4,5"
        );
    }

    /// Pasting has a corner to start from rather than a shape to cover, so it
    /// goes into the block the active cell is in and leaves the rest alone.
    #[gpui::test]
    async fn test_pasting_lands_in_the_block_of_the_active_cell(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid(
            "a,b,c
1,2,3
4,5,6",
            cx,
        )
        .await;

        cx.update(|_window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string("z".to_owned()));
        });
        select_blocks(
            &pane,
            &[
                (body_cell(0, 0), body_cell(0, 0)),
                (body_cell(1, 2), body_cell(1, 2)),
            ],
            &mut cx,
        );
        cx.dispatch_action(PasteCells);
        cx.run_until_parked();

        assert_eq!(
            buffer_text(&editor, &mut cx),
            "a,b,c
1,2,3
4,5,z"
        );
    }

    /// Inserting rows needs one edge to insert at, so it takes the block the
    /// active cell is in - and the count of rows from that block alone.
    #[gpui::test]
    async fn test_inserting_rows_uses_the_block_of_the_active_cell(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid(
            "h
a
b
c", cx,
        )
        .await;

        select_blocks(
            &pane,
            &[
                (body_cell(0, 0), body_cell(0, 0)),
                (body_cell(2, 0), body_cell(2, 0)),
            ],
            &mut cx,
        );
        cx.dispatch_action(InsertRowAbove);
        cx.run_until_parked();

        assert_eq!(
            buffer_text(&editor, &mut cx),
            "h
a
b

c"
        );
    }

    ///// The structural actions and an open cell /////

    /// The toolbar's and the keyboard's "insert a row" address the cell left
    /// open for input by the data row it was opened on, and inserting hands that
    /// index to the blank row: what was typed has to be written out first, or it
    /// lands one row down from where the user typed it.
    #[gpui::test]
    async fn test_the_insert_row_action_writes_out_the_open_cell_first(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("h,n\na,1\nb,2", cx).await;

        cx.simulate_keystrokes("down down");
        cx.simulate_input("zzz");
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| assert!(pane.grid.editing.is_some()));

        cx.dispatch_action(InsertRowAbove);
        cx.run_until_parked();

        // The caret was on the second body row, so the blank row goes above that
        // row and what was typed stays with the row it was typed into.
        assert_eq!(buffer_text(&editor, &mut cx), "h,n\na,1\n,\nzzz,2");
    }

    /// The same for deleting: the open cell's row is about to stop existing, and
    /// every data row under it takes a new index.
    #[gpui::test]
    async fn test_the_delete_rows_action_writes_out_the_open_cell_first(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("h,n\na,1\nb,2\nc,3", cx).await;

        cx.simulate_keystrokes("down");
        cx.simulate_input("zzz");
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| assert!(pane.grid.editing.is_some()));

        cx.dispatch_action(DeleteRows);
        cx.run_until_parked();

        // What was typed went into the row it was typed in, and that row is the
        // one the delete then took.
        assert_eq!(buffer_text(&editor, &mut cx), "h,n\nb,2\nc,3");

        cx.dispatch_action(editor::actions::Undo);
        cx.run_until_parked();
        assert_eq!(buffer_text(&editor, &mut cx), "h,n\nzzz,1\nb,2\nc,3");
    }

    #[gpui::test]
    async fn test_the_insert_column_action_writes_out_the_open_cell_first(cx: &mut TestAppContext) {
        let (pane, editor, mut cx) = open_grid("h,n\na,1", cx).await;

        cx.simulate_keystrokes("down right");
        cx.simulate_input("zzz");
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| assert!(pane.grid.editing.is_some()));

        cx.dispatch_action(InsertColumnLeft);
        cx.run_until_parked();

        assert_eq!(buffer_text(&editor, &mut cx), "h,,n\na,,zzz");
    }

    ///// Right clicking a heading /////

    /// A cell range that happens to cross a row says nothing about that row, so
    /// the heading's menu - whose row entries act on the selection - gets the row
    /// that was actually clicked.
    #[gpui::test]
    async fn test_right_clicking_a_row_outside_a_row_selection_takes_that_row(
        cx: &mut TestAppContext,
    ) {
        let (pane, _editor, mut cx) = open_grid("h\na\nb\nc", cx).await;

        select_blocks(&pane, &[(body_cell(0, 0), body_cell(2, 0))], &mut cx);
        pane.update_in(&mut cx, |pane, window, cx| {
            pane.on_row_mouse_down(
                GridRow::Body(DisplayRow(1)),
                MouseButton::Right,
                gpui::Modifiers::none(),
                point(px(0.), px(0.)),
                window,
                cx,
            );
        });

        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.grid.selection_kind, GridSelectionKind::Rows);
            assert_eq!(pane.selected_rows(), vec![GridRow::Body(DisplayRow(1))]);
        });
    }

    /// Rows picked as rows are meant to be acted on together, so a right click
    /// inside them keeps them.
    #[gpui::test]
    async fn test_right_clicking_inside_a_row_selection_keeps_it(cx: &mut TestAppContext) {
        let (pane, _editor, mut cx) = open_grid("h\na\nb\nc", cx).await;

        pane.update(&mut cx, |pane, cx| {
            pane.change_row_selection(GridRow::Body(DisplayRow(0)), SelectionChange::Replace, cx);
            pane.change_row_selection(GridRow::Body(DisplayRow(2)), SelectionChange::Extend, cx);
        });
        pane.update_in(&mut cx, |pane, window, cx| {
            pane.on_row_mouse_down(
                GridRow::Body(DisplayRow(1)),
                MouseButton::Right,
                gpui::Modifiers::none(),
                point(px(0.), px(0.)),
                window,
                cx,
            );
        });

        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.selected_rows(),
                vec![
                    GridRow::Body(DisplayRow(0)),
                    GridRow::Body(DisplayRow(1)),
                    GridRow::Body(DisplayRow(2)),
                ]
            );
        });
    }

    /// The same for a column heading, and for a column added with Ctrl: a block
    /// of columns picked as columns survives a right click inside it.
    #[gpui::test]
    async fn test_right_clicking_a_column_outside_a_column_selection_takes_it(
        cx: &mut TestAppContext,
    ) {
        let (pane, _editor, mut cx) = open_grid("a,b,c\n1,2,3", cx).await;

        select_blocks(&pane, &[(body_cell(0, 0), body_cell(0, 2))], &mut cx);
        pane.update_in(&mut cx, |pane, window, cx| {
            pane.on_column_mouse_down(
                AnyColumn(1),
                MouseButton::Right,
                gpui::Modifiers::none(),
                point(px(0.), px(0.)),
                window,
                cx,
            );
        });

        pane.read_with(&cx, |pane, _| {
            assert_eq!(pane.grid.selection_kind, GridSelectionKind::Columns);
            assert_eq!(pane.all_selected_cols(), vec![AnyColumn(1)]);
        });

        pane.update(&mut cx, |pane, cx| {
            pane.change_column_selection(AnyColumn(0), SelectionChange::Replace, cx);
            pane.change_column_selection(AnyColumn(2), SelectionChange::Add, cx);
        });
        pane.update_in(&mut cx, |pane, window, cx| {
            pane.on_column_mouse_down(
                AnyColumn(2),
                MouseButton::Right,
                gpui::Modifiers::none(),
                point(px(0.), px(0.)),
                window,
                cx,
            );
        });
        pane.read_with(&cx, |pane, _| {
            assert_eq!(
                pane.all_selected_cols(),
                vec![AnyColumn(0), AnyColumn(2)],
                "a column added with Ctrl is still a column selection"
            );
        });
    }

    /// The menu a right click builds holds the grid, and the grid holds the
    /// menu: a strong handle in one of its entries keeps the whole pane, its
    /// buffer and its editor alive for as long as the window.
    #[gpui::test]
    async fn test_closing_the_grid_releases_it_despite_an_open_context_menu(
        cx: &mut TestAppContext,
    ) {
        use workspace::SaveIntent;

        let (workspace, pane, _editor, mut cx) = open_grid_in_workspace("a,b\nc,d", cx).await;
        cx.run_until_parked();

        pane.update_in(&mut cx, |pane, window, cx| {
            pane.deploy_grid_context_menu(
                GridMenuTarget::ColumnHeader(AnyColumn(0)),
                point(px(0.), px(0.)),
                window,
                cx,
            );
        });
        cx.run_until_parked();
        pane.read_with(&cx, |pane, _| {
            assert!(
                pane.grid.context_menu.is_some(),
                "the menu has to be open, or nothing holds the grid"
            );
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

    /// The two entries the four parts of this feature only earn together: the
    /// column heading is where a user who never found the arrow looks for
    /// filtering, and the cell menu is where a selection is compared.
    #[test]
    fn test_the_menus_offer_filtering_and_comparing() {
        let labels = |target| {
            grid_context_menu_entries(target)
                .into_iter()
                .filter_map(|entry| match entry {
                    GridMenuEntry::Separator => None,
                    GridMenuEntry::Action { label, .. }
                    | GridMenuEntry::Colors { label, .. }
                    | GridMenuEntry::Sort { label, .. }
                    | GridMenuEntry::Filter { label, .. } => Some(label),
                })
                .collect::<Vec<_>>()
        };

        let columns = labels(GridMenuTarget::ColumnHeader(AnyColumn(1)));
        let sorting = columns
            .iter()
            .position(|label| *label == "Clear Sort")
            .expect("the column menu should offer sorting");
        let filtering = columns
            .iter()
            .position(|label| *label == "Filter…")
            .expect("the column menu should offer filtering");
        assert_eq!(
            filtering,
            sorting + 1,
            "filtering belongs with sorting, after it: {columns:?}"
        );
        assert!(
            !labels(GridMenuTarget::RowHeader).contains(&"Filter…"),
            "a row heading has no column to filter"
        );

        let cell = labels(GridMenuTarget::Cell);
        assert_eq!(
            cell.last(),
            Some(&"Compare Selected Cells"),
            "comparing is the cell menu's last entry: {cell:?}"
        );
    }

    /// The column the filter entry carries is the one that was clicked, the way
    /// the sorting entries already work.
    #[test]
    fn test_the_filter_entry_carries_the_clicked_column() {
        let filter_columns = |target| {
            grid_context_menu_entries(target)
                .into_iter()
                .filter_map(|entry| match entry {
                    GridMenuEntry::Filter { col, .. } => Some(col),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            filter_columns(GridMenuTarget::ColumnHeader(AnyColumn(3))),
            vec![AnyColumn(3)]
        );
        assert!(filter_columns(GridMenuTarget::Cell).is_empty());
    }
}
