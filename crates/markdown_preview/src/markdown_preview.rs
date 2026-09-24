use gpui::{App, actions};
use workspace::Workspace;

pub mod fold_headings_selector;
pub mod image_dialog;
pub mod markdown_export;
pub mod markdown_preview_settings;
pub mod markdown_preview_view;
pub mod mermaid_diagram_selector;
pub mod mermaid_editor;
pub mod table_editor;

pub use zed_actions::preview::markdown::{
    EditMermaid, EditTable, ExportToPdf, OpenPreview, OpenPreviewToTheSide,
};

use crate::markdown_preview_view::MarkdownPreviewView;

actions!(
    markdown,
    [
        /// Scrolls up by one page in the markdown preview.
        #[action(deprecated_aliases = ["markdown::MovePageUp"])]
        ScrollPageUp,
        /// Scrolls down by one page in the markdown preview.
        #[action(deprecated_aliases = ["markdown::MovePageDown"])]
        ScrollPageDown,
        /// Scrolls up by approximately one visual line.
        ScrollUp,
        /// Scrolls down by approximately one visual line.
        ScrollDown,
        /// Scrolls up by one markdown element in the markdown preview
        ScrollUpByItem,
        /// Scrolls down by one markdown element in the markdown preview
        ScrollDownByItem,
        /// Scrolls to the top of the markdown preview.
        ScrollToTop,
        /// Scrolls to the bottom of the markdown preview.
        ScrollToBottom,
        /// Opens a following markdown preview that syncs with the editor.
        OpenFollowingPreview,
        /// Closes the markdown preview and returns focus to the source editor.
        CloseAndReturnToEditor,
        /// Picks the Mermaid diagram type to insert at the cursor, since
        /// `markdown::InsertMermaidDiagram` can only carry one type at a time.
        InsertMermaidDiagramPicker,
        /// Picks the heading level to fold, since `markdown::FoldHeadings`
        /// always carries one and so cannot be built from the palette.
        FoldHeadingsPicker,
        /// Opens the image dialog: inserts an image at the cursor, or edits the
        /// image the cursor is on.
        InsertImage
    ]
);

pub fn init(cx: &mut App) {
    workspace::register_serializable_item::<MarkdownPreviewView>(cx);

    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        markdown_preview_view::MarkdownPreviewView::register(workspace, window, cx);
        mermaid_diagram_selector::register(workspace);
        fold_headings_selector::register(workspace);
        table_editor::register(workspace);
        mermaid_editor::register(workspace);
        image_dialog::register(workspace);
    })
    .detach();
}
