use ui::table_row::TableRow;

use crate::types::{CellContentSpan, DataRow, LineNumber, TableCell};

/// Generic container struct of table-like data (CSV, TSV, etc)
#[derive(Clone)]
pub struct TableLikeContent {
    /// Number of data columns.
    /// Defines table width used to validate `TableRow` on creation
    pub number_of_cols: usize,
    pub headers: TableRow<TableCell>,
    pub rows: Vec<TableRow<TableCell>>,
    /// Follows the same indices as `rows`
    pub line_numbers: Vec<LineNumber>,
    /// Buffer span of every data row, from the first character of the row up to the last one
    /// before its line terminator. A row holding a quoted newline spans all of its lines.
    /// Follows the same indices as `rows`
    pub row_spans: Vec<CellContentSpan>,
    /// Buffer span of the header row, `None` when the first line is parsed as ordinary data
    pub header_span: Option<CellContentSpan>,
    /// Whether the first line of the buffer was parsed as the header row
    pub first_row_is_header: bool,
    /// Buffer version these contents were parsed from. Lets the debounced reparse skip work that
    /// a synchronous reparse (triggered by a grid edit) has already done.
    pub source_version: Option<clock::Global>,
    /// Size of the buffer these contents were parsed from. Everything derived from the contents
    /// uses the same size threshold, so contents and derived state never update a frame apart.
    pub source_byte_len: usize,
}

impl Default for TableLikeContent {
    fn default() -> Self {
        Self {
            number_of_cols: 0,
            headers: TableRow::<TableCell>::from_vec(vec![], 0),
            rows: vec![],
            line_numbers: vec![],
            row_spans: vec![],
            header_span: None,
            first_row_is_header: true,
            source_version: None,
            source_byte_len: 0,
        }
    }
}

impl TableLikeContent {
    pub(crate) fn get_row(&self, data_row: DataRow) -> Option<&TableRow<TableCell>> {
        self.rows.get(*data_row)
    }
}
