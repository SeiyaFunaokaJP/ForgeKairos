use std::{ops::Range, sync::Arc};

use gpui::{
    App, FontStyle, FontWeight, GridColumnWidth, StrikethroughStyle, TextAlign,
    TextStyleRefinement, UnderlineStyle,
};
use pulldown_cmark::Alignment;
use ui::{ScrollAxes, Scrollbars, WithScrollbar, prelude::*};

use crate::html::html_parser::{
    HtmlColumnWidth, HtmlHighlightStyle, HtmlImage, HtmlParagraph, HtmlParagraphChunk,
    HtmlSpanningCell, HtmlSpanningTable, HtmlTablePart, HtmlVerticalAlign, ParsedHtmlBlock,
    ParsedHtmlDetails, ParsedHtmlElement, ParsedHtmlList, ParsedHtmlListItemType, ParsedHtmlTable,
    ParsedHtmlTableRow, ParsedHtmlText, html_paragraph_text,
};
use crate::{MarkdownElement, MarkdownElementBuilder, TableColumnMinWidths};

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
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        if !block.table_parts.is_empty() {
            return self.render_html_table_parts(block, builder, markdown_end, window, cx);
        }

        let mut source_allocator = HtmlSourceAllocator::new(block.source_range.clone());

        let spanning_index = block.details_span.as_ref().and_then(|_| {
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
                        window,
                        cx,
                    );
                }
                _ => self.render_html_element(
                    element,
                    &mut source_allocator,
                    builder,
                    markdown_end,
                    window,
                    cx,
                ),
            }
        }

        collapsed
    }

    /// Draws a block that takes part in a table spanning blocks: the HTML
    /// around and inside the table's cells, and the table's own boxes, which
    /// stay open for the markdown of the following blocks to land in.
    fn render_html_table_parts(
        &self,
        block: &ParsedHtmlBlock,
        builder: &mut MarkdownElementBuilder,
        markdown_end: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let spanning_index = block.details_span.as_ref().and_then(|_| {
            block
                .children
                .iter()
                .rposition(|element| matches!(element, ParsedHtmlElement::Details(_)))
        });

        let mut collapsed = false;
        for part in &block.table_parts {
            match part {
                HtmlTablePart::Elements(range) => {
                    let Some(elements) = block.children.get(range.clone()) else {
                        continue;
                    };
                    let mut source_allocator = HtmlSourceAllocator::new(
                        elements
                            .first()
                            .and_then(ParsedHtmlElement::source_range)
                            .unwrap_or_else(|| block.source_range.clone()),
                    );
                    for (offset, element) in elements.iter().enumerate() {
                        match element {
                            ParsedHtmlElement::Details(details)
                                if Some(range.start + offset) == spanning_index =>
                            {
                                collapsed = !self.render_html_details(
                                    details,
                                    true,
                                    &mut source_allocator,
                                    builder,
                                    markdown_end,
                                    window,
                                    cx,
                                );
                            }
                            _ => self.render_html_element(
                                element,
                                &mut source_allocator,
                                builder,
                                markdown_end,
                                window,
                                cx,
                            ),
                        }
                    }
                }
                HtmlTablePart::OpenTable(table) => {
                    self.push_html_spanning_table(table, block, builder, markdown_end, window, cx);
                }
                HtmlTablePart::OpenCell(cell) => {
                    self.push_html_spanning_cell(
                        cell,
                        &block.source_range,
                        builder,
                        markdown_end,
                        cx,
                    );
                }
                HtmlTablePart::CloseCell => {
                    builder.pop_div();
                    builder.pop_div();
                    builder.pop_text_style();
                }
                HtmlTablePart::EndRow {
                    row,
                    occupied_columns,
                } => {
                    let column_count = builder
                        .spanning_table_columns
                        .last()
                        .copied()
                        .unwrap_or_default();
                    for column in 0..column_count {
                        if occupied_columns.get(column).copied().unwrap_or(false) {
                            continue;
                        }
                        builder.push_div(
                            place_in_grid(div(), *row, column, 1, 1)
                                .when(column > 0, |this| this.border_l_1())
                                .when(*row > 0, |this| this.border_t_1())
                                .border_color(cx.theme().colors().border)
                                .when(row % 2 == 1, |this| {
                                    this.bg(cx.theme().colors().panel_background)
                                }),
                            &block.source_range,
                            markdown_end,
                        );
                        builder.pop_div();
                    }
                }
                HtmlTablePart::CloseTable {
                    has_caption,
                    caption_below,
                } => {
                    builder.pop_div();
                    if let Some(range) = caption_below {
                        self.render_html_caption(block, range, builder, markdown_end, window, cx);
                    }
                    if *has_caption {
                        builder.pop_div();
                    }
                    builder.pop_div();
                    builder.spanning_table_columns.pop();
                }
            }
        }

        collapsed
    }

    /// Opens the boxes of a table spanning blocks, drawn like an HTML table
    /// that fits in one block. Its columns share the width the way a
    /// browser's do, since a cell's markdown can be as long as a paragraph.
    fn push_html_spanning_table(
        &self,
        table: &HtmlSpanningTable,
        block: &ParsedHtmlBlock,
        builder: &mut MarkdownElementBuilder,
        markdown_end: usize,
        window: &mut Window,
        cx: &mut App,
    ) {
        let source_range = &block.source_range;
        let column_tracks = html_table_column_tracks(&table.column_widths, None);
        let fills_width = column_tracks
            .as_ref()
            .is_some_and(|(_, fills_width)| *fills_width);
        let table_row = match table.text_align.or_else(|| builder.inherited_text_align()) {
            None | Some(TextAlign::Left) => div().flex(),
            Some(TextAlign::Center) => div().flex().justify_center(),
            Some(TextAlign::Right) => div().flex().justify_end(),
        };
        builder.push_div(table_row, source_range, markdown_end);
        if table.has_caption {
            builder.push_div(
                html_captioned_table_box(fills_width),
                source_range,
                markdown_end,
            );
            if let Some(range) = &table.caption_above {
                self.render_html_caption(block, range, builder, markdown_end, window, cx);
            }
        }

        let column_count = table.column_count as u16;
        builder.push_div(
            div()
                .id(("html-spanning-table", table.source_start))
                .debug_selector(|| "markdown_html_spanning_table".into())
                .min_w_0()
                .when(table.has_caption, |this| this.max_w_full())
                .grid()
                .map(|this| {
                    if self.style.table_columns_min_size {
                        this.w_full().grid_cols_min_content(column_count)
                    } else if let Some((tracks, fills_width)) = column_tracks {
                        this.when(fills_width, |this| this.w_full())
                            .grid_cols_widths(tracks)
                    } else {
                        this.grid_cols_max_content(column_count)
                    }
                })
                .mb_2()
                .border(px(1.5))
                .border_color(cx.theme().colors().border)
                .rounded_sm()
                .when_else(
                    self.style.table_overflow_x_scroll,
                    |this| {
                        this.restrict_scroll_to_axis().custom_scrollbars(
                            Scrollbars::new(ScrollAxes::Horizontal)
                                .id(("markdown-html-spanning-table-scrollbar", table.source_start))
                                .notify_content(),
                            window,
                            cx,
                        )
                    },
                    |this| this.overflow_hidden(),
                ),
            source_range,
            markdown_end,
        );
        builder.spanning_table_columns.push(table.column_count);
    }

    /// Draws the elements of a table spanning blocks that make up its
    /// `<caption>`, which the parser already centered.
    fn render_html_caption(
        &self,
        block: &ParsedHtmlBlock,
        range: &Range<usize>,
        builder: &mut MarkdownElementBuilder,
        markdown_end: usize,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(elements) = block.children.get(range.clone()) else {
            return;
        };
        let mut source_allocator = HtmlSourceAllocator::new(
            elements
                .first()
                .and_then(ParsedHtmlElement::source_range)
                .unwrap_or_else(|| block.source_range.clone()),
        );
        self.render_html_elements(
            elements,
            &mut source_allocator,
            builder,
            markdown_end,
            window,
            cx,
        );
    }

    fn push_html_spanning_cell(
        &self,
        cell: &HtmlSpanningCell,
        source_range: &Range<usize>,
        builder: &mut MarkdownElementBuilder,
        markdown_end: usize,
        cx: &App,
    ) {
        let column_count = builder.spanning_table_columns.last().copied().unwrap_or(1);
        let col_span = cell
            .col_span
            .min(column_count.saturating_sub(cell.column))
            .max(1);
        let text_align = cell
            .text_align
            .unwrap_or(self.style.base_text_style.text_align);

        builder.push_text_style(TextStyleRefinement {
            text_align: cell.text_align,
            ..Default::default()
        });
        builder.push_div(
            place_in_grid(div(), cell.row, cell.column, cell.row_span, col_span)
                .flex()
                .flex_col()
                .when(cell.column > 0, |this| this.border_l_1())
                .when(cell.row > 0, |this| this.border_t_1())
                .border_color(cx.theme().colors().border)
                .px_2()
                .py_1()
                .h_full()
                .when(cell.is_header, |this| {
                    this.bg(cx.theme().colors().title_bar_background)
                })
                .when(!cell.is_header && cell.row % 2 == 1, |this| {
                    this.bg(cx.theme().colors().panel_background)
                }),
            source_range,
            markdown_end,
        );
        builder.push_div(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .w_full()
                .map(|this| match cell.vertical_align {
                    HtmlVerticalAlign::Top => this.justify_start(),
                    HtmlVerticalAlign::Middle => this.justify_center(),
                    HtmlVerticalAlign::Bottom => this.justify_end(),
                })
                .text_align(text_align),
            source_range,
            markdown_end,
        );
    }

    fn render_html_elements(
        &self,
        elements: &[ParsedHtmlElement],
        source_allocator: &mut HtmlSourceAllocator,
        builder: &mut MarkdownElementBuilder,
        markdown_end: usize,
        window: &mut Window,
        cx: &mut App,
    ) {
        for element in elements {
            self.render_html_element(element, source_allocator, builder, markdown_end, window, cx);
        }
    }

    fn render_html_element(
        &self,
        element: &ParsedHtmlElement,
        source_allocator: &mut HtmlSourceAllocator,
        builder: &mut MarkdownElementBuilder,
        markdown_end: usize,
        window: &mut Window,
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
                if let Some(percent) = paragraph.font_scale_percent {
                    self.push_paragraph_scale(builder, percent, &source_range, markdown_end);
                }
                self.render_html_paragraph(
                    &paragraph.contents,
                    source_allocator,
                    builder,
                    cx,
                    markdown_end,
                );
                if paragraph.font_scale_percent.is_some() {
                    self.pop_paragraph_scale(builder);
                }
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
                // A heading keeps the size of its level, the way the Markdown
                // path leaves a `font-size` in a heading alone.
                builder.html_heading_depth += 1;
                self.render_html_paragraph(
                    &heading.contents,
                    source_allocator,
                    builder,
                    cx,
                    markdown_end,
                );
                builder.html_heading_depth -= 1;
                self.pop_markdown_heading(builder);
            }
            ParsedHtmlElement::List(list) => {
                self.render_html_list(list, source_allocator, builder, markdown_end, window, cx);
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
                    window,
                    cx,
                );
                self.pop_markdown_block_quote(builder);
            }
            ParsedHtmlElement::Table(table) => {
                self.render_html_table(table, source_allocator, builder, markdown_end, window, cx);
            }
            ParsedHtmlElement::Image(image) => {
                // Laying an image out turns the div it lands in into a row, so
                // it gets one of its own rather than pulling the blocks drawn
                // around it into the same row.
                builder.push_div(div(), &image.source_range, markdown_end);
                self.render_html_image(image, builder, cx);
                builder.pop_div();
            }
            ParsedHtmlElement::Details(details) => {
                self.render_html_details(
                    details,
                    false,
                    source_allocator,
                    builder,
                    markdown_end,
                    window,
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
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let key = details.key;
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
            key.element_id_suffix().into(),
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
                window,
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
        window: &mut Window,
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
                window,
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
        window: &mut Window,
        cx: &mut App,
    ) {
        let grid = HtmlTableGrid::of(table);
        let max_column_count = grid.column_count();

        if max_column_count == 0 {
            if let Some(caption) = &table.caption {
                self.render_html_table_caption(
                    caption,
                    &table.source_range,
                    source_allocator,
                    builder,
                    markdown_end,
                    cx,
                );
            }
            return;
        }

        let total_rows = table.header.len() + table.body.len();

        let column_min_widths = self.style.table_overflow_x_scroll.then(|| {
            html_table_column_min_widths(
                table,
                &grid,
                self.style
                    .base_text_style
                    .font_size
                    .to_pixels(window.rem_size()),
                window.rem_size(),
            )
        });

        let column_tracks =
            html_table_column_tracks(&grid.column_widths(table), column_min_widths.as_deref());
        let fills_width = column_tracks
            .as_ref()
            .is_some_and(|(_, fills_width)| *fills_width);

        let table_row = match table.text_align {
            None | Some(TextAlign::Left) => div().flex(),
            Some(TextAlign::Center) => div().flex().justify_center(),
            Some(TextAlign::Right) => div().flex().justify_end(),
        };
        builder.push_div(table_row, &table.source_range, markdown_end);
        if let Some(caption) = &table.caption {
            builder.push_div(
                html_captioned_table_box(fills_width),
                &table.source_range,
                markdown_end,
            );
            if !table.caption_below {
                self.render_html_table_caption(
                    caption,
                    &table.source_range,
                    source_allocator,
                    builder,
                    markdown_end,
                    cx,
                );
            }
        }
        builder.push_div(
            div()
                .id(("html-table", table.source_range.start))
                .debug_selector(|| "markdown_html_table".into())
                .min_w_0()
                .when(table.caption.is_some(), |this| this.max_w_full())
                .grid()
                .map(|this| {
                    if self.style.table_columns_min_size {
                        this.w_full().grid_cols_min_content(max_column_count as u16)
                    } else if let Some((tracks, fills_width)) = column_tracks {
                        this.when(fills_width, |this| this.w_full())
                            .grid_cols_widths(tracks)
                    } else if let Some(column_min_widths) = column_min_widths {
                        this.grid_cols_min_widths(column_min_widths)
                    } else {
                        this.grid_cols_max_content(max_column_count as u16)
                    }
                })
                .mb_2()
                .border(px(1.5))
                .border_color(cx.theme().colors().border)
                .rounded_sm()
                // Only the preview scrolls its tables; everywhere else the table
                // still has to clip its own corners.
                .when_else(
                    self.style.table_overflow_x_scroll,
                    |this| {
                        this.restrict_scroll_to_axis().custom_scrollbars(
                            Scrollbars::new(ScrollAxes::Horizontal)
                                .id(("markdown-html-table-scrollbar", table.source_range.start))
                                .notify_content(),
                            window,
                            cx,
                        )
                    },
                    |this| this.overflow_hidden(),
                ),
            &table.source_range,
            markdown_end,
        );

        for (row_index, row) in table.header.iter().chain(table.body.iter()).enumerate() {
            for (cell_index, cell) in row.columns.iter().enumerate() {
                let Some(column_index) = grid.cell_column(row_index, cell_index) else {
                    break;
                };

                let max_span = max_column_count.saturating_sub(column_index);
                let alignment = match (cell.alignment, table.text_align) {
                    (Alignment::None, Some(TextAlign::Left)) => Alignment::Left,
                    (Alignment::None, Some(TextAlign::Center)) => Alignment::Center,
                    (Alignment::None, Some(TextAlign::Right)) => Alignment::Right,
                    (alignment, _) => alignment,
                };
                let text_align = match alignment {
                    Alignment::Left => TextAlign::Left,
                    Alignment::Center => TextAlign::Center,
                    Alignment::Right => TextAlign::Right,
                    _ => self.style.base_text_style.text_align,
                };

                let mut cell_div = place_in_grid(
                    div(),
                    row_index,
                    column_index,
                    cell.row_span.min(total_rows - row_index),
                    cell.col_span.min(max_span),
                )
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

                cell_div = match alignment {
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
            }

            // Whatever the row's own cells and the spans reaching into it from
            // above leave empty still needs a box, or the grid would pull the
            // following rows up into the hole.
            for column_index in 0..max_column_count {
                if grid.is_occupied(row_index, column_index) {
                    continue;
                }

                builder.push_div(
                    place_in_grid(div(), row_index, column_index, 1, 1)
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
            }
        }

        builder.pop_div();
        if let Some(caption) = &table.caption {
            if table.caption_below {
                self.render_html_table_caption(
                    caption,
                    &table.source_range,
                    source_allocator,
                    builder,
                    markdown_end,
                    cx,
                );
            }
            builder.pop_div();
        }
        builder.pop_div();
    }

    /// Draws a table's `<caption>` as a centered paragraph, the way a browser
    /// centers it over the table.
    fn render_html_table_caption(
        &self,
        caption: &HtmlParagraph,
        source_range: &Range<usize>,
        source_allocator: &mut HtmlSourceAllocator,
        builder: &mut MarkdownElementBuilder,
        markdown_end: usize,
        cx: &mut App,
    ) {
        self.push_markdown_paragraph(builder, source_range, markdown_end, Some(TextAlign::Center));
        self.render_html_paragraph(caption, source_allocator, builder, cx, markdown_end);
        self.pop_markdown_paragraph(builder);
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
            let mut font_scale = 1.0_f32;

            for (highlight_range, style) in &text.highlights {
                if highlight_range.start < end && highlight_range.end > start {
                    apply_html_highlight_style(&mut refinement, style);
                    has_refinement = true;
                    is_code |= style.code;
                    if let Some(percent) = style.font_scale_percent {
                        font_scale *= f32::from(percent) / 100.0;
                    }
                }
            }
            let scales_text = font_scale != 1.0 && builder.html_heading_depth == 0;

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
            if scales_text {
                builder.push_font_scale(font_scale);
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

            if scales_text {
                builder.pop_font_scale();
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

    // Highlights arrive outermost first, so a link inside a colored span takes
    // the link color back, while a span inside a link recolors it.
    if style.link {
        refinement.color = None;
    }
    if let Some(color) = style.color {
        refinement.color = Some(crate::html::html_parser::css_color_hsla(color));
    }

    if let Some(background_color) = style.background_color {
        refinement.background_color =
            Some(crate::html::html_parser::css_color_hsla(background_color));
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

/// The widest an HTML table may get. A `colspan` is a number the document is
/// free to make absurd, and every column of every row becomes an element, so
/// without a ceiling one cell can ask for a grid that does not fit in memory.
/// No table anyone can read comes anywhere near this.
const MAX_TABLE_COLUMNS: usize = 512;

/// Where an HTML table's cells land once `rowspan` and `colspan` are resolved.
///
/// A cell that spans rows keeps its columns busy in the rows below it, so the
/// cells of those rows start further right than their own `colspan`s alone
/// would say. Counting `colspan`s is therefore not enough, neither for the
/// column each cell belongs to nor for how many columns the table has.
struct HtmlTableGrid {
    /// Which columns of each row are taken, the header rows first.
    occupied: Vec<Vec<bool>>,
    /// The column each cell of each row starts at, in the same row order.
    cell_columns: Vec<Vec<usize>>,
    column_count: usize,
}

impl HtmlTableGrid {
    fn of(table: &ParsedHtmlTable) -> Self {
        Self::new(table.header.iter().chain(table.body.iter()))
    }

    fn new<'a>(rows: impl Iterator<Item = &'a ParsedHtmlTableRow>) -> Self {
        // Every row has to exist before the first cell is placed: a `rowspan`
        // claims columns in rows that have not been walked yet.
        let rows = rows.collect::<Vec<_>>();
        let mut grid = Self {
            occupied: vec![Vec::new(); rows.len()],
            cell_columns: Vec::with_capacity(rows.len()),
            column_count: 0,
        };

        for (row_index, row) in rows.iter().enumerate() {
            let mut columns = Vec::with_capacity(row.columns.len());
            let mut column_index = 0;

            for cell in &row.columns {
                while grid.is_occupied(row_index, column_index) {
                    column_index += 1;
                }
                if column_index >= MAX_TABLE_COLUMNS {
                    // The cells past the ceiling get no column at all, which is
                    // what `cell_column` returning `None` tells the renderer.
                    break;
                }
                let col_span = cell.col_span.min(MAX_TABLE_COLUMNS - column_index);
                grid.occupy(row_index, column_index, cell.row_span, col_span);
                columns.push(column_index);
                grid.column_count = grid.column_count.max(column_index + col_span);
                column_index += col_span;
            }

            grid.cell_columns.push(columns);
        }

        grid
    }

    fn column_count(&self) -> usize {
        self.column_count
    }

    fn is_occupied(&self, row_index: usize, column_index: usize) -> bool {
        self.occupied
            .get(row_index)
            .and_then(|row| row.get(column_index))
            .copied()
            .unwrap_or(false)
    }

    /// The width each column asks for through the cells that sit in it alone.
    fn column_widths(&self, table: &ParsedHtmlTable) -> Vec<Option<HtmlColumnWidth>> {
        let mut widths = vec![None; self.column_count];
        for (row_index, row) in table.header.iter().chain(table.body.iter()).enumerate() {
            for (cell_index, cell) in row.columns.iter().enumerate() {
                let Some(column_index) = self.cell_column(row_index, cell_index) else {
                    break;
                };
                if cell.col_span == 1
                    && let Some(width) = cell.width
                    && let Some(known) = widths.get_mut(column_index)
                {
                    *known = Some(HtmlColumnWidth::merge(*known, width));
                }
            }
        }
        widths
    }

    /// The column the `cell_index`th cell of `row_index` starts at, or `None`
    /// when the grid has no such cell.
    fn cell_column(&self, row_index: usize, cell_index: usize) -> Option<usize> {
        self.cell_columns.get(row_index)?.get(cell_index).copied()
    }

    fn occupy(&mut self, row_index: usize, column_index: usize, row_span: usize, col_span: usize) {
        let end = column_index + col_span;
        for row_offset in 0..row_span {
            // A `rowspan` past the last row claims nothing below the table.
            let Some(row) = self.occupied.get_mut(row_index + row_offset) else {
                break;
            };
            if row.len() < end {
                row.resize(end, false);
            }
            for column in row.get_mut(column_index..end).into_iter().flatten() {
                *column = true;
            }
        }
    }
}

/// Puts a table's cell at its own row and column of the grid, rather than
/// leaving it to the grid to find a place, which with `rowspan`s could differ
/// from where the table's own counting put it.
fn place_in_grid(cell: Div, row: usize, column: usize, row_span: usize, col_span: usize) -> Div {
    let span = |span: usize| u16::try_from(span).unwrap_or(u16::MAX).max(1);
    let cell = cell.col_span(span(col_span)).row_span(span(row_span));
    match (i16::try_from(row + 1), i16::try_from(column + 1)) {
        (Ok(row_line), Ok(column_line)) => cell.row_start(row_line).col_start(column_line),
        _ => cell,
    }
}

/// The box a table with a `<caption>` is drawn in together with it, as wide
/// as the table so the caption centers over the table rather than the page.
fn html_captioned_table_box(fills_width: bool) -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .min_w_0()
        .when(fills_width, |this| this.w_full())
}

/// The grid tracks of a table whose cells ask for widths, and whether the
/// table has to take the whole width for its percentages to have something to
/// be a part of. The columns that ask for none are sized to their content, or
/// share out the width the percentages leave. `floors` are the narrowest the
/// columns get to keep their text readable.
fn html_table_column_tracks(
    widths: &[Option<HtmlColumnWidth>],
    floors: Option<&[Pixels]>,
) -> Option<(Vec<GridColumnWidth>, bool)> {
    if widths.iter().all(Option::is_none) {
        return None;
    }
    let fills_width = widths
        .iter()
        .any(|width| matches!(width, Some(HtmlColumnWidth::Fraction(_))));
    let tracks = widths
        .iter()
        .enumerate()
        .map(|(column, width)| {
            let floor = floors
                .and_then(|floors| floors.get(column))
                .copied()
                .unwrap_or_default();
            match width {
                Some(HtmlColumnWidth::Fraction(fraction)) => GridColumnWidth::Fraction(*fraction),
                Some(HtmlColumnWidth::Pixels(pixels)) => GridColumnWidth::Content {
                    min: floor.max(px(*pixels)),
                },
                None if fills_width => GridColumnWidth::Remainder { min: floor },
                None => GridColumnWidth::Content { min: floor },
            }
        })
        .collect();
    Some((tracks, fills_width))
}

/// Floors for an HTML table's columns, the same ones a markdown table gets, so
/// a table that outgrows the pane scrolls instead of wrapping to one character
/// per line. Cells that span several columns are left out: their text says
/// nothing about how wide any single column has to be.
fn html_table_column_min_widths(
    table: &ParsedHtmlTable,
    grid: &HtmlTableGrid,
    font_size: Pixels,
    rem_size: Pixels,
) -> Vec<Pixels> {
    let mut widths = TableColumnMinWidths::new(grid.column_count(), font_size);
    for (row_index, row) in table.header.iter().chain(table.body.iter()).enumerate() {
        for (cell_index, cell) in row.columns.iter().enumerate() {
            let Some(column_index) = grid.cell_column(row_index, cell_index) else {
                break;
            };
            if cell.col_span == 1 {
                widths.observe_cell(column_index, &html_paragraph_text(&cell.children));
            }
        }
    }
    widths.finish(rem_size)
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use gpui::{Entity, TestAppContext, size};
    use ui::prelude::*;

    use super::{HtmlTableGrid, MAX_TABLE_COLUMNS, html_table_column_min_widths};
    use crate::html::html_parser::{ParsedHtmlElement, ParsedHtmlTable, html_details_key};
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
        toggled_details: Option<(&str, u32)>,
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

        if let Some((summary, ordinal)) = toggled_details {
            markdown.update(cx, |markdown, cx| {
                markdown.toggle_details(html_details_key(summary, ordinal), false);
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

        let expanded = render_html_preview(source, Some(("題", 0)), cx);
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

        let expanded = render_html_preview(source, Some(("題", 0)), cx);
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
    fn test_details_stay_open_when_the_text_before_them_changes(cx: &mut TestAppContext) {
        let body = "<details>\n<summary>\u{984c}</summary>\n\n\u{672c}\u{6587}\n\n</details>";
        let moved = format!("\u{524d}\u{66f8}\u{304d}\n\n{body}");

        let expanded = render_html_preview(body, Some(("\u{984c}", 0)), cx);
        assert!(expanded.contains("\u{672c}\u{6587}"), "got: {expanded:?}");

        // The same key has to reach the same disclosure once a line is inserted
        // in front of it, which a source-offset key could not do.
        let expanded_after_edit = render_html_preview(&moved, Some(("\u{984c}", 0)), cx);
        assert!(
            expanded_after_edit.contains("\u{672c}\u{6587}"),
            "got: {expanded_after_edit:?}"
        );
    }

    #[gpui::test]
    fn test_details_that_share_a_summary_toggle_apart(cx: &mut TestAppContext) {
        let source = "<details>
<summary>\u{984c}</summary>

\u{672c}\u{6587}A

</details>

<details>
<summary>\u{984c}</summary>

\u{672c}\u{6587}B

</details>";

        let second = render_html_preview(source, Some(("\u{984c}", 1)), cx);
        assert!(!second.contains("\u{672c}\u{6587}A"), "got: {second:?}");
        assert!(second.contains("\u{672c}\u{6587}B"), "got: {second:?}");

        let first = render_html_preview(source, Some(("\u{984c}", 0)), cx);
        assert!(first.contains("\u{672c}\u{6587}A"), "got: {first:?}");
        assert!(!first.contains("\u{672c}\u{6587}B"), "got: {first:?}");
    }

    #[gpui::test]
    fn test_details_inside_a_list_item_hides_its_body(cx: &mut TestAppContext) {
        let source = "- item\n\n  <details>\n  <summary>\u{984c}</summary>\n\n  \u{672c}\u{6587}\n\n  </details>\n\n\u{5f8c}\u{308d}";

        let collapsed = render_html_preview(source, None, cx);
        assert!(collapsed.contains('\u{984c}'), "got: {collapsed:?}");
        assert!(
            !collapsed.contains("\u{672c}\u{6587}"),
            "got: {collapsed:?}"
        );
        assert!(collapsed.contains("item"), "got: {collapsed:?}");
        assert!(collapsed.contains("\u{5f8c}\u{308d}"), "got: {collapsed:?}");

        let expanded = render_html_preview(source, Some(("\u{984c}", 0)), cx);
        assert!(expanded.contains("\u{672c}\u{6587}"), "got: {expanded:?}");
        assert!(expanded.contains("\u{5f8c}\u{308d}"), "got: {expanded:?}");
    }

    #[gpui::test]
    fn test_collapsed_details_in_a_list_keeps_the_formatting_around_it(cx: &mut TestAppContext) {
        // The disclosure's body is hidden by event index while the inline
        // formatting tags around it are rewritten in place, so a document that
        // has both is what catches the two passes disagreeing.
        let source = "a <b>\u{592a}\u{5b57}</b>\n\n- item\n\n  <details>\n  <summary>\u{984c}</summary>\n\n  \u{672c}\u{6587} <u>\u{4e0b}\u{7dda}</u>\n\n  </details>\n\n\u{5f8c}\u{308d} <i>\u{659c}\u{4f53}</i>";

        let collapsed = render_html_preview(source, None, cx);
        assert_eq!(
            collapsed,
            "a \u{592a}\u{5b57}\nitem\n\u{984c}\n\u{5f8c}\u{308d} \u{659c}\u{4f53}"
        );

        let expanded = render_html_preview(source, Some(("\u{984c}", 0)), cx);
        assert_eq!(
            expanded,
            "a \u{592a}\u{5b57}\nitem\n\u{984c}\n\u{672c}\u{6587} \u{4e0b}\u{7dda}\n\u{5f8c}\u{308d} \u{659c}\u{4f53}"
        );
    }

    #[gpui::test]
    fn test_details_inside_a_block_quote_hides_its_body(cx: &mut TestAppContext) {
        let source = "> <details>\n> <summary>\u{984c}</summary>\n>\n> \u{672c}\u{6587}\n>\n> </details>\n\n\u{5f8c}\u{308d}";

        let collapsed = render_html_preview(source, None, cx);
        assert!(collapsed.contains('\u{984c}'), "got: {collapsed:?}");
        assert!(
            !collapsed.contains("\u{672c}\u{6587}"),
            "got: {collapsed:?}"
        );
        assert!(collapsed.contains("\u{5f8c}\u{308d}"), "got: {collapsed:?}");

        let expanded = render_html_preview(source, Some(("\u{984c}", 0)), cx);
        assert!(expanded.contains("\u{672c}\u{6587}"), "got: {expanded:?}");
        assert!(expanded.contains("\u{5f8c}\u{308d}"), "got: {expanded:?}");
    }

    #[gpui::test]
    fn test_summary_renders_its_markdown(cx: &mut TestAppContext) {
        let rendered = render_html_preview(
            "<details><summary>**\u{592a}\u{5b57}** と `code`</summary>\u{672c}\u{6587}</details>",
            None,
            cx,
        );
        assert!(
            rendered.contains("\u{592a}\u{5b57} と code"),
            "got: {rendered:?}"
        );
        assert!(!rendered.contains('*'), "got: {rendered:?}");
        assert!(!rendered.contains('`'), "got: {rendered:?}");
    }

    #[gpui::test]
    fn test_summary_keeps_its_html_styling_inside_a_code_span(cx: &mut TestAppContext) {
        // The code span's backticks disappear while the `<b>` highlight was
        // measured against the text that still had them, so the moved highlight
        // has to land on a character boundary or laying the summary out panics.
        let rendered = render_html_preview(
            "<details><summary>`<b>\u{3042}</b>`</summary>\u{672c}\u{6587}</details>",
            None,
            cx,
        );
        assert!(rendered.contains('\u{3042}'), "got: {rendered:?}");
    }

    #[gpui::test]
    fn test_inline_formatting_tags_of_a_plain_paragraph_are_not_shown(cx: &mut TestAppContext) {
        let rendered = render_html_preview("a <b>bold</b> <u>under</u> <kbd>K</kbd>", None, cx);
        assert_eq!(rendered, "a bold under K");
    }

    #[gpui::test]
    fn test_sub_and_sup_of_a_plain_paragraph_use_unicode(cx: &mut TestAppContext) {
        let rendered = render_html_preview("H<sub>2</sub>O x<sup>2</sup>", None, cx);
        assert_eq!(rendered, "H\u{2082}O x\u{b2}");
    }

    /// Lays `source` out the way the preview does, HTML parsing on, and hands
    /// back what was laid out.
    fn render_html_preview_text(source: &str, cx: &mut TestAppContext) -> crate::RenderedText {
        struct PreviewTextView {
            markdown: Entity<Markdown>,
            rendered_text: Rc<RefCell<Option<crate::RenderedText>>>,
        }

        impl Render for PreviewTextView {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let rendered_text = self.rendered_text.clone();
                MarkdownElement::new(self.markdown.clone(), MarkdownStyle::default())
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

        let rendered_text = Rc::new(RefCell::new(None));
        let (_, cx) = cx.add_window_view({
            let rendered_text = rendered_text.clone();
            move |_, _| PreviewTextView {
                markdown,
                rendered_text,
            }
        });
        cx.run_until_parked();

        rendered_text
            .borrow()
            .clone()
            .expect("markdown should be rendered in the test view")
    }

    fn line_containing<'a>(
        rendered: &'a crate::RenderedText,
        needle: &str,
    ) -> &'a crate::RenderedLine {
        rendered
            .lines
            .iter()
            .find(|line| line.layout.text().contains(needle))
            .unwrap_or_else(|| panic!("no line contains {needle:?}"))
    }

    fn line_font_size(line: &crate::RenderedLine) -> Pixels {
        line.layout
            .line_layouts()
            .first()
            .expect("the line was laid out")
            .unwrapped_layout
            .font_size
    }

    /// The size the first glyph of `needle` in the line was drawn at.
    fn glyph_font_size(line: &crate::RenderedLine, needle: &str) -> Pixels {
        let mut offset = line
            .layout
            .text()
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} is not in the line"));
        // Each of the text's `\n`-separated lines is laid out on its own.
        for wrapped in line.layout.line_layouts() {
            let layout = &wrapped.unwrapped_layout;
            if offset > layout.len {
                offset -= layout.len + 1;
                continue;
            }
            let run = layout
                .runs
                .iter()
                .find(|run| run.glyphs.iter().any(|glyph| glyph.index == offset))
                .unwrap_or_else(|| panic!("no glyph of {needle:?} was laid out"));
            return layout.run_font_size(run);
        }
        panic!("{needle:?} is past the laid out lines");
    }

    #[gpui::test]
    fn test_font_sizes_on_part_of_a_paragraph_are_drawn_in_the_line(cx: &mut TestAppContext) {
        let source = "並\n\n<p>前<span style=\"font-size: 2em\">大</span>後</p>\n\n<h2>見<span style=\"font-size: 2em\">出</span>しA</h2>\n\n# 見<span style=\"font-size: 2em\">出</span>しB\n\n小<span style=\"font-size: 50%\">さ<span style=\"font-size: 200%\">戻</span></span>る\n\n改行前  \nの後<span style=\"font-size: 2em\">大</span>

深<span style=\"font-size: 400%\"><span style=\"font-size: 400%\"><span style=\"font-size: 400%\">い</span></span></span>入れ子

<p>深<span style=\"font-size: 400%\"><span style=\"font-size: 400%\">い</span></span>HTML</p>";
        let rendered = render_html_preview_text(source, cx);
        let normal = line_font_size(line_containing(&rendered, "並"));

        let html = line_containing(&rendered, "前大後");
        assert_eq!(glyph_font_size(html, "前"), normal);
        assert_eq!(glyph_font_size(html, "大"), normal * 2.);
        assert_eq!(glyph_font_size(html, "後"), normal);

        // A heading keeps the size of its level.
        for heading in ["見出しA", "見出しB"] {
            let line = line_containing(&rendered, heading);
            assert_eq!(
                glyph_font_size(line, "出"),
                glyph_font_size(line, "見"),
                "{heading}"
            );
        }

        // Sizes inside sizes are relative to the text around them.
        let nested = line_containing(&rendered, "小さ戻る");
        assert_eq!(glyph_font_size(nested, "小"), normal);
        assert_eq!(glyph_font_size(nested, "さ"), normal * 0.5);
        assert_eq!(glyph_font_size(nested, "戻"), normal);
        assert_eq!(glyph_font_size(nested, "る"), normal);

        // A hard break starts another line of the same text.
        let broken = line_containing(&rendered, "改行前");
        assert_eq!(glyph_font_size(broken, "改行前"), normal);
        assert_eq!(glyph_font_size(broken, "の後"), normal);
        assert_eq!(glyph_font_size(broken, "大"), normal * 2.);

        // Sizes inside sizes stop at the largest size a single one may ask for.
        for text in ["深い入れ子", "深いHTML"] {
            let deep = line_containing(&rendered, text);
            assert_eq!(glyph_font_size(deep, "い"), normal * 4., "{text}");
        }
    }

    #[gpui::test]
    fn test_a_paragraph_wrapped_in_a_font_size_span_is_laid_out_larger(cx: &mut TestAppContext) {
        let source = "並の段落\n\n<span style=\"font-size: 1.5em\">大きい段落</span>\n\n一部だけ<span style=\"font-size: 1.5em\">大</span>きい\n\n<p style=\"font-size: 1.5em\">HTML の段落</p>";
        let rendered = render_html_preview_text(source, cx);

        let normal = line_containing(&rendered, "並の段落");
        let large = line_containing(&rendered, "大きい段落");
        let partial = line_containing(&rendered, "一部だけ");
        let html = line_containing(&rendered, "HTML の段落");

        assert!(
            line_font_size(large) > line_font_size(normal) * 1.4,
            "{:?} against {:?}",
            line_font_size(large),
            line_font_size(normal)
        );
        assert!(large.layout.line_height() > normal.layout.line_height() * 1.4);
        assert!(line_font_size(html) > line_font_size(normal) * 1.4);
        // Part of a paragraph is drawn at its size within the paragraph's
        // lines, which grow tall enough for it.
        assert_eq!(line_font_size(partial), line_font_size(normal));
        assert_eq!(glyph_font_size(partial, "一部"), line_font_size(normal));
        assert_eq!(glyph_font_size(partial, "きい"), line_font_size(normal));
        assert!(glyph_font_size(partial, "大") > line_font_size(normal) * 1.4);
        assert!(partial.layout.line_height() > normal.layout.line_height() * 1.4);

        // The tags are not drawn, and the text still maps back to its source,
        // which is what copying and clicking through to the editor rely on.
        assert_eq!(large.layout.text(), "大きい段落");
        assert_eq!(partial.layout.text(), "一部だけ大きい");
        let start = source.find("大きい段落").expect("the source has the text");
        assert_eq!(
            rendered.text_for_range(start..start + "大きい段落".len()),
            "大きい段落"
        );
    }

    #[gpui::test]
    fn test_font_sizes_inside_a_sized_paragraph_stop_at_the_largest(cx: &mut TestAppContext) {
        let source = "並

<span style=\"font-size: 400%\"><span style=\"font-size: 400%\"><span style=\"font-size: 400%\">三重</span></span></span>

<span style=\"font-size: 400%\">段落と<span style=\"font-size: 400%\">一部</span></span>

<p style=\"font-size: 400%\">HTML と<span style=\"font-size: 400%\">内側</span></p>

<span style=\"font-size: 200%\">倍と<span style=\"font-size: 150%\">更に</span></span>";
        let rendered = render_html_preview_text(source, cx);
        let normal = line_font_size(line_containing(&rendered, "並"));

        let nested = line_containing(&rendered, "三重");
        assert_eq!(glyph_font_size(nested, "三重"), normal * 4.);
        for (line, part) in [("段落と一部", "一部"), ("HTML と内側", "内側")] {
            let line = line_containing(&rendered, line);
            assert_eq!(line_font_size(line), normal * 4.);
            assert_eq!(glyph_font_size(line, part), normal * 4., "{part}");
        }
        // Below the limit, a size inside a sized paragraph is still relative
        // to the paragraph's.
        let moderate = line_containing(&rendered, "倍と更に");
        assert_eq!(glyph_font_size(moderate, "倍と"), normal * 2.);
        assert_eq!(glyph_font_size(moderate, "更に"), normal * 3.);
    }

    /// The run the first occurrence of `needle` in the line was drawn with.
    fn run_at<'a>(line: &'a crate::RenderedLine, needle: &str) -> &'a gpui::TextRun {
        let text = line.layout.text();
        let offset = text
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} is not in {text:?}"));
        let mut run_end = 0;
        for run in &line.runs {
            run_end += run.len;
            if offset < run_end {
                return run;
            }
        }
        panic!("no run covers {needle:?} in {text:?}");
    }

    #[gpui::test]
    fn test_colored_text_is_drawn_in_its_color(cx: &mut TestAppContext) {
        let source = "前 <span style=\"color: #d44c47\">赤 **太字**</span> <mark>印</mark> 後\n\n\
            <p><span style=\"color: #337ea9\">青</span> <mark style=\"background-color: #b7ebc6\">緑</mark></p>";
        let rendered = render_html_preview_text(source, cx);
        let red: gpui::Hsla = gpui::rgb(0xd44c47).into();
        let black: gpui::Hsla = gpui::rgb(0x000000).into();

        let line = line_containing(&rendered, "前");
        assert_eq!(line.layout.text(), "前 赤 太字 印 後");
        assert_eq!(run_at(line, "赤").color, red);
        // Bold text is normally drawn brighter, but not over the span's color.
        assert_eq!(run_at(line, "太字").color, red);
        assert_eq!(run_at(line, "太字").font.weight, gpui::FontWeight::BOLD);
        assert_eq!(
            run_at(line, "印").background_color,
            Some(gpui::rgb(0xffff00).into())
        );
        assert_eq!(run_at(line, "印").color, black);
        assert_ne!(run_at(line, "後").color, red);
        assert_eq!(run_at(line, "後").background_color, None);

        let html = line_containing(&rendered, "青");
        assert_eq!(run_at(html, "青").color, gpui::rgb(0x337ea9).into());
        assert_eq!(
            run_at(html, "緑").background_color,
            Some(gpui::rgb(0xb7ebc6).into())
        );
        assert_eq!(run_at(html, "緑").color, black);

        // The colored text still maps back to its source.
        let start = source.find('赤').expect("the source has the text");
        assert_eq!(
            rendered.text_for_range(start..start + '赤'.len_utf8()),
            "赤"
        );
    }

    #[test]
    fn test_span_and_html_colors_become_the_text_style() {
        let refinement = crate::inline_span_text_style(&crate::parser::InlineSpanStyle {
            color: Some(0xd44c47),
            background: Some(0xffff00),
            ..Default::default()
        });
        assert_eq!(refinement.color, Some(gpui::rgb(0xd44c47).into()));
        assert_eq!(
            refinement.background_color,
            Some(gpui::rgb(0xffff00).into())
        );

        let mut refinement = gpui::TextStyleRefinement::default();
        super::apply_html_highlight_style(
            &mut refinement,
            &crate::html::html_parser::HtmlHighlightStyle {
                color: Some(0x337ea9),
                background_color: Some(0xb7ebc6),
                ..Default::default()
            },
        );
        assert_eq!(refinement.color, Some(gpui::rgb(0x337ea9).into()));
        assert_eq!(
            refinement.background_color,
            Some(gpui::rgb(0xb7ebc6).into())
        );

        // A link inside the colored span takes the link color back, while the
        // marker behind it stays.
        super::apply_html_highlight_style(
            &mut refinement,
            &crate::html::html_parser::HtmlHighlightStyle {
                link: true,
                ..Default::default()
            },
        );
        assert_eq!(refinement.color, None);
        assert_eq!(
            refinement.background_color,
            Some(gpui::rgb(0xb7ebc6).into())
        );
    }

    #[gpui::test]
    fn test_html_table_scrolls_instead_of_wrapping_its_cells(cx: &mut TestAppContext) {
        ensure_theme_initialized(cx);

        struct TableView {
            markdown: Entity<Markdown>,
            style: MarkdownStyle,
        }

        impl Render for TableView {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().size_full().child(MarkdownElement::new(
                    self.markdown.clone(),
                    self.style.clone(),
                ))
            }
        }

        const TABLE: &str = "<table><thead><tr><th>\u{9805}\u{76ee}</th><th>\u{8aac}\u{660e}</th></tr></thead>\
<tbody><tr><td>\u{6700}\u{521d}\u{306e}\u{9805}\u{76ee}</td>\
<td>\u{3053}\u{306e}\u{5217}\u{306f}\u{9577}\u{3044}\u{6587}\u{7ae0}\u{3092}\u{6301}\u{3061}\u{307e}\u{3059}</td></tr>\
<tr><td>\u{6b21}\u{306e}\u{9805}\u{76ee}</td><td>\u{3082}\u{3046}\u{4e00}\u{3064}\u{306e}\u{9577}\u{3044}\u{8aac}\u{660e}\u{6587}</td></tr></tbody></table>";

        let table_bounds = |table_overflow_x_scroll: bool, cx: &mut TestAppContext| {
            let mut style = MarkdownStyle::default();
            style.table_overflow_x_scroll = table_overflow_x_scroll;
            let window = cx.open_window(size(px(200.), px(600.)), |_, cx| {
                let markdown = cx.new(|cx| {
                    Markdown::new_with_options(
                        TABLE.into(),
                        None,
                        None,
                        MarkdownOptions {
                            parse_html: true,
                            ..Default::default()
                        },
                        cx,
                    )
                });
                TableView { markdown, style }
            });
            cx.run_until_parked();
            gpui::VisualTestContext::from_window(window.into(), cx)
                .debug_bounds("markdown_html_table")
                .expect("the html table should have been rendered")
        };

        let wrapped = table_bounds(false, cx);
        let scrolling = table_bounds(true, cx);

        // Either way the table stays inside the pane, so a wide table cannot blow
        // out the layout around it.
        assert_eq!(wrapped.size.width, px(200.));
        assert_eq!(scrolling.size.width, px(200.));

        // With the floors in place each column keeps its own width and the rest
        // goes to the horizontal scrollbar, instead of wrapping cell text a few
        // characters at a time.
        assert!(
            scrolling.size.height < wrapped.size.height,
            "the floors should keep the rows compact; wrapped {:?}, scrolling {:?}",
            wrapped.size.height,
            scrolling.size.height
        );
    }

    fn parsed_table(html: &str) -> ParsedHtmlTable {
        let parsed = crate::html::html_parser::parse_html_block(html, 0..html.len())
            .expect("the block should parse");
        match parsed.children.into_iter().next() {
            Some(ParsedHtmlElement::Table(table)) => table,
            other => panic!("expected a table, got {other:?}"),
        }
    }

    #[test]
    fn a_row_span_pushes_the_cells_below_it_to_the_right() {
        let table = parsed_table(concat!(
            "<table><tbody>",
            "<tr><td rowspan=\"2\">a</td></tr>",
            "<tr><td>b</td><td>c</td></tr>",
            "</tbody></table>",
        ));
        let grid = HtmlTableGrid::of(&table);

        assert_eq!(
            grid.column_count(),
            3,
            "`a` keeps column 0 of the second row, so `b` and `c` sit beside it"
        );
        assert_eq!(grid.cell_column(0, 0), Some(0));
        assert_eq!(grid.cell_column(1, 0), Some(1));
        assert_eq!(grid.cell_column(1, 1), Some(2));
        assert!(
            grid.is_occupied(1, 0),
            "the span reaches into the second row"
        );
        assert!(!grid.is_occupied(2, 0), "and no further");
    }

    #[test]
    fn a_row_span_in_the_header_reaches_into_the_body() {
        let table = parsed_table(concat!(
            "<table>",
            "<thead><tr><th rowspan=\"2\">h</th><th>i</th></tr></thead>",
            "<tbody><tr><td>b</td></tr></tbody>",
            "</table>",
        ));
        let grid = HtmlTableGrid::of(&table);

        assert_eq!(grid.column_count(), 2);
        assert_eq!(
            grid.cell_column(1, 0),
            Some(1),
            "the body row starts under the header cell the span leaves free"
        );
    }

    #[test]
    fn a_column_floor_is_measured_against_the_column_the_cell_lands_in() {
        let table = parsed_table(concat!(
            "<table><tbody>",
            "<tr><td rowspan=\"2\">R</td><td>短</td></tr>",
            "<tr><td>とても長い見出しの文字列</td></tr>",
            "</tbody></table>",
        ));
        let grid = HtmlTableGrid::of(&table);
        let widths = html_table_column_min_widths(&table, &grid, px(14.), px(16.));

        let (Some(first), Some(second)) = (widths.first(), widths.get(1)) else {
            panic!("the table has two columns, got {widths:?}");
        };
        assert!(
            second > first,
            "the long cell of the second row belongs to column 1, got {widths:?}"
        );
    }

    #[test]
    fn an_absurd_span_cannot_ask_for_a_grid_that_does_not_fit_in_memory() {
        for span in ["99999", "4294967296", "18446744073709551615"] {
            let table = parsed_table(&format!(
                "<table><tbody><tr><td colspan=\"{span}\">a</td></tr><tr><td>b</td></tr></tbody></table>",
            ));
            let grid = HtmlTableGrid::of(&table);

            assert!(
                grid.column_count() <= MAX_TABLE_COLUMNS,
                "colspan=\"{span}\" gave {} columns",
                grid.column_count()
            );
            assert_eq!(grid.cell_column(1, 0), Some(0));
        }

        let table = parsed_table(concat!(
            "<table><tbody>",
            "<tr><td rowspan=\"18446744073709551615\">a</td><td>b</td></tr>",
            "<tr><td>c</td></tr>",
            "</tbody></table>",
        ));
        let grid = HtmlTableGrid::of(&table);
        assert_eq!(
            grid.cell_column(1, 0),
            Some(1),
            "a rowspan past the last row claims nothing below the table"
        );
    }

    #[test]
    fn a_span_that_is_not_a_positive_number_counts_as_one() {
        // HTML gives `rowspan="0"` the rest of the rows, but treating it as one
        // is what the parser already does, and a cell that occupies nothing at
        // all would leave the renderer drawing a filler on top of it.
        let table = parsed_table(concat!(
            "<table><tbody>",
            "<tr><td colspan=\"0\">a</td><td rowspan=\"0\">b</td></tr>",
            "<tr><td colspan=\"-3\">c</td><td colspan=\"x\">d</td></tr>",
            "</tbody></table>",
        ));
        let grid = HtmlTableGrid::of(&table);

        assert_eq!(grid.column_count(), 2);
        assert_eq!(grid.cell_column(0, 0), Some(0));
        assert_eq!(grid.cell_column(0, 1), Some(1));
        assert_eq!(grid.cell_column(1, 0), Some(0));
        assert_eq!(grid.cell_column(1, 1), Some(1));
        for row_index in 0..2 {
            for column_index in 0..2 {
                assert!(
                    grid.is_occupied(row_index, column_index),
                    "({row_index}, {column_index}) has a cell in it"
                );
            }
        }
    }

    #[test]
    fn a_row_span_and_a_column_span_on_the_same_cell_claim_a_rectangle() {
        let table = parsed_table(concat!(
            "<table><tbody>",
            "<tr><td rowspan=\"2\" colspan=\"2\">a</td><td>b</td></tr>",
            "<tr><td>c</td></tr>",
            "<tr><td>d</td><td>e</td><td>f</td></tr>",
            "</tbody></table>",
        ));
        let grid = HtmlTableGrid::of(&table);

        assert_eq!(grid.column_count(), 3);
        assert_eq!(grid.cell_column(0, 1), Some(2));
        assert_eq!(grid.cell_column(1, 0), Some(2), "`c` clears the rectangle");
        assert_eq!(grid.cell_column(2, 0), Some(0), "which ends after two rows");
        assert!(grid.is_occupied(1, 1));
        assert_eq!(grid.cell_column(2, 2), Some(2));
    }

    #[test]
    fn a_header_less_table_and_an_empty_row_still_have_a_grid() {
        // No `<thead>`, a header row spelled with `<th>` inside the body, and an
        // empty `<tr>` the parser drops on the way in.
        let table = parsed_table(concat!(
            "<table><tbody>",
            "<tr><th rowspan=\"2\">h</th><th>i</th></tr>",
            "<tr></tr>",
            "<tr><td>a</td></tr>",
            "</tbody></table>",
        ));
        let grid = HtmlTableGrid::of(&table);

        assert_eq!(grid.column_count(), 2);
        assert_eq!(
            grid.cell_column(1, 0),
            Some(1),
            "the row under the header span starts beside it"
        );
        assert!(grid.cell_column(1, 1).is_none(), "and has nothing else");
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

        let first_expanded = render_html_preview(source, Some(("A", 0)), cx);
        assert!(first_expanded.contains("本文A"), "got: {first_expanded:?}");
        assert!(!first_expanded.contains("本文B"), "got: {first_expanded:?}");
        assert!(first_expanded.contains('B'), "got: {first_expanded:?}");
    }

    /// Lays `source` out the way the preview does, root block markers and
    /// HTML parsing on, in a pane `width` wide, and hands back what was laid
    /// out along with where the table spanning blocks ended up.
    fn render_spanning_table_preview(
        source: &str,
        width: Pixels,
        cx: &mut TestAppContext,
    ) -> (crate::RenderedText, Option<gpui::Bounds<Pixels>>) {
        render_table_preview(source, width, "markdown_html_spanning_table", cx)
    }

    /// Like [`render_spanning_table_preview`], for the table drawn with the
    /// debug selector `table_selector`.
    fn render_table_preview(
        source: &str,
        width: Pixels,
        table_selector: &'static str,
        cx: &mut TestAppContext,
    ) -> (crate::RenderedText, Option<gpui::Bounds<Pixels>>) {
        struct SpanningTableView {
            markdown: Entity<Markdown>,
            rendered_text: Rc<RefCell<Option<crate::RenderedText>>>,
        }

        impl Render for SpanningTableView {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let rendered_text = self.rendered_text.clone();
                div().size_full().child(
                    MarkdownElement::new(self.markdown.clone(), MarkdownStyle::default())
                        .show_root_block_markers()
                        .on_render(move |text| *rendered_text.borrow_mut() = Some(text)),
                )
            }
        }

        ensure_theme_initialized(cx);
        let rendered_text = Rc::new(RefCell::new(None));
        let window = cx.open_window(size(width, px(1200.)), {
            let source = source.to_string();
            let rendered_text = rendered_text.clone();
            move |_, cx| {
                let markdown = cx.new(|cx| {
                    Markdown::new_with_options(
                        source.into(),
                        None,
                        None,
                        MarkdownOptions {
                            parse_html: true,
                            ..Default::default()
                        },
                        cx,
                    )
                });
                SpanningTableView {
                    markdown,
                    rendered_text,
                }
            }
        });
        cx.run_until_parked();

        let table_bounds =
            gpui::VisualTestContext::from_window(window.into(), cx).debug_bounds(table_selector);
        let rendered_text = rendered_text
            .borrow()
            .clone()
            .expect("markdown should be rendered in the test view");
        (rendered_text, table_bounds)
    }

    fn text_bounds(
        rendered: &crate::RenderedText,
        source: &str,
        needle: &str,
    ) -> gpui::Bounds<Pixels> {
        let start = source.find(needle).expect("the text is in the source");
        rendered
            .bounds_for_source_range(start..start + needle.len())
            .first()
            .copied()
            .unwrap_or_else(|| panic!("{needle:?} should have been laid out"))
    }

    /// The README layout: each cell's markdown, written between the table's
    /// blocks, is drawn inside the table, the cells side by side.
    #[gpui::test]
    fn test_markdown_between_table_blocks_is_drawn_in_its_cell(cx: &mut TestAppContext) {
        let source = "<table>\n<tr>\n<td>\n\n**左の列** の本文\n\n- 項目\n\n</td>\n<td>\n\n右の列\n\n</td>\n</tr>\n</table>\n\n後ろ\n";
        let (rendered, table) = render_spanning_table_preview(source, px(600.), cx);
        let table = table.expect("the table should be drawn as one");

        let left = text_bounds(&rendered, source, "左の列");
        let item = text_bounds(&rendered, source, "項目");
        let right = text_bounds(&rendered, source, "右の列");
        let after = text_bounds(&rendered, source, "後ろ");

        for (name, bounds) in [("左の列", left), ("項目", item), ("右の列", right)] {
            assert!(
                table.contains(&bounds.center()),
                "{name} at {bounds:?} should be inside the table at {table:?}"
            );
        }
        assert!(
            right.left() > item.right() && right.left() > left.left(),
            "the second cell should be to the right of the first: {left:?} {item:?} {right:?}"
        );
        assert!(
            right.top() < item.bottom(),
            "the cells should share a row: {right:?} {item:?}"
        );
        assert!(
            after.top() >= table.bottom(),
            "what follows the table should be drawn after it: {after:?} {table:?}"
        );

        // Copying keeps the cells' text in the order it was written.
        let copied = rendered.text_for_range(0..source.len());
        let positions = ["左の列", "項目", "右の列", "後ろ"].map(|needle| {
            copied
                .find(needle)
                .unwrap_or_else(|| panic!("{needle:?} should be copied: {copied:?}"))
        });
        assert!(positions.is_sorted(), "copied: {copied:?}");
    }

    #[gpui::test]
    fn test_rows_of_a_spanning_table_stack_and_align(cx: &mut TestAppContext) {
        let source = "<table>\n<tr>\n<th>\n\n見出し\n\n</th>\n<td align=\"right\">\n\n右\n\n</td>\n</tr>\n<tr>\n<td colspan=\"2\" align=\"center\">\n\n下の段\n\n</td>\n</tr>\n<tr>\n<td>\n\n片側\n\n</td>\n</tr>\n<tr>\n<td>\n\n次の行\n\n</td>\n<td>\n\n次の右\n\n</td>\n</tr>\n</table>\n";
        let (rendered, table) = render_spanning_table_preview(source, px(600.), cx);
        let table = table.expect("the table should be drawn as one");

        let heading = text_bounds(&rendered, source, "見出し");
        let right = text_bounds(&rendered, source, "右");
        let below = text_bounds(&rendered, source, "下の段");
        assert!(table.contains(&below.center()), "{below:?} {table:?}");
        assert!(
            below.top() >= heading.bottom() && below.top() >= right.bottom(),
            "the second row should be under the first: {heading:?} {right:?} {below:?}"
        );

        // A row with a column left over still takes the whole row, so the
        // next row does not move up into its empty column.
        let one_sided = text_bounds(&rendered, source, "片側");
        let next_row = text_bounds(&rendered, source, "次の行");
        assert!(
            next_row.top() >= one_sided.bottom(),
            "the next row should start under the short one: {one_sided:?} {next_row:?}"
        );

        let line_align = |needle: &str| {
            rendered
                .lines
                .iter()
                .find(|line| line.layout.wrapped_text().contains(needle))
                .map(|line| line.text_align)
                .unwrap_or_else(|| panic!("{needle:?} should be laid out"))
        };
        assert_eq!(line_align("見出し"), gpui::TextAlign::Center);
        assert_eq!(line_align("右"), gpui::TextAlign::Right);
        assert_eq!(line_align("下の段"), gpui::TextAlign::Center);
    }

    /// The cell's markdown lands inside the box that places it vertically,
    /// not beside it.
    #[gpui::test]
    fn test_cell_vertical_alignment_reaches_its_markdown(cx: &mut TestAppContext) {
        let source = "<table>\n<tr>\n<td valign=\"top\">\n\n上\n\n</td>\n<td valign=\"bottom\">\n\n下\n\n</td>\n<td>\n\n一\n\n二\n\n三\n\n</td>\n</tr>\n</table>\n";
        let (rendered, _) = render_spanning_table_preview(source, px(600.), cx);
        let top = text_bounds(&rendered, source, "上");
        let bottom = text_bounds(&rendered, source, "下");
        let first = text_bounds(&rendered, source, "一");
        let second = text_bounds(&rendered, source, "二");
        assert!(
            bottom.top() > second.top() && bottom.top() > first.bottom(),
            "a bottom-aligned cell should sit at the foot of its row: {bottom:?} {first:?} {second:?}"
        );
        assert!(
            top.top() < second.top(),
            "a top-aligned cell should sit at the head of its row: {top:?} {second:?}"
        );
    }

    #[gpui::test]
    fn test_an_aligned_div_centers_a_spanning_table(cx: &mut TestAppContext) {
        let source = "<div align=\"center\">\n\n<table>\n<tr>\n<td>\n\n中\n\n</td>\n</tr>\n</table>\n\n</div>\n";
        let (_, table) = render_spanning_table_preview(source, px(600.), cx);
        let table = table.expect("the table should be drawn as one");
        assert!(
            table.left() > px(200.) && table.right() < px(400.),
            "a small table in a centered div should sit in the middle: {table:?}"
        );
    }

    #[gpui::test]
    fn test_table_that_does_not_pair_keeps_its_markdown(cx: &mut TestAppContext) {
        let source = "<table>\n<tr>\n<td>\n\n本文\n\n後ろ\n";
        let (rendered, table) = render_spanning_table_preview(source, px(600.), cx);
        assert_eq!(table, None);
        text_bounds(&rendered, source, "本文");
        text_bounds(&rendered, source, "後ろ");
    }

    #[gpui::test]
    fn test_collapsed_details_in_a_table_cell_keeps_the_table_whole(cx: &mut TestAppContext) {
        for source in [
            "<table><tr><td>\n\n<details><summary>もっと</summary>\n\n隠れる\n\n</details>\n\n</td><td>\n\n右\n\n</td></tr></table>\n\n後ろ\n",
            "<table><tr><td><details><summary>もっと</summary>\n\n隠れる\n\n</details>\n\n</td><td>\n\n右\n\n</td></tr></table>\n\n後ろ\n",
            // The disclosure closes in the block that goes on to the next cell.
            "<table><tr><td><details><summary>もっと</summary>\n\n隠れる\n\n</details></td><td>\n\n右\n\n</td></tr></table>\n\n後ろ\n",
        ] {
            let collapsed = render_html_preview(source, None, cx);
            for needle in ["もっと", "右", "後ろ"] {
                assert!(collapsed.contains(needle), "{needle:?} in {collapsed:?}");
            }
            assert!(!collapsed.contains("隠れる"), "got: {collapsed:?}");

            let expanded = render_html_preview(source, Some(("もっと", 0)), cx);
            assert!(expanded.contains("隠れる"), "got: {expanded:?}");

            let (rendered, table) = render_spanning_table_preview(source, px(600.), cx);
            let table = table.expect("the table should be drawn as one");
            let right = text_bounds(&rendered, source, "右");
            let after = text_bounds(&rendered, source, "後ろ");
            assert!(table.contains(&right.center()), "{right:?} {table:?}");
            assert!(after.top() >= table.bottom(), "{after:?} {table:?}");
        }
    }

    /// A list item's text box asks for no width of its own, which in a column
    /// sized to its content would wrap the text after every character.
    #[gpui::test]
    fn test_lists_in_a_spanning_cell_keep_their_width(cx: &mut TestAppContext) {
        let long_item =
            "これは長い項目の文章で、セルの幅には収まらないので折り返されます。".repeat(3);
        let source = format!(
            "<table>\n<tr>\n<td>\n\n- 項目\n- [ ] 作業\n- {long_item}\n\n<ul><li>HTMLの項目</li></ul>\n\n脚注[^1]\n\n[^1]: 脚注の本文\n\n</td>\n<td>\n\n右の列\n\n</td>\n</tr>\n</table>\n"
        );
        let (rendered, table) = render_spanning_table_preview(&source, px(600.), cx);
        let table = table.expect("the table should be drawn as one");

        let wrapped = |needle: &str| {
            rendered
                .lines
                .iter()
                .map(|line| line.layout.wrapped_text())
                .find(|text| text.replace('\n', "").contains(needle))
                .unwrap_or_else(|| panic!("{needle:?} should be laid out"))
        };
        for needle in ["項目", "作業", "HTMLの項目", "脚注の本文"] {
            assert_eq!(wrapped(needle), needle, "{needle:?} should fit on one line");
        }
        let long = text_bounds(&rendered, &source, &long_item);
        assert!(
            wrapped(&long_item).contains('\n') && long.right() <= table.right(),
            "a long item should wrap inside the table: {long:?} {table:?}"
        );
    }

    /// Where the line holding `needle` was laid out. Unlike [`text_bounds`] it
    /// does not go through the source, which a table in one block hands out
    /// to its text in drawing order rather than where each text is written.
    fn line_bounds(rendered: &crate::RenderedText, needle: &str) -> gpui::Bounds<Pixels> {
        rendered
            .lines
            .iter()
            .find(|line| line.layout.wrapped_text().contains(needle))
            .map(|line| line.layout.bounds())
            .unwrap_or_else(|| panic!("{needle:?} should have been laid out"))
    }

    const IN_ONE_BLOCK: &str = "markdown_html_table";
    const SPANNING_BLOCKS: &str = "markdown_html_spanning_table";

    #[gpui::test]
    fn test_row_spans_place_the_cells_beside_them(cx: &mut TestAppContext) {
        for (source, selector) in [
            (
                "<table><tr><td rowspan=\"2\">結合した列</td><td>右上</td></tr><tr><td>右下</td></tr><tr><td>左下</td><td>最後</td></tr></table>\n",
                IN_ONE_BLOCK,
            ),
            (
                "<table>\n<tr>\n<td rowspan=\"2\">\n\n結合した列\n\n</td>\n<td>\n\n右上\n\n</td>\n</tr>\n<tr>\n<td>\n\n右下\n\n</td>\n</tr>\n<tr>\n<td>\n\n左下\n\n</td>\n<td>\n\n最後\n\n</td>\n</tr>\n</table>\n",
                SPANNING_BLOCKS,
            ),
        ] {
            let (rendered, table) = render_table_preview(source, px(600.), selector, cx);
            let table = table.unwrap_or_else(|| panic!("the table should be drawn: {source:?}"));
            let joined = line_bounds(&rendered, "結合した列");
            let upper = line_bounds(&rendered, "右上");
            let lower = line_bounds(&rendered, "右下");
            let left_below = line_bounds(&rendered, "左下");
            let last = line_bounds(&rendered, "最後");
            for bounds in [joined, upper, lower, left_below, last] {
                assert!(table.contains(&bounds.center()), "{bounds:?} {table:?}");
            }
            assert!(
                (lower.left() - upper.left()).abs() < px(1.) && lower.top() >= upper.bottom(),
                "the cell under the joined one's row goes in the next column: {upper:?} {lower:?} ({source:?})"
            );
            assert!(
                lower.left() > joined.right(),
                "{joined:?} {lower:?} ({source:?})"
            );
            assert!(
                left_below.left() < upper.left()
                    && left_below.top() >= lower.bottom()
                    && (last.left() - upper.left()).abs() < px(1.),
                "the row after the span starts at the left again: {left_below:?} {last:?} ({source:?})"
            );
        }
    }

    #[gpui::test]
    fn test_percentage_widths_take_their_share_of_the_table(cx: &mut TestAppContext) {
        for (source, selector) in [
            (
                "<table><tr><td width=\"75%\">左</td><td>右</td></tr></table>\n",
                IN_ONE_BLOCK,
            ),
            (
                "<table>\n<tr>\n<td style=\"width: 75%\">\n\n左\n\n</td>\n<td>\n\n右\n\n</td>\n</tr>\n</table>\n",
                SPANNING_BLOCKS,
            ),
        ] {
            let (rendered, table) = render_table_preview(source, px(600.), selector, cx);
            let table = table.unwrap_or_else(|| panic!("the table should be drawn: {source:?}"));
            assert!(
                table.size.width > px(500.),
                "a table with a percentage takes the width it is a part of: {table:?} ({source:?})"
            );
            let right = line_bounds(&rendered, "右");
            let share = (right.left() - table.left()) / table.size.width;
            assert!(
                (0.72..0.8).contains(&share),
                "the second column starts three quarters across: {share} ({source:?})"
            );
        }
    }

    #[gpui::test]
    fn test_pixel_widths_keep_their_columns_open(cx: &mut TestAppContext) {
        let source = "<table><tr><td width=\"300\">左</td><td>右</td></tr></table>\n";
        let (rendered, table) = render_table_preview(source, px(600.), IN_ONE_BLOCK, cx);
        let table = table.expect("the table should be drawn");
        let right = line_bounds(&rendered, "右");
        assert!(
            right.left() - table.left() >= px(300.),
            "{right:?} {table:?}"
        );
        assert!(
            table.size.width < px(400.),
            "without a percentage the table stays as wide as its columns: {table:?}"
        );
    }

    #[gpui::test]
    fn test_a_caption_is_centered_over_or_under_its_table(cx: &mut TestAppContext) {
        for (source, selector, below) in [
            (
                "<table><caption>表の題</caption><tr><td>とても長いセルの本文です</td></tr></table>\n",
                IN_ONE_BLOCK,
                false,
            ),
            (
                "<table><caption style=\"caption-side: bottom\">表の題</caption><tr><td>とても長いセルの本文です</td></tr></table>\n",
                IN_ONE_BLOCK,
                true,
            ),
            (
                "<table>\n<caption>表の題</caption>\n<tr>\n<td>\n\nとても長いセルの本文です\n\n</td>\n</tr>\n</table>\n\n後ろ\n",
                SPANNING_BLOCKS,
                false,
            ),
            (
                "<table>\n<caption align=\"bottom\">表の題</caption>\n<tr>\n<td>\n\nとても長いセルの本文です\n\n</td>\n</tr>\n</table>\n\n後ろ\n",
                SPANNING_BLOCKS,
                true,
            ),
        ] {
            let (rendered, table) = render_table_preview(source, px(600.), selector, cx);
            let table = table.unwrap_or_else(|| panic!("the table should be drawn: {source:?}"));
            let caption = line_bounds(&rendered, "表の題");
            assert!(
                (caption.center().x - table.center().x).abs() < px(2.),
                "the caption is centered on the table: {caption:?} {table:?} ({source:?})"
            );
            if below {
                assert!(caption.top() >= table.bottom(), "{caption:?} {table:?}");
            } else {
                assert!(caption.bottom() <= table.top(), "{caption:?} {table:?}");
            }
            if selector == SPANNING_BLOCKS {
                let after = line_bounds(&rendered, "後ろ");
                assert!(
                    after.top() >= table.bottom().max(caption.bottom()),
                    "{after:?} {table:?} {caption:?}"
                );
            }
        }
    }
}
