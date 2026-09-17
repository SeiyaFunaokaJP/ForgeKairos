use editor::{Editor, MultiBuffer};
use gpui::{AnyElement, Entity, Modifiers};
use markdown_preview::markdown_preview_view::MarkdownPreviewView;
use svg_preview::svg_preview_view::SvgPreviewView;
use tabular_data_preview::TabularDataPreviewPane;
use ui::{Tooltip, prelude::*, text_for_keystroke};

use super::QuickActionBar;

enum PreviewTarget {
    Markdown(Entity<Editor>),
    Svg(Entity<MultiBuffer>),
    TabularData(Entity<Editor>),
}

impl QuickActionBar {
    pub fn render_preview_button(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        // Resolve against this toolbar's own pane item rather than the
        // workspace's focused item, so each pane's button reflects and
        // targets the content of the pane it belongs to.
        let active_item = self.active_item.as_ref()?;
        let editor = active_item.act_as::<Editor>(cx);

        let preview_target = if let Some(editor) = &editor
            && MarkdownPreviewView::is_markdown_file(editor, cx)
        {
            PreviewTarget::Markdown(editor.clone())
        } else if let Some(buffer) = active_item.act_as::<MultiBuffer>(cx)
            && SvgPreviewView::is_svg_file(&buffer, cx)
        {
            PreviewTarget::Svg(buffer)
        } else if let Some(editor) = editor
            && TabularDataPreviewPane::is_tabular_data_file(&editor, cx)
        {
            PreviewTarget::TabularData(editor)
        } else {
            return None;
        };

        // The button opens the preview beside the source, so both stay visible;
        // the tooltip therefore advertises the "to the side" action's binding.
        let (button_id, tooltip_text, open_action_for_tooltip, alt_click_meta) =
            match &preview_target {
                PreviewTarget::Markdown(_) => (
                    "toggle-markdown-preview",
                    "Markdown をプレビュー",
                    &markdown_preview::OpenPreviewToTheSide as &dyn gpui::Action,
                    "{} で同じタブグループに開く",
                ),
                PreviewTarget::Svg(_) => (
                    "toggle-svg-preview",
                    "SVG をプレビュー",
                    &svg_preview::OpenPreviewToTheSide as &dyn gpui::Action,
                    "{} で同じタブグループに開く",
                ),
                // A grid and a text tab for one file cannot share a pane, so Alt swaps this
                // tab instead of adding one next to it.
                PreviewTarget::TabularData(_) => (
                    "toggle-tabular-preview",
                    "表を横に開く",
                    &tabular_data_preview::OpenPreviewToTheSide as &dyn gpui::Action,
                    "{} でこのタブを切り替える",
                ),
            };

        let alt_click = gpui::Keystroke {
            key: "click".into(),
            modifiers: Modifiers::alt(),
            ..Default::default()
        };

        let button = IconButton::new(button_id, IconName::Eye)
            .icon_size(IconSize::Small)
            .style(ButtonStyle::Subtle)
            .tooltip(move |_window, cx| {
                Tooltip::with_meta(
                    tooltip_text,
                    Some(open_action_for_tooltip),
                    alt_click_meta.replace(
                        "{}",
                        &text_for_keystroke(&alt_click.modifiers, &alt_click.key, cx),
                    ),
                    cx,
                )
            })
            .on_click({
                let workspace_handle = self.workspace.clone();
                let active_item = active_item.boxed_clone();
                move |_, window, cx| {
                    let Some(workspace) = workspace_handle.upgrade() else {
                        return;
                    };
                    workspace.update(cx, |workspace, cx| {
                        let Some(pane) = workspace.pane_for(active_item.as_ref()) else {
                            return;
                        };
                        // Splitting is the default: opening the preview in the
                        // same pane buries the source it was rendered from.
                        let open_in_same_pane = window.modifiers().alt;
                        match &preview_target {
                            PreviewTarget::Markdown(editor) => {
                                let editor = editor.clone();
                                if open_in_same_pane {
                                    MarkdownPreviewView::open_preview_in_pane(
                                        workspace, editor, pane, window, cx,
                                    );
                                } else {
                                    MarkdownPreviewView::open_preview_to_the_side_of_pane(
                                        workspace, editor, pane, window, cx,
                                    );
                                }
                            }
                            PreviewTarget::Svg(buffer) => {
                                let buffer = buffer.clone();
                                if open_in_same_pane {
                                    SvgPreviewView::open_preview_in_pane(
                                        workspace, buffer, pane, window, cx,
                                    );
                                } else {
                                    SvgPreviewView::open_preview_to_the_side_of_pane(
                                        workspace, buffer, pane, window, cx,
                                    );
                                }
                            }
                            PreviewTarget::TabularData(editor) => {
                                let editor = editor.clone();
                                if open_in_same_pane {
                                    TabularDataPreviewPane::open_preview_in_pane(
                                        editor, pane, window, cx,
                                    );
                                } else {
                                    TabularDataPreviewPane::open_preview_to_the_side_of_pane(
                                        workspace, editor, pane, window, cx,
                                    );
                                }
                            }
                        }
                    });
                }
            });

        Some(button.into_any_element())
    }

    pub fn render_export_pdf_button(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let active_item = self.active_item.as_ref()?;
        let editor = active_item.act_as::<Editor>(cx)?;
        if !MarkdownPreviewView::is_markdown_file(&editor, cx) {
            return None;
        }

        let button = IconButton::new("export-markdown-to-pdf", IconName::Download)
            .icon_size(IconSize::Small)
            .style(ButtonStyle::Subtle)
            .tooltip(|_window, cx| {
                Tooltip::for_action("PDF に書き出す", &markdown_preview::ExportToPdf, cx)
            })
            .on_click({
                let workspace_handle = self.workspace.clone();
                move |_, window, cx| {
                    let Some(workspace) = workspace_handle.upgrade() else {
                        return;
                    };
                    let editor = editor.clone();
                    workspace.update(cx, |workspace, cx| {
                        MarkdownPreviewView::export_to_pdf(workspace, editor, window, cx);
                    });
                }
            });

        Some(button.into_any_element())
    }
}
