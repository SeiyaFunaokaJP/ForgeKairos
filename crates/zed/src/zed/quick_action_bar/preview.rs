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

        // SVG and tables open beside the source by default so both stay visible;
        // Markdown opens as a tab of its own pane, and Ctrl-click splits it off.
        let (button_id, tooltip_text, open_action_for_tooltip, modified_click_meta) =
            match &preview_target {
                PreviewTarget::Markdown(_) => (
                    "toggle-markdown-preview",
                    i18n::t!("Preview Markdown"),
                    &markdown_preview::OpenPreview as &dyn gpui::Action,
                    i18n::msg!("{} to open in a split"),
                ),
                PreviewTarget::Svg(_) => (
                    "toggle-svg-preview",
                    i18n::t!("Preview SVG"),
                    &svg_preview::OpenPreviewToTheSide as &dyn gpui::Action,
                    i18n::msg!("{} to open in the same tab group"),
                ),
                // A grid and a text tab for one file cannot share a pane, so Alt swaps this
                // tab instead of adding one next to it.
                PreviewTarget::TabularData(_) => (
                    "toggle-tabular-preview",
                    i18n::t!("Preview Tabular Data"),
                    &tabular_data_preview::OpenPreviewToTheSide as &dyn gpui::Action,
                    i18n::msg!("{} to switch this tab"),
                ),
            };

        let modified_click = gpui::Keystroke {
            key: "click".into(),
            modifiers: match &preview_target {
                PreviewTarget::Markdown(_) => Modifiers::secondary_key(),
                PreviewTarget::Svg(_) | PreviewTarget::TabularData(_) => Modifiers::alt(),
            },
            ..Default::default()
        };

        let button = IconButton::new(button_id, IconName::Eye)
            .icon_size(IconSize::Small)
            .style(ButtonStyle::Subtle)
            .tooltip(move |_window, cx| {
                Tooltip::with_meta(
                    tooltip_text,
                    Some(open_action_for_tooltip),
                    i18n::tr(modified_click_meta).replace(
                        "{}",
                        &text_for_keystroke(&modified_click.modifiers, &modified_click.key, cx),
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
                        let open_in_same_pane = window.modifiers().alt;
                        match &preview_target {
                            PreviewTarget::Markdown(editor) => {
                                let editor = editor.clone();
                                if window.modifiers().secondary() {
                                    MarkdownPreviewView::open_preview_to_the_side_of_pane(
                                        workspace, editor, pane, window, cx,
                                    );
                                } else {
                                    MarkdownPreviewView::open_preview_in_pane(
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
                Tooltip::for_action(
                    i18n::t!("Export to PDF"),
                    &markdown_preview::ExportToPdf,
                    cx,
                )
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
