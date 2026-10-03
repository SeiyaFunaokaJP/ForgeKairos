# CSV and TSV grid

[日本語](ja/tabular-data.md)

ForgeKairos opens `.csv`, `.tsv`, `.psv` and `.ssv` files as a spreadsheet-like grid
(`tabular_data.open_as_grid`). The file's text stays the source: every grid operation is an
edit of the text, undone in one step, and saving, the unsaved marker, Git diffs and autosave
work as for any text file.

- The grid toolbar opens the text beside the grid; in the text editor, the eye button opens
  the grid. Both stay in sync. Alt+click switches the tab between grid and text in place.
- Cells that contain line breaks are shown in full (`tabular_data.multiline_cells`).
- "First Row Is Header" can be turned off.

## Editing

| Key | Action |
|---|---|
| Enter / Shift+Enter, Tab / Shift+Tab | Move to the next or previous cell |
| typing | Replace the cell's content |
| F2 or double-click | Edit the cell in place |
| Alt+Enter | Line break inside the cell |
| Delete | Clear the content |
| Ctrl+Shift+= / Ctrl+- | Insert / delete rows or columns |
| Shift+Space / Ctrl+Space | Select the row / the column |
| Ctrl+D / Ctrl+R | Fill down / fill right |
| Ctrl+F / Ctrl+H | Search / replace (also with regular expressions and in the selection) |

- Copy uses the tab-separated format that spreadsheets read.
- Input methods (for example Japanese input) work in cells.
- Blank lines are kept as rows, and the existing quoting is preserved.

## Rows and columns

- A **+** row and an **Add Column** column sit at the ends; thin bands on every row and
  column boundary insert there.
- The right-click menus of cells, row headers and column headers insert, delete and clear.
- The fill handle copies a single cell (with Ctrl, a number counts up by 1). From two or
  more cells that form a series of numbers or dates (including weekdays and Japanese date
  formats), it continues the series (with Ctrl, it copies).
- Dragging the border of a selection moves the cells; Ctrl+drag copies them.
- Ctrl+click adds ranges to the selection.

## Filter

The ▾ on each column header (or Alt+Down) filters by a list of values or by text: contains,
does not contain, equals, starts with, ends with, blank, not blank. Up to two conditions
per column can be combined with AND or OR. Filters and sorting are remembered per file.

## Cell colors

Cells, rows, columns or the whole table can be given one of 8 fill and text colors. The
colors are stored by ForgeKairos, not in the file.

## Compare

Compare the grid with the version at Git HEAD, with another file, or between selected cells,
rows or columns. The two sides are shown side by side with cell and character differences
highlighted, and the comparison is updated when either side changes. Cells with several
lines are compared in full.

## Not supported

Formulas, number and date formatting, merged cells, and reordering columns by dragging are
not supported.
