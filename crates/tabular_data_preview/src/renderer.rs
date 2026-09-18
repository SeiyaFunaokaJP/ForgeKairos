#[cfg(feature = "dev-tools")]
mod performance_metrics_overlay;
mod preview_view;
pub(crate) mod render_table;
mod row_identifiers;
mod settings;
// Public within the crate so that the grid can size a row it has not measured
// yet from the same frame the cells draw around their text.
pub(crate) mod table_cell;
pub(crate) mod table_diff_view;
// Public within the crate so that the pane can hold a handle to the column
// filter popover, which is the only way to open it from the keyboard.
pub(crate) mod table_header;
