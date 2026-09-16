use std::{ops::Range, sync::Arc};

use gpui::{
    App, FontStyle, FontWeight, StrikethroughStyle, TextAlign, TextStyleRefinement, UnderlineStyle,
};
use pulldown_cmark::Alignment;
use ui::prelude::*;

use crate::html::html_parser::{
    HtmlHighlightStyle, HtmlImage, HtmlParagraph, HtmlParagraphChunk, ParsedHtmlBlock,
    ParsedHtmlDetails, ParsedHtmlElement, ParsedHtmlList, ParsedHtmlListItemType, ParsedHtmlTable,
    ParsedHtmlTableRow, ParsedHtmlText,
};
use crate::{MarkdownElement, MarkdownElementBuilder};

/// Shown in place of a missing `<summary>`, matching what a browser puts there.
const DEFAULT_DETAILS_SUMMARY: &str = "詳細";

pub(crate) struct HtmlSourceAllocator {
    source_range: Range<usize>,
    next_source_index: usize,
}

impl HtmlSourceAllocator {
    pub(crate) fn new(source_range: Range<usize>) -> Self {
        Self {
            next_source_index: source_range.start,
            source_range,
        }
    }

    pub(crate) fn allocate(&mut self, requested_len: usize) -> Range<usize> {
        let remaining = self.source_range.end.saturating_sub(self.next_source_index);
        let len = requested_len.min(remaining);
        let start = self.next_source_index;
        let end = start + len;
        self.next_source_index = end;
        start..end
    }
}

impl MarkdownElement {
    /// Renders an HTML block, returning whether it ends in a `<details>` whose
    /// body continues in later blocks and is currently collapsed, so the caller
    /// can skip those blocks.
    pub(crate) fn render_html_block(
        &self,
        block: &ParsedHtmlBlock,
        builder: &mut MarkdownElementBuilder,
        markdown_end: usize,
        cx: &mut App,
    ) -> bool {
        let mut source_allocator = HtmlSourceAllocator::new(block.source_range.clone());

        let spanning_index = block.details_span.and_then(|_| {
            block
                .children
                .iter()
                .rposition(|element| matches!(element, ParsedHtmlElement::Details(_)))
        });

        let mut collapsed = false;
        for (index, element) in block.children.iter().enumerate() {
            match element {
                ParsedHtmlElement::Details(details) if Some(index) == spanning_index => {
                    collapsed = !self.render_html_details(
                        details,
                        true,
                        &mut source_allocator,
                        builder,
                        markdown_end,
                        cx,
                    );
                }
                _ => self.render_html_element(
                    element,
                    &mut source_allocator,
                    builder,
                    markdown_end,
                    cx,
                ),
            }
        }

        collapsed
    }

    fn render_html_elements(
        &self,
        elements: &[ParsedHtmlElement],
        source_allocator: &mut HtmlSourceAllocator,
        builder: &mut MarkdownElementBuilder,
        markdown_end: usize,
        cx: &mut App,
    ) {
        for element in elements {
            self.render_html_element(element, source_allocator, builder, markdown_end, cx);
        }
    }

    fn render_html_element(
        &self,
        element: &ParsedHtmlElement,
        source_allocator: &mut HtmlSourceAllocator,
        builder: &mut MarkdownElementBuilder,
        markdown_end: usize,
        cx: &mut App,
    ) {
        let Some(source_range) = element.source_range() else {
            return;
        };

        match element {
            ParsedHtmlElement::Paragraph(paragraph) => {
                self.push_markdown_paragraph(
                    builder,
                    &source_range,
                    markdown_end,
                    paragraph.text_align,
                );
                self.render_html_paragraph(
                    &paragraph.contents,
                    source_allocator,
                    builder,
                    cx,
                    markdown_end,
                );
                self.pop_markdown_paragraph(builder);
            }
            ParsedHtmlElement::Heading(heading) => {
                self.push_markdown_heading(
                    builder,
                    heading.level,
                    &heading.source_range,
                    markdown_end,
                    heading.text_align,
                );
                self.render_html_paragraph(
                    &heading.contents,
                    source_allocator,
                    builder,
                    cx,
                    markdown_end,
                );
                self.pop_markdown_heading(builder);
            }
            ParsedHtmlElement::List(list) => {
                self.render_html_list(list, source_allocator, builder, markdown_end, cx);
            }
            ParsedHtmlElement::BlockQuote(block_quote) => {
                self.push_markdown_block_quote(
                    builder,
                    None,
                    &block_quote.source_range,
                    markdown_end,
                );
                self.render_html_elements(
                    &block_quote.children,
                    source_allocator,
                    builder,
                    markdown_end,
                    cx,
                );
                self.pop_markdown_block_quote(builder);
            }
            ParsedHtmlElement::Table(table) => {
                self.render_html_table(table, source_allocator, builder, markdown_end, cx);
            }
            ParsedHtmlElement::Image(image) => {
                self.render_html_image(image, builder, cx);
            }
            ParsedHtmlElement::Details(details) => {
                self.render_html_details(
                    details,
                    false,
                    source_allocator,
                    builder,
                    markdown_end,
                    cx,
                );
            }
            ParsedHtmlElement::Rule(rule_range) => {
                builder.push_div(
                    div()
                        .border_b_1()
                        .my_2()
                        .border_color(self.style.rule_color),
                    rule_range,
                    markdown_end,
                );
                builder.pop_div();
            }
        }
    }

    /// Renders a `<details>` disclosure, returning whether it is open.
    ///
    /// `spans_blocks` is set for the disclosure whose body continues in the
    /// following root blocks: its own container has to end with this block, so
    /// only the summary row and whatever shares the block are drawn here.
    fn render_html_details(
        &self,
        details: &ParsedHtmlDetails,
        spans_blocks: bool,
        source_allocator: &mut HtmlSourceAllocator,
        builder: &mut MarkdownElementBuilder,
        markdown_end: usize,
        cx: &mut App,
    ) -> bool {
        let key = (details.source_range.start, details.index);
        let open_by_default = details.open;
        let is_open = self.markdown.read(cx).is_details_open(key, open_by_default);

        builder.push_div(
            div().when(!spans_blocks, |this| {
                this.my_1()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .rounded_sm()
                    .px_2()
                    .py_1()
            }),
            &details.source_range,
            markdown_end,
        );

        let markdown = self.markdown.clone();
        let summary_id = ElementId::NamedChild(
            Arc::new(ElementId::from(("markdown-details", markdown.entity_id()))),
            format!("{}-{}", key.0, key.1).into(),
        );

        builder.push_div(
            div()
                .id(summary_id)
                .flex()
                .flex_row()
                .items_start()
                .gap_1()
                .cursor_pointer()
                .on_click(move |_event, _window, cx| {
                    markdown.update(cx, |markdown, cx| {
                        markdown.toggle_details(key, open_by_default);
                        cx.notify();
                    });
                })
                .child(
                    Icon::new(if is_open {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .size(IconSize::Small)
                    .color(Color::Muted),
                ),
            &details.source_range,
            markdown_end,
        );

        if details.summary.is_empty() {
            let summary_range = source_allocator.allocate(DEFAULT_DETAILS_SUMMARY.len());
            builder.push_text(DEFAULT_DETAILS_SUMMARY, summary_range);
        } else {
            self.render_html_paragraph(
                &details.summary,
                source_allocator,
                builder,
                cx,
                markdown_end,
            );
        }

        builder.pop_div();

        if is_open && !details.children.is_empty() {
            builder.push_div(div().pl_3(), &details.source_range, markdown_end);
            self.render_html_elements(
                &details.children,
                source_allocator,
                builder,
                markdown_end,
                cx,
            );
            builder.pop_div();
        }

        builder.pop_div();

        is_open
    }

    fn render_html_list(
        &self,
        list: &ParsedHtmlList,
        source_allocator: &mut HtmlSourceAllocator,
        builder: &mut MarkdownElementBuilder,
        markdown_end: usize,
        cx: &mut App,
    ) {
        builder.push_div(div().pl_2p5(), &list.source_range, markdown_end);

        for list_item in &list.items {
            let bullet = match list_item.item_type {
                ParsedHtmlListItemType::Ordered(order) => html_list_item_prefix(
                    order as usize,
                    list.ordered,
                    list.depth.saturating_sub(1) as usize,
                ),
                ParsedHtmlListItemType::Unordered => {
                    html_list_item_prefix(1, false, list.depth.saturating_sub(1) as usize)
                }
            };

            self.push_markdown_list_item(
                builder,
                div().child(bullet).into_any_element(),
                &list_item.source_range,
                markdown_end,
            );
            self.render_html_elements(
                &list_item.content,
                source_allocator,
                builder,
                markdown_end,
                cx,
            );
            self.pop_markdown_list_item(builder);
        }

        builder.pop_div();
    }

    fn render_html_table(
        &self,
        table: &ParsedHtmlTable,
        source_allocator: &mut HtmlSourceAllocator,
        builder: &mut MarkdownElementBuilder,
        markdown_end: usize,
        cx: &mut App,
    ) {
        if let Some(caption) = &table.caption {
            builder.push_div(
                div().when(!self.style.height_is_multiple_of_line_height, |el| {
                    el.mb_2().line_height(rems(1.3))
                }),
                &table.source_range,
                markdown_end,
            );
            self.render_html_paragraph(caption, source_allocator, builder, cx, markdown_end);
            builder.pop_div();
        }

        let actual_header_column_count = html_table_columns_count(&table.header);
        let actual_body_column_count = html_table_columns_count(&table.body);
        let max_column_count = actual_header_column_count.max(actual_body_column_count);

        if max_column_count == 0 {
            return;
        }

        let total_rows = table.header.len() + table.body.len();
        let mut grid_occupied = vec![vec![false; max_column_count]; total_rows];

        builder.push_div(div().flex(), &table.source_range, markdown_end);
        builder.push_div(
            div()
                .id(("html-table", table.source_range.start))
                .min_w_0()
                .grid()
                .when(self.style.table_columns_min_size, |this| {
                    this.w_full().grid_cols_min_content(max_column_count as u16)
                })
                .when(!self.style.table_columns_min_size, |this| {
                    this.grid_cols_max_content(max_column_count as u16)
                })
                .mb_2()
                .border(px(1.5))
                .border_color(cx.theme().colors().border)
                .rounded_sm()
                .overflow_hidden(),
            &table.source_range,
            markdown_end,
        );

        for (row_index, row) in table.header.iter().chain(table.body.iter()).enumerate() {
            let mut column_index = 0;

            for cell in &row.columns {
                while column_index < max_column_count && grid_occupied[row_index][column_index] {
                    column_index += 1;
                }

                if column_index >= max_column_count {
                    break;
                }

                let max_span = max_column_count.saturating_sub(column_index);
                let text_align = match cell.alignment {
                    Alignment::Left => TextAlign::Left,
                    Alignment::Center => TextAlign::Center,
                    Alignment::Right => TextAlign::Right,
                    _ => self.style.base_text_style.text_align,
                };

                let mut cell_div = div()
                    .col_span(cell.col_span.min(max_span) as u16)
                    .row_span(cell.row_span.min(total_rows - row_index) as u16)
                    .flex()
                    .flex_col()
                    .when(column_index > 0, |this| this.border_l_1())
                    .when(row_index > 0, |this| this.border_t_1())
                    .border_color(cx.theme().colors().border)
                    .px_2()
                    .py_1()
                    .h_full()
                    .when(cell.is_header, |this| {
                        this.bg(cx.theme().colors().title_bar_background)
                    })
                    .when(!cell.is_header && row_index % 2 == 1, |this| {
                        this.bg(cx.theme().colors().panel_background)
                    });

                cell_div = match cell.alignment {
                    Alignment::Center => cell_div.items_center(),
                    Alignment::Right => cell_div.items_end(),
                    _ => cell_div,
                };

                builder.push_text_style(TextStyleRefinement {
                    text_align: Some(text_align),
                    ..Default::default()
                });
                builder.push_div(cell_div, &table.source_range, markdown_end);
                builder.push_div(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .w_full()
                        .justify_center()
                        .text_align(text_align),
                    &table.source_range,
                    markdown_end,
                );
                self.render_html_paragraph(
                    &cell.children,
                    source_allocator,
                    builder,
                    cx,
                    markdown_end,
                );
                builder.pop_div();
                builder.pop_div();
                builder.pop_text_style();

                for row_offset in 0..cell.row_span {
                    for column_offset in 0..cell.col_span {
                        if row_index + row_offset < total_rows
                            && column_index + column_offset < max_column_count
                        {
                            grid_occupied[row_index + row_offset][column_index + column_offset] =
                                true;
                        }
                    }
                }

                column_index += cell.col_span;
            }

            while column_index < max_column_count {
                if grid_occupied[row_index][column_index] {
                    column_index += 1;
                    continue;
                }

                builder.push_div(
                    div()
                        .when(column_index > 0, |this| this.border_l_1())
                        .when(row_index > 0, |this| this.border_t_1())
                        .border_color(cx.theme().colors().border)
                        .when(row_index % 2 == 1, |this| {
                            this.bg(cx.theme().colors().panel_background)
                        }),
                    &table.source_range,
                    markdown_end,
                );
                builder.pop_div();
                column_index += 1;
            }
        }

        builder.pop_div();
        builder.pop_div();
    }

    fn render_html_paragraph(
        &self,
        paragraph: &HtmlParagraph,
        source_allocator: &mut HtmlSourceAllocator,
        builder: &mut MarkdownElementBuilder,
        cx: &mut App,
        _markdown_end: usize,
    ) {
        for chunk in paragraph {
            match chunk {
                HtmlParagraphChunk::Text(text) => {
                    self.render_html_text(text, source_allocator, builder, cx);
                }
                HtmlParagraphChunk::Image(image) => {
                    self.render_html_image(image, builder, cx);
                }
            }
        }
    }

    fn render_html_text(
        &self,
        text: &ParsedHtmlText,
        source_allocator: &mut HtmlSourceAllocator,
        builder: &mut MarkdownElementBuilder,
        cx: &mut App,
    ) {
        let text_contents = text.contents.as_ref();
        if text_contents.is_empty() {
            return;
        }

        let allocated_range = source_allocator.allocate(text_contents.len());
        let allocated_len = allocated_range.end.saturating_sub(allocated_range.start);

        let mut boundaries = vec![0, text_contents.len()];
        for (range, _) in &text.highlights {
            boundaries.push(range.start);
            boundaries.push(range.end);
        }
        for (range, _) in &text.links {
            boundaries.push(range.start);
            boundaries.push(range.end);
        }
        boundaries.sort_unstable();
        boundaries.dedup();

        for &[start, end] in boundaries.array_windows::<2>() {
            if start >= end {
                continue;
            }

            let source_start = allocated_range.start + start.min(allocated_len);
            let source_end = allocated_range.start + end.min(allocated_len);
            if source_start >= source_end {
                continue;
            }

            let mut refinement = TextStyleRefinement::default();
            let mut has_refinement = false;
            let mut is_code = false;

            for (highlight_range, style) in &text.highlights {
                if highlight_range.start < end && highlight_range.end > start {
                    apply_html_highlight_style(&mut refinement, style);
                    has_refinement = true;
                    is_code |= style.code;
                }
            }

            if is_code {
                let mut code_style = self.style.inline_code.clone();
                code_style.refine(&refinement);
                refinement = code_style;
            }

            let link = text.links.iter().find_map(|(link_range, link)| {
                if link_range.start < end && link_range.end > start {
                    Some(link.clone())
                } else {
                    None
                }
            });

            if let Some(link) = link.as_ref() {
                builder.push_link(link.clone(), source_start..source_end);
                let link_style = self
                    .style
                    .link_callback
                    .as_ref()
                    .and_then(|callback| callback(link.as_ref(), cx))
                    .unwrap_or_else(|| self.style.link.clone());
                builder.push_text_style(link_style);
            }

            if has_refinement {
                builder.push_text_style(refinement);
            }

            // `<br>` reaches the renderer as a newline inside the paragraph's
            // text. Handing it to the builder's line break instead of leaving it
            // in the string makes it lay out exactly like markdown's own hard
            // break, including in the flex-wrap row a paragraph with an image
            // uses, where a bare newline would leave an empty line behind.
            let segment = &text_contents[start..end];
            if segment.contains('\n') {
                let mut rendered = start;
                for piece in segment.split('\n') {
                    if rendered > start {
                        builder.push_line_break(
                            allocated_range.start + (rendered - 1).min(allocated_len)
                                ..allocated_range.start + rendered.min(allocated_len),
                        );
                    }
                    if !piece.is_empty() {
                        builder.push_text(
                            piece,
                            allocated_range.start + rendered.min(allocated_len)
                                ..allocated_range.start
                                    + (rendered + piece.len()).min(allocated_len),
                        );
                    }
                    rendered += piece.len() + 1;
                }
            } else {
                builder.push_text(segment, source_start..source_end);
            }

            if has_refinement {
                builder.pop_text_style();
            }

            if link.is_some() {
                builder.pop_text_style();
            }
        }
    }

    fn render_html_image(&self, image: &HtmlImage, builder: &mut MarkdownElementBuilder, cx: &App) {
        let Some(source) = self
            .image_resolver
            .as_ref()
            .and_then(|resolve| resolve(image.dest_url.as_ref(), cx))
        else {
            return;
        };

        self.push_markdown_image(
            builder,
            &image.source_range,
            source,
            image.dest_url.clone(),
            image.alt_text.clone(),
            image.width,
            image.height,
        );
    }
}

fn apply_html_highlight_style(refinement: &mut TextStyleRefinement, style: &HtmlHighlightStyle) {
    if style.weight != FontWeight::default() {
        refinement.font_weight = Some(style.weight);
    }

    if style.oblique {
        refinement.font_style = Some(FontStyle::Oblique);
    } else if style.italic {
        refinement.font_style = Some(FontStyle::Italic);
    }

    if style.underline {
        refinement.underline = Some(UnderlineStyle {
            thickness: px(1.),
            color: None,
            ..Default::default()
        });
    }

    if style.strikethrough {
        refinement.strikethrough = Some(StrikethroughStyle {
            thickness: px(1.),
            color: None,
        });
    }
}

fn html_list_item_prefix(order: usize, ordered: bool, depth: usize) -> String {
    let index = order.saturating_sub(1);
    const NUMBERED_PREFIXES_1: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    const NUMBERED_PREFIXES_2: &str = "abcdefghijklmnopqrstuvwxyz";
    const BULLETS: [&str; 5] = ["•", "◦", "▪", "‣", "⁃"];

    if ordered {
        match depth {
            0 => format!("{}. ", order),
            1 => format!(
                "{}. ",
                NUMBERED_PREFIXES_1
                    .chars()
                    .nth(index % NUMBERED_PREFIXES_1.len())
                    .unwrap()
            ),
            _ => format!(
                "{}. ",
                NUMBERED_PREFIXES_2
                    .chars()
                    .nth(index % NUMBERED_PREFIXES_2.len())
                    .unwrap()
            ),
        }
    } else {
        let depth = depth.min(BULLETS.len() - 1);
        format!("{} ", BULLETS[depth])
    }
}

fn html_table_columns_count(rows: &[ParsedHtmlTableRow]) -> usize {
    let mut actual_column_count = 0;
    for row in rows {
        actual_column_count = actual_column_count.max(
            row.columns
                .iter()
                .map(|column| column.col_span)
                .sum::<usize>(),
        );
    }
    actual_column_count
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use gpui::{Entity, TestAppContext, size};
    use ui::prelude::*;

    use crate::{
        CodeBlockRenderer, CopyButtonVisibility, Markdown, MarkdownElement, MarkdownOptions,
        MarkdownStyle,
    };

    fn ensure_theme_initialized(cx: &mut TestAppContext) {
        cx.update(|cx| {
            if !cx.has_global::<settings::SettingsStore>() {
                settings::init(cx);
            }
            if !cx.has_global::<theme::GlobalTheme>() {
                theme_settings::init(theme::LoadThemes::JustBase, cx);
            }
        });
    }

    use crate::WrapButtonVisibility;

    fn render_markdown_text(markdown: &str, cx: &mut TestAppContext) -> crate::RenderedText {
        struct TestWindow;

        impl Render for TestWindow {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
            }
        }

        ensure_theme_initialized(cx);

        let (_, cx) = cx.add_window_view(|_, _| TestWindow);
        let markdown = cx.new(|cx| Markdown::new(markdown.to_string().into(), None, None, cx));
        cx.run_until_parked();
        let (rendered, _) = cx.draw(
            Default::default(),
            size(px(600.0), px(600.0)),
            |_window, _cx| {
                MarkdownElement::new(markdown, MarkdownStyle::default()).code_block_renderer(
                    CodeBlockRenderer::Default {
                        copy_button_visibility: CopyButtonVisibility::Hidden,
                        wrap_button_visibility: WrapButtonVisibility::Hidden,
                        border: false,
                    },
                )
            },
        );
        rendered.text
    }

    #[gpui::test]
    fn test_html_block_rendering_smoke(cx: &mut TestAppContext) {
        let rendered = render_markdown_text(
            "<h1>Hello</h1><blockquote><p>world</p></blockquote><ul><li>item</li></ul>",
            cx,
        );

        let rendered_lines = rendered
            .lines
            .iter()
            .map(|line| line.layout.wrapped_text())
            .collect::<Vec<_>>();

        assert_eq!(
            rendered_lines.concat().replace('\n', ""),
            "<h1>Hello</h1><blockquote><p>world</p></blockquote><ul><li>item</li></ul>"
        );
    }

    #[gpui::test]
    fn test_html_block_rendering_can_be_enabled(cx: &mut TestAppContext) {
        struct TestWindow;

        impl Render for TestWindow {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
            }
        }

        ensure_theme_initialized(cx);

        let (_, cx) = cx.add_window_view(|_, _| TestWindow);
        let markdown = cx.new(|cx| {
            Markdown::new_with_options(
                "<h1>Hello</h1><blockquote><p>world</p></blockquote><ul><li>item</li></ul>".into(),
                None,
                None,
                MarkdownOptions {
                    parse_html: true,
                    ..Default::default()
                },
                cx,
            )
        });
        cx.run_until_parked();
        let (rendered, _) = cx.draw(
            Default::default(),
            size(px(600.0), px(600.0)),
            |_window, _cx| {
                MarkdownElement::new(markdown, MarkdownStyle::default()).code_block_renderer(
                    CodeBlockRenderer::Default {
                        copy_button_visibility: CopyButtonVisibility::Hidden,
                        wrap_button_visibility: WrapButtonVisibility::Hidden,
                        border: false,
                    },
                )
            },
        );

        let rendered_lines = rendered
            .text
            .lines
            .iter()
            .map(|line| line.layout.wrapped_text())
            .collect::<Vec<_>>();

        assert_eq!(rendered_lines[0], "Hello");
        assert_eq!(rendered_lines[1], "world");
        assert!(rendered_lines.iter().any(|line| line.contains("item")));
    }

    /// Renders `source` with HTML parsing on, optionally toggling a disclosure
    /// first, and returns everything the preview laid out as one string.
    fn render_html_preview(
        source: &str,
        toggled_details: Option<(usize, usize)>,
        cx: &mut TestAppContext,
    ) -> String {
        // The disclosure's summary row handles clicks, and an element with
        // listeners can only be laid out while a view is rendering, so the
        // markdown has to be drawn from inside one.
        struct PreviewTestView {
            markdown: Entity<Markdown>,
            rendered_text: Rc<RefCell<Option<crate::RenderedText>>>,
        }

        impl Render for PreviewTestView {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let rendered_text = self.rendered_text.clone();
                MarkdownElement::new(self.markdown.clone(), MarkdownStyle::default())
                    .code_block_renderer(CodeBlockRenderer::Default {
                        copy_button_visibility: CopyButtonVisibility::Hidden,
                        wrap_button_visibility: WrapButtonVisibility::Hidden,
                        border: false,
                    })
                    .on_render(move |text| *rendered_text.borrow_mut() = Some(text))
            }
        }

        ensure_theme_initialized(cx);

        let markdown = cx.new(|cx| {
            Markdown::new_with_options(
                source.to_string().into(),
                None,
                None,
                MarkdownOptions {
                    parse_html: true,
                    ..Default::default()
                },
                cx,
            )
        });
        cx.run_until_parked();

        if let Some(key) = toggled_details {
            markdown.update(cx, |markdown, cx| {
                markdown.toggle_details(key, false);
                cx.notify();
            });
        }

        let rendered_text = Rc::new(RefCell::new(None));
        let (_, cx) = cx.add_window_view({
            let markdown = markdown.clone();
            let rendered_text = rendered_text.clone();
            move |_, _| PreviewTestView {
                markdown,
                rendered_text,
            }
        });
        cx.run_until_parked();

        let rendered_text = rendered_text
            .borrow()
            .clone()
            .expect("markdown should be rendered in the test view");

        rendered_text
            .lines
            .iter()
            .map(|line| line.layout.wrapped_text())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[gpui::test]
    fn test_details_hides_its_body_until_the_summary_is_clicked(cx: &mut TestAppContext) {
        let source = "<details><summary>題</summary>本文</details>";

        let collapsed = render_html_preview(source, None, cx);
        assert!(collapsed.contains('題'), "got: {collapsed:?}");
        assert!(!collapsed.contains("本文"), "got: {collapsed:?}");

        let expanded = render_html_preview(source, Some((0, 0)), cx);
        assert!(expanded.contains('題'), "got: {expanded:?}");
        assert!(expanded.contains("本文"), "got: {expanded:?}");
    }

    #[gpui::test]
    fn test_details_open_attribute_starts_expanded(cx: &mut TestAppContext) {
        let expanded = render_html_preview(
            "<details open><summary>題</summary>本文</details>",
            None,
            cx,
        );
        assert!(expanded.contains("本文"), "got: {expanded:?}");
    }

    #[gpui::test]
    fn test_details_without_summary_falls_back_to_a_label(cx: &mut TestAppContext) {
        let collapsed = render_html_preview("<details>本文だけ</details>", None, cx);
        assert!(collapsed.contains("詳細"), "got: {collapsed:?}");
        assert!(!collapsed.contains("本文だけ"), "got: {collapsed:?}");
    }

    #[gpui::test]
    fn test_details_split_across_blocks_hides_the_markdown_body(cx: &mut TestAppContext) {
        let source = "<details>\n<summary>題</summary>\n\n本文 **太字**\n\n</details>\n\n後ろ";

        let collapsed = render_html_preview(source, None, cx);
        assert!(collapsed.contains('題'), "got: {collapsed:?}");
        assert!(!collapsed.contains("本文"), "got: {collapsed:?}");
        assert!(collapsed.contains("後ろ"), "got: {collapsed:?}");
        assert!(!collapsed.contains("details"), "got: {collapsed:?}");

        let expanded = render_html_preview(source, Some((0, 0)), cx);
        assert!(expanded.contains("本文"), "got: {expanded:?}");
        assert!(expanded.contains("太字"), "got: {expanded:?}");
        assert!(expanded.contains("後ろ"), "got: {expanded:?}");
    }

    #[gpui::test]
    fn test_br_inside_an_html_block_renders_a_line_break(cx: &mut TestAppContext) {
        for source in ["<p>a<br>b</p>", "<div>a<br/>b</div>"] {
            let rendered = render_html_preview(source, None, cx);
            assert!(
                rendered.contains("a\nb"),
                "{source} should break the line; got: {rendered:?}"
            );
            assert!(
                !rendered.contains("<br"),
                "{source} should not show the tag; got: {rendered:?}"
            );
        }
    }

    #[gpui::test]
    fn test_br_inside_a_markdown_table_cell_renders_a_line_break(cx: &mut TestAppContext) {
        let rendered = render_html_preview("| a<br>b | c |\n| --- | --- |\n| d | e |", None, cx);
        assert!(rendered.contains("a\nb"), "got: {rendered:?}");
        assert!(!rendered.contains("<br"), "got: {rendered:?}");
    }

    #[gpui::test]
    fn test_unsupported_html_block_still_shows_its_text(cx: &mut TestAppContext) {
        let rendered =
            render_html_preview("<section><figure>図の説明</figure></section>", None, cx);
        assert!(rendered.contains("図の説明"), "got: {rendered:?}");
    }

    #[gpui::test]
    fn test_adjacent_details_keep_their_own_summaries(cx: &mut TestAppContext) {
        // No blank line between `</details>` and the next `<details>`, so one
        // HTML block both closes a disclosure and opens the next one.
        let source = "<details>
<summary>A</summary>

本文A

</details>
<details>
<summary>B</summary>

本文B

</details>

後ろ";

        let collapsed = render_html_preview(source, None, cx);
        assert!(collapsed.contains('A'), "got: {collapsed:?}");
        assert!(collapsed.contains('B'), "got: {collapsed:?}");
        assert!(!collapsed.contains("本文"), "got: {collapsed:?}");
        assert!(collapsed.contains("後ろ"), "got: {collapsed:?}");

        let first_expanded = render_html_preview(source, Some((0, 0)), cx);
        assert!(first_expanded.contains("本文A"), "got: {first_expanded:?}");
        assert!(!first_expanded.contains("本文B"), "got: {first_expanded:?}");
        assert!(first_expanded.contains('B'), "got: {first_expanded:?}");
    }
}
