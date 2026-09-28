use std::ops::Range;
use std::sync::Arc;

use crate::{
    TabularDataPreviewPane,
    types::TableLikeContent,
    types::{CellContentSpan, LineNumber, TableCell},
};
use editor::Editor;
use gpui::{App, AppContext, Context, Entity, Subscription, Task};
use std::path::Path;
use std::time::{Duration, Instant};
use text::BufferSnapshot;
use ui::{SharedString, table_row::TableRow};

pub(crate) const REPARSE_DEBOUNCE: Duration = Duration::from_millis(200);

/// Buffers larger than this are reparsed in the background even when the grid asks for a
/// synchronous reparse, so a single cell edit can never block the UI thread on a huge file. The
/// display mapping follows the same threshold, so contents and mapping always update together.
pub(crate) const MAX_SYNC_PARSE_BYTES: usize = 256 * 1024;

pub(crate) struct EditorState {
    pub editor: Entity<Editor>,
    pub _subscription: Subscription,
}

#[derive(Clone, Copy)]
pub(crate) enum TabularFormat {
    Csv,
    Tsv,
    Psv,
    Ssv,
}

const TABULAR_FORMATS: &[(&str, TabularFormat)] = &[
    ("csv", TabularFormat::Csv),
    ("tsv", TabularFormat::Tsv),
    ("psv", TabularFormat::Psv),
    ("ssv", TabularFormat::Ssv),
];

impl TabularFormat {
    pub(crate) fn from_editor(editor: &Entity<Editor>, cx: &App) -> Option<Self> {
        Self::from_extension(editor_file_extension(editor, cx)?)
    }

    pub(crate) fn from_extension(ext: &str) -> Option<Self> {
        TABULAR_FORMATS
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(ext))
            .map(|(_, format)| *format)
    }

    pub(crate) fn delimiter(self) -> char {
        match self {
            TabularFormat::Csv => ',',
            TabularFormat::Tsv => '\t',
            TabularFormat::Psv => '|',
            TabularFormat::Ssv => ';',
        }
    }
}

pub(crate) fn editor_file_extension<'a>(editor: &Entity<Editor>, cx: &'a App) -> Option<&'a str> {
    let buffer = editor.read(cx).buffer().read(cx).as_singleton()?;
    let file = buffer.read(cx).file()?;
    Path::new(file.file_name(cx)).extension()?.to_str()
}

impl TabularDataPreviewPane {
    /// Delimiter of the previewed file, derived from its extension.
    pub(crate) fn current_delimiter(&self, cx: &App) -> char {
        let extension = editor_file_extension(&self.active_editor_state.editor, cx);

        extension
            .and_then(TabularFormat::from_extension)
            .map(TabularFormat::delimiter)
            .unwrap_or_else(|| {
                log::warn!(
                    "unrecognized tabular data extension {extension:?}, defaulting to comma delimiter"
                );
                ','
            })
    }

    pub(crate) fn parse_from_active_editor(
        &mut self,
        wait_for_debounce: bool,
        cx: &mut Context<Self>,
    ) {
        // A grid edit reparses synchronously and then sees the editor's own `Edited` event arrive.
        // Without this check that event would raise `is_parsing` for the whole debounce window and
        // the table would blink into its loading state after every keystroke.
        if self.contents_match_buffer(cx) {
            return;
        }
        let editor = self.active_editor_state.editor.clone();
        self.is_parsing = true;
        self.parsing_task = Some(self.parse_in_background(wait_for_debounce, editor, cx));
    }

    /// Whether `engine.contents` already describes the buffer exactly as it is now.
    fn contents_match_buffer(&self, cx: &App) -> bool {
        let Some(buffer) = self
            .active_editor_state
            .editor
            .read(cx)
            .buffer()
            .read(cx)
            .as_singleton()
        else {
            return false;
        };
        let contents = &self.engine.contents;
        contents.first_row_is_header == self.settings.first_row_is_header
            && contents.source_version.as_ref() == Some(&buffer.read(cx).version())
    }

    /// Reparses the buffer on the foreground thread and refreshes everything derived from it.
    /// The grid calls this right after committing an edit so the rendered table never lags a tick
    /// behind the buffer. Oversized buffers fall back to the background parse.
    pub(crate) fn reparse_now(&mut self, cx: &mut Context<Self>) {
        if self.contents_match_buffer(cx) {
            return;
        }
        let Some(buffer) = self
            .active_editor_state
            .editor
            .read(cx)
            .buffer()
            .read(cx)
            .as_singleton()
        else {
            return;
        };
        let buffer_snapshot = buffer.read(cx).text_snapshot();

        if buffer_snapshot.len() > MAX_SYNC_PARSE_BYTES {
            self.parse_from_active_editor(false, cx);
            return;
        }

        let delimiter = self.current_delimiter(cx);
        let first_row_is_header = self.settings.first_row_is_header;
        let instant = Instant::now();
        let parsed_contents =
            from_buffer_with_delimiter(&buffer_snapshot, delimiter, first_row_is_header);
        let parse_duration = instant.elapsed();

        // Dropping the pending task cancels a debounced parse that would redo this work.
        self.parsing_task = None;
        self.performance_metrics
            .timings
            .insert("Parsing", (parse_duration, Instant::now()));
        self.engine.contents = Arc::new(parsed_contents);
        self.engine.calculate_available_filters();
        self.sync_column_widths(cx);
        self.last_parse_end_time = Some(Instant::now());
        self.is_parsing = false;
        self.apply_filter_sort(cx);
        cx.notify();
    }

    fn parse_in_background(
        &mut self,
        wait_for_debounce: bool,
        editor: Entity<Editor>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        cx.spawn(async move |view, cx| {
            if wait_for_debounce {
                // Smart debouncing: check if cooldown period has already passed
                let now = Instant::now();
                let should_wait = view.update(cx, |view, _| {
                    if let Some(last_end) = view.last_parse_end_time {
                        let cooldown_until = last_end + REPARSE_DEBOUNCE;
                        if now < cooldown_until {
                            Some(cooldown_until - now)
                        } else {
                            None // Cooldown already passed, parse immediately
                        }
                    } else {
                        None // First parse, no debounce
                    }
                })?;

                if let Some(wait_duration) = should_wait {
                    cx.background_executor().timer(wait_duration).await;
                }
            }

            let (buffer_snapshot, delimiter, first_row_is_header, parsed_version, parsed_as_header) =
                view.update(cx, |view, cx| {
                    let buffer_snapshot = editor
                        .read(cx)
                        .buffer()
                        .read(cx)
                        .as_singleton()
                        .map(|b| b.read(cx).text_snapshot());

                    (
                        buffer_snapshot,
                        view.current_delimiter(cx),
                        view.settings.first_row_is_header,
                        view.engine.contents.source_version.clone(),
                        view.engine.contents.first_row_is_header,
                    )
                })?;

            let Some(buffer_snapshot) = buffer_snapshot else {
                return Ok(());
            };

            // A synchronous reparse from the grid may already have covered this buffer version.
            if parsed_version.as_ref() == Some(buffer_snapshot.version())
                && parsed_as_header == first_row_is_header
            {
                return view.update(cx, |view, cx| {
                    view.is_parsing = false;
                    cx.notify();
                });
            }

            let instant = Instant::now();
            let parsed_contents = cx
                .background_spawn(async move {
                    from_buffer_with_delimiter(&buffer_snapshot, delimiter, first_row_is_header)
                })
                .await;
            let parse_duration = instant.elapsed();
            let parse_end_time: Instant = Instant::now();
            log::debug!("Parsed data in {}ms", parse_duration.as_millis());
            view.update(cx, move |view, cx| {
                view.performance_metrics
                    .timings
                    .insert("Parsing", (parse_duration, Instant::now()));

                log::debug!("Parsed {} rows", parsed_contents.rows.len());
                view.engine.contents = Arc::new(parsed_contents);
                view.engine.calculate_available_filters();
                view.sync_column_widths(cx);
                view.last_parse_end_time = Some(parse_end_time);

                view.is_parsing = false;
                view.apply_filter_sort(cx);
                cx.notify();
            })
        })
    }
}

pub fn from_buffer_with_delimiter(
    buffer_snapshot: &BufferSnapshot,
    delimiter: char,
    first_row_is_header: bool,
) -> TableLikeContent {
    let source_version = Some(buffer_snapshot.version().clone());
    let source_byte_len = buffer_snapshot.len();
    let empty_contents = || TableLikeContent {
        first_row_is_header,
        source_version: source_version.clone(),
        source_byte_len,
        ..TableLikeContent::default()
    };

    let text = buffer_snapshot.text();
    if text.is_empty() {
        return empty_contents();
    }

    let parsed_rows = parse_delimited_text_with_positions(&text, delimiter);

    // Calculating the longest row, as the data might have fewer headers than max row width
    let Some(max_number_of_cols) = parsed_rows.iter().map(|row| row.cells.len()).max() else {
        return empty_contents();
    };
    if max_number_of_cols == 0 {
        return empty_contents();
    }

    let mut parsed_rows = parsed_rows.into_iter();
    let (headers, header_span) = match first_row_is_header.then(|| parsed_rows.next()).flatten() {
        Some(header) => (
            create_table_row(buffer_snapshot, max_number_of_cols, header.cells),
            Some(span_for_range(buffer_snapshot, header.range)),
        ),
        None => (
            TableRow::from_element(TableCell::Virtual, max_number_of_cols),
            None,
        ),
    };

    let mut rows = Vec::new();
    let mut line_numbers = Vec::new();
    let mut row_spans = Vec::new();
    for parsed_row in parsed_rows {
        rows.push(create_table_row(
            buffer_snapshot,
            max_number_of_cols,
            parsed_row.cells,
        ));
        line_numbers.push(parsed_row.line_number);
        row_spans.push(span_for_range(buffer_snapshot, parsed_row.range));
    }

    TableLikeContent {
        headers,
        rows,
        line_numbers,
        row_spans,
        header_span,
        number_of_cols: max_number_of_cols,
        first_row_is_header,
        source_version,
        source_byte_len,
    }
}

/// Parses delimited text into plain cell values, without the buffer anchors
/// [`from_buffer_with_delimiter`] hangs off every cell.
///
/// The table comparison works on text it was handed rather than on an open
/// buffer - the committed version of a file has no buffer at all - and it only
/// ever needs the values. Going through the same scanner is what makes two
/// files that differ only in their quoting compare as equal.
pub(crate) fn parse_cell_rows(text: &str, delimiter: char) -> Vec<Vec<SharedString>> {
    parse_delimited_text_with_positions(text, delimiter)
        .into_iter()
        .map(|row| row.cells.into_iter().map(|(value, _range)| value).collect())
        .collect()
}

fn span_for_range(buffer_snapshot: &BufferSnapshot, range: Range<usize>) -> CellContentSpan {
    CellContentSpan {
        start: buffer_snapshot.anchor_before(range.start),
        end: buffer_snapshot.anchor_after(range.end),
    }
}

/// One source line (or several, when a quoted field contains newlines) of delimited text.
struct ParsedRow {
    cells: Vec<(SharedString, Range<usize>)>,
    /// Byte range of the whole row, excluding its line terminator
    range: Range<usize>,
    line_number: LineNumber,
}

/// Parse delimited text and track byte positions for each cell.
///
/// Blank lines are kept as a row holding a single empty cell, so the grid can show and edit them
/// the way a spreadsheet does. A trailing line terminator does not produce a row of its own.
fn parse_delimited_text_with_positions(text: &str, delimiter: char) -> Vec<ParsedRow> {
    let mut rows = Vec::new();
    let mut current_cells: Vec<(SharedString, Range<usize>)> = Vec::new();
    let mut current_field = String::new();
    // `None` until the field has a first character, so an empty field ends up with an empty range
    // at the position where it was expected, while a quoted field keeps its opening quote.
    let mut field_start_offset: Option<usize> = None;
    let mut current_offset = 0;
    let mut row_start_offset = 0;
    let mut in_quotes = false;
    let mut current_line = 1; // 1-based line numbering
    let mut row_start_line = 1;
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        let char_byte_len = ch.len_utf8();

        match ch {
            '"' => {
                if in_quotes {
                    if chars.peek() == Some(&'"') {
                        // Escaped quote
                        chars.next();
                        current_field.push('"');
                        current_offset += 1; // Skip the second quote
                    } else {
                        // End of quoted field
                        in_quotes = false;
                    }
                } else {
                    // Start of quoted field
                    in_quotes = true;
                    field_start_offset.get_or_insert(current_offset);
                }
            }
            c if c == delimiter && !in_quotes => {
                let start = field_start_offset.take().unwrap_or(current_offset);
                current_cells.push((current_field.as_str().into(), start..current_offset));
                current_field.clear();
            }
            '\n' | '\r' if !in_quotes => {
                let terminator_len = if ch == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                    char_byte_len + 1
                } else {
                    char_byte_len
                };

                let row_end_offset = current_offset;
                let start = field_start_offset.take().unwrap_or(row_end_offset);
                current_cells.push((current_field.as_str().into(), start..row_end_offset));
                current_field.clear();

                rows.push(ParsedRow {
                    cells: std::mem::take(&mut current_cells),
                    range: row_start_offset..row_end_offset,
                    line_number: line_number_for(row_start_line, current_line),
                });

                current_line += 1;
                row_start_line = current_line;
                current_offset += terminator_len;
                row_start_offset = current_offset;
                continue;
            }
            '\n' | '\r' => {
                // Line terminator inside quotes - preserve it, but keep counting source lines
                if ch == '\n' || chars.peek() != Some(&'\n') {
                    current_line += 1;
                }
                field_start_offset.get_or_insert(current_offset);
                current_field.push(ch);
            }
            _ => {
                field_start_offset.get_or_insert(current_offset);
                current_field.push(ch);
            }
        }

        current_offset += char_byte_len;
    }

    // A buffer ending in a line terminator must not grow a phantom trailing row.
    if field_start_offset.is_some() || !current_cells.is_empty() {
        let start = field_start_offset.unwrap_or(current_offset);
        current_cells.push((current_field.as_str().into(), start..current_offset));
        rows.push(ParsedRow {
            cells: current_cells,
            range: row_start_offset..current_offset,
            line_number: line_number_for(row_start_line, current_line),
        });
    }

    rows
}

fn line_number_for(row_start_line: usize, row_end_line: usize) -> LineNumber {
    if row_start_line == row_end_line {
        LineNumber::Line(row_start_line)
    } else {
        LineNumber::LineRange(row_start_line, row_end_line)
    }
}

fn create_table_row(
    buffer_snapshot: &BufferSnapshot,
    max_number_of_cols: usize,
    row: Vec<(SharedString, Range<usize>)>,
) -> TableRow<TableCell> {
    let mut raw_row = row
        .into_iter()
        .map(|(content, range)| {
            TableCell::from_buffer_position(content, range.start, range.end, buffer_snapshot)
        })
        .collect::<Vec<_>>();

    let append_elements = max_number_of_cols.saturating_sub(raw_row.len());
    for _ in 0..append_elements {
        raw_row.push(TableCell::Virtual);
    }

    TableRow::from_vec(raw_row, max_number_of_cols)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_csv_parsing_basic() {
        let csv_data = "Name,Age,City\nJohn,30,New York\nJane,25,Los Angeles";
        let parsed = TableLikeContent::from_str(csv_data.to_string(), true);

        assert_eq!(parsed.headers.cols(), 3);
        assert_eq!(parsed.headers[0].display_value().unwrap().as_ref(), "Name");
        assert_eq!(parsed.headers[1].display_value().unwrap().as_ref(), "Age");
        assert_eq!(parsed.headers[2].display_value().unwrap().as_ref(), "City");

        assert_eq!(parsed.rows.len(), 2);
        assert_eq!(parsed.rows[0][0].display_value().unwrap().as_ref(), "John");
        assert_eq!(parsed.rows[0][1].display_value().unwrap().as_ref(), "30");
        assert_eq!(
            parsed.rows[0][2].display_value().unwrap().as_ref(),
            "New York"
        );
    }

    #[test]
    fn test_csv_parsing_with_quotes() {
        let csv_data = r#"Name,Description
"John Doe","A person with ""special"" characters"
Jane,"Simple name""#;
        let parsed = TableLikeContent::from_str(csv_data.to_string(), true);

        assert_eq!(parsed.headers.cols(), 2);
        assert_eq!(parsed.rows.len(), 2);
        assert_eq!(
            parsed.rows[0][1].display_value().unwrap().as_ref(),
            r#"A person with "special" characters"#
        );
    }

    #[test]
    fn test_csv_parsing_with_newlines_in_quotes() {
        let csv_data = "Name,Description,Status\n\"John\nDoe\",\"A person with\nmultiple lines\",Active\n\"Jane Smith\",\"Simple\",\"Also\nActive\"";
        let parsed = TableLikeContent::from_str(csv_data.to_string(), true);

        assert_eq!(parsed.headers.cols(), 3);
        assert_eq!(parsed.headers[0].display_value().unwrap().as_ref(), "Name");
        assert_eq!(
            parsed.headers[1].display_value().unwrap().as_ref(),
            "Description"
        );
        assert_eq!(
            parsed.headers[2].display_value().unwrap().as_ref(),
            "Status"
        );

        assert_eq!(parsed.rows.len(), 2);
        assert_eq!(
            parsed.rows[0][0].display_value().unwrap().as_ref(),
            "John\nDoe"
        );
        assert_eq!(
            parsed.rows[0][1].display_value().unwrap().as_ref(),
            "A person with\nmultiple lines"
        );
        assert_eq!(
            parsed.rows[0][2].display_value().unwrap().as_ref(),
            "Active"
        );

        assert_eq!(
            parsed.rows[1][0].display_value().unwrap().as_ref(),
            "Jane Smith"
        );
        assert_eq!(
            parsed.rows[1][1].display_value().unwrap().as_ref(),
            "Simple"
        );
        assert_eq!(
            parsed.rows[1][2].display_value().unwrap().as_ref(),
            "Also\nActive"
        );

        // Check line numbers
        assert_eq!(parsed.line_numbers.len(), 2);
        match &parsed.line_numbers[0] {
            LineNumber::LineRange(start, end) => {
                assert_eq!(start, &2);
                assert_eq!(end, &4);
            }
            _ => panic!("Expected LineRange for multiline row"),
        }
        match &parsed.line_numbers[1] {
            LineNumber::LineRange(start, end) => {
                assert_eq!(start, &5);
                assert_eq!(end, &6);
            }
            _ => panic!("Expected LineRange for second multiline row"),
        }
    }

    #[test]
    fn test_empty_input() {
        let parsed = TableLikeContent::from_str("".to_string(), true);
        assert_eq!(parsed.headers.cols(), 0);
        assert!(parsed.rows.is_empty());
        assert!(parsed.header_span.is_none());
        assert!(parsed.row_spans.is_empty());
    }

    #[test]
    fn test_tsv_parsing() {
        let tsv_data = "Name\tAge\tCity\nJohn\t30\tNew York\nJane\t25\tLos Angeles";
        let parsed_rows = parse_delimited_text_with_positions(tsv_data, '\t');

        assert_eq!(parsed_rows.len(), 3);
        assert_eq!(parsed_rows[0].cells.len(), 3);
        assert_eq!(parsed_rows[0].cells[0].0.as_ref(), "Name");
        assert_eq!(parsed_rows[0].cells[1].0.as_ref(), "Age");
        assert_eq!(parsed_rows[0].cells[2].0.as_ref(), "City");
        assert_eq!(parsed_rows[1].cells[0].0.as_ref(), "John");
        assert_eq!(parsed_rows[1].cells[1].0.as_ref(), "30");
    }

    #[test]
    fn test_psv_parsing() {
        let psv_data = "Name|Age|City\nJohn|30|New York\nJane|25|Los Angeles";
        let parsed_rows = parse_delimited_text_with_positions(psv_data, '|');

        assert_eq!(parsed_rows.len(), 3);
        assert_eq!(parsed_rows[0].cells.len(), 3);
        assert_eq!(parsed_rows[0].cells[0].0.as_ref(), "Name");
        assert_eq!(parsed_rows[0].cells[1].0.as_ref(), "Age");
        assert_eq!(parsed_rows[0].cells[2].0.as_ref(), "City");
        assert_eq!(parsed_rows[1].cells[0].0.as_ref(), "John");
        assert_eq!(parsed_rows[1].cells[1].0.as_ref(), "30");
    }

    #[test]
    fn test_ssv_parsing() {
        let ssv_data = "Name;Age;City\nJohn;30;New York\nJane;25;Los Angeles";
        let parsed_rows = parse_delimited_text_with_positions(ssv_data, ';');

        assert_eq!(parsed_rows.len(), 3);
        assert_eq!(parsed_rows[0].cells.len(), 3);
        assert_eq!(parsed_rows[0].cells[0].0.as_ref(), "Name");
        assert_eq!(parsed_rows[0].cells[1].0.as_ref(), "Age");
        assert_eq!(parsed_rows[0].cells[2].0.as_ref(), "City");
        assert_eq!(parsed_rows[1].cells[0].0.as_ref(), "John");
        assert_eq!(parsed_rows[1].cells[1].0.as_ref(), "30");
    }

    #[test]
    fn test_csv_parsing_quote_offset_handling() {
        let csv_data = r#"first,"se,cond",third"#;
        let parsed_rows = parse_delimited_text_with_positions(csv_data, ',');

        assert_eq!(parsed_rows.len(), 1); // One row
        assert_eq!(parsed_rows[0].cells.len(), 3); // Three cells

        // first: 0..5 (no quotes)
        let (content1, range1) = &parsed_rows[0].cells[0];
        assert_eq!(content1.as_ref(), "first");
        assert_eq!(*range1, 0..5);

        // "se,cond": 6..15 (includes quotes in range, content without quotes)
        let (content2, range2) = &parsed_rows[0].cells[1];
        assert_eq!(content2.as_ref(), "se,cond");
        assert_eq!(*range2, 6..15);

        // third: 16..21 (no quotes)
        let (content3, range3) = &parsed_rows[0].cells[2];
        assert_eq!(content3.as_ref(), "third");
        assert_eq!(*range3, 16..21);

        assert_eq!(parsed_rows[0].range, 0..21);
    }

    #[test]
    fn test_csv_parsing_complex_quotes() {
        let csv_data = r#"id,"name with spaces","description, with commas",status
1,"John Doe","A person with ""quotes"" and, commas",active
2,"Jane Smith","Simple description",inactive"#;
        let parsed_rows = parse_delimited_text_with_positions(csv_data, ',');

        assert_eq!(parsed_rows.len(), 3); // header + 2 rows

        // Check header row
        let header_row = &parsed_rows[0].cells;
        assert_eq!(header_row.len(), 4);

        // id: 0..2
        assert_eq!(header_row[0].0.as_ref(), "id");
        assert_eq!(header_row[0].1, 0..2);

        // "name with spaces": 3..21 (includes quotes)
        assert_eq!(header_row[1].0.as_ref(), "name with spaces");
        assert_eq!(header_row[1].1, 3..21);

        // "description, with commas": 22..48 (includes quotes)
        assert_eq!(header_row[2].0.as_ref(), "description, with commas");
        assert_eq!(header_row[2].1, 22..48);

        // status: 49..55
        assert_eq!(header_row[3].0.as_ref(), "status");
        assert_eq!(header_row[3].1, 49..55);

        // Check first data row
        let first_row = &parsed_rows[1].cells;
        assert_eq!(first_row.len(), 4);

        // 1: 56..57
        assert_eq!(first_row[0].0.as_ref(), "1");
        assert_eq!(first_row[0].1, 56..57);

        // "John Doe": 58..68 (includes quotes)
        assert_eq!(first_row[1].0.as_ref(), "John Doe");
        assert_eq!(first_row[1].1, 58..68);

        // Content should be stripped of quotes but include escaped quotes
        assert_eq!(
            first_row[2].0.as_ref(),
            r#"A person with "quotes" and, commas"#
        );
        // The range should include the outer quotes: 69..107
        assert_eq!(first_row[2].1, 69..107);

        // active: 108..114
        assert_eq!(first_row[3].0.as_ref(), "active");
        assert_eq!(first_row[3].1, 108..114);
    }

    #[test]
    fn test_empty_quoted_field_keeps_its_quotes_in_range() {
        let parsed_rows = parse_delimited_text_with_positions(r#"a,"",b"#, ',');

        assert_eq!(parsed_rows.len(), 1);
        assert_eq!(parsed_rows[0].cells[1].0.as_ref(), "");
        assert_eq!(parsed_rows[0].cells[1].1, 2..4);
    }

    #[test]
    fn test_blank_lines_are_kept_as_rows() {
        let parsed = TableLikeContent::from_str("a,b\n\nc,d\n".to_string(), true);

        assert_eq!(parsed.rows.len(), 2);
        assert_eq!(parsed.rows[0][0].display_value().unwrap().as_ref(), "");
        assert!(matches!(parsed.rows[0][1], TableCell::Virtual));
        assert_eq!(parsed.rows[1][0].display_value().unwrap().as_ref(), "c");
        assert_eq!(parsed.row_spans.len(), 2);
    }

    #[test]
    fn test_trailing_newline_does_not_add_a_row() {
        let with_newline = TableLikeContent::from_str("a,b\nc,d\n".to_string(), true);
        let without_newline = TableLikeContent::from_str("a,b\nc,d".to_string(), true);

        assert_eq!(with_newline.rows.len(), 1);
        assert_eq!(without_newline.rows.len(), 1);
    }

    #[test]
    fn test_first_row_is_data_when_header_is_disabled() {
        let parsed = TableLikeContent::from_str("a,b\nc,d".to_string(), false);

        assert!(parsed.header_span.is_none());
        assert_eq!(parsed.headers.cols(), 2);
        assert!(matches!(parsed.headers[0], TableCell::Virtual));
        assert_eq!(parsed.rows.len(), 2);
        assert_eq!(parsed.rows[0][0].display_value().unwrap().as_ref(), "a");
        assert_eq!(parsed.line_numbers.len(), 2);
        assert_eq!(parsed.row_spans.len(), 2);
        assert!(!parsed.first_row_is_header);
    }

    #[test]
    fn test_row_spans_cover_whole_rows() {
        let text = "a,b\nc,d\n".to_string();
        let buffer = crate::edit_engine::tests::new_buffer(&text);
        let snapshot = buffer.snapshot().clone();
        let parsed = from_buffer_with_delimiter(&snapshot, ',', true);

        let header_span = parsed.header_span.expect("header row should have a span");
        assert_eq!(
            crate::edit_engine::tests::span_offsets(&header_span, &snapshot),
            0..3
        );
        assert_eq!(
            crate::edit_engine::tests::span_offsets(&parsed.row_spans[0], &snapshot),
            4..7
        );
    }

    #[test]
    fn test_source_version_is_recorded() {
        let buffer = crate::edit_engine::tests::new_buffer("a,b\n");
        let snapshot = buffer.snapshot().clone();
        let parsed = from_buffer_with_delimiter(&snapshot, ',', true);

        assert_eq!(parsed.source_version.as_ref(), Some(snapshot.version()));
    }
}

impl TableLikeContent {
    #[cfg(test)]
    pub fn from_str(text: String, first_row_is_header: bool) -> Self {
        let buffer = crate::edit_engine::tests::new_buffer(&text);
        let snapshot = buffer.snapshot();
        from_buffer_with_delimiter(snapshot, ',', first_row_is_header)
    }
}
